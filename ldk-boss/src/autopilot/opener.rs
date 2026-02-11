use crate::autopilot::candidate::Candidate;
use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use ldk_server_protos::api::{ConnectPeerRequest, OpenChannelRequest};
use log::{error, info, warn};

/// A planned channel open.
pub struct PlannedOpen {
    pub candidate: Candidate,
    pub amount_sats: u64,
}

/// Plan how to distribute the budget across candidates.
///
/// Mimics CLBoss Planner logic:
/// - If few existing channels, open multiple to build connectivity.
/// - If enough channels, open only 1 at a time.
/// - Respect min/max channel size limits.
pub fn plan_opens(
    config: &Config,
    candidates: &[Candidate],
    budget_sats: u64,
    max_proposals: usize,
) -> Vec<PlannedOpen> {
    let mut plan = Vec::new();
    let mut remaining = budget_sats;

    let num_to_open = max_proposals.min(candidates.len());

    for i in 0..num_to_open {
        if remaining < config.autopilot.min_channel_sats {
            break;
        }

        // Skip candidates without addresses (earnings-based may lack address)
        if candidates[i].address.is_empty() {
            continue;
        }

        // Divide remaining evenly among remaining slots, but respect limits
        let slots_left = (num_to_open - i) as u64;
        let per_channel = remaining / slots_left.max(1);
        let amount = per_channel
            .max(config.autopilot.min_channel_sats)
            .min(config.autopilot.max_channel_sats)
            .min(remaining);

        // Hard safety limit: no single channel > 50% of total budget
        let amount = amount.min(budget_sats / 2).max(config.autopilot.min_channel_sats);

        if amount < config.autopilot.min_channel_sats {
            break;
        }

        plan.push(PlannedOpen {
            candidate: candidates[i].clone(),
            amount_sats: amount,
        });

        remaining = remaining.saturating_sub(amount);
    }

    plan
}

/// Execute a planned channel open: connect to peer, then open channel.
pub async fn execute_open(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    open: &PlannedOpen,
) -> anyhow::Result<()> {
    info!(
        "Autopilot: opening {} sat channel with {} ({})",
        open.amount_sats,
        open.candidate.node_id,
        open.candidate.address,
    );

    if config.general.dry_run {
        info!("  (dry-run: not executing)");
        return Ok(());
    }

    // Step 1: Connect to peer
    let connect_req = ConnectPeerRequest {
        node_pubkey: open.candidate.node_id.clone(),
        address: open.candidate.address.clone(),
        persist: true,
    };

    match client.connect_peer(connect_req).await {
        Ok(_) => {
            info!("Autopilot: connected to {}", open.candidate.node_id);
        }
        Err(e) => {
            // Connection failure might be OK if already connected
            warn!(
                "Autopilot: connect to {} returned: {} (may already be connected)",
                open.candidate.node_id, e
            );
        }
    }

    // Step 2: Open channel
    let open_req = OpenChannelRequest {
        node_pubkey: open.candidate.node_id.clone(),
        address: open.candidate.address.clone(),
        channel_amount_sats: open.amount_sats,
        push_to_counterparty_msat: None,
        channel_config: None,
        announce_channel: config.autopilot.announce_channels,
    };

    match client.open_channel(open_req).await {
        Ok(resp) => {
            info!(
                "Autopilot: channel opened with {} -- user_channel_id={}",
                open.candidate.node_id,
                resp.user_channel_id,
            );

            // Save peer address for reconnection
            let now = chrono::Utc::now().timestamp() as f64;
            db.conn().execute(
                "INSERT OR REPLACE INTO peer_addresses \
                 (node_id, address, last_connected_at, source) \
                 VALUES (?1, ?2, ?3, 'autopilot')",
                rusqlite::params![
                    open.candidate.node_id,
                    open.candidate.address,
                    now,
                ],
            )?;

            // Record in audit trail
            db.conn().execute(
                "INSERT INTO autopilot_opens \
                 (channel_id, counterparty_node_id, amount_sats, opened_at, reason) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    resp.user_channel_id,
                    open.candidate.node_id,
                    open.amount_sats,
                    now,
                    format!("source={:?}, score={:.2}", open.candidate.source, open.candidate.score),
                ],
            )?;
        }
        Err(e) => {
            error!(
                "Autopilot: failed to open channel with {}: {}",
                open.candidate.node_id, e
            );
            return Err(e);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::candidate::{Candidate, CandidateSource};
    use crate::config::Config;

    fn test_config() -> Config {
        Config::test_default(std::path::PathBuf::from("/dev/null"))
    }

    fn make_candidate(id: &str, addr: &str, score: f64) -> Candidate {
        Candidate {
            node_id: id.to_string(),
            address: addr.to_string(),
            score,
            source: CandidateSource::Hardcoded,
        }
    }

    #[test]
    fn test_plan_opens_basic() {
        let config = test_config();
        let candidates = vec![
            make_candidate("a", "1.2.3.4:9735", 100.0),
            make_candidate("b", "5.6.7.8:9735", 90.0),
        ];
        let plan = plan_opens(&config, &candidates, 500_000, 2);
        assert_eq!(plan.len(), 2);
        // Budget split roughly evenly (250k each), both above min_channel_sats (100k)
        assert!(plan[0].amount_sats >= config.autopilot.min_channel_sats);
        assert!(plan[1].amount_sats >= config.autopilot.min_channel_sats);
    }

    #[test]
    fn test_plan_opens_budget_too_small() {
        let config = test_config();
        let candidates = vec![make_candidate("a", "1.2.3.4:9735", 100.0)];
        // Budget below min_channel_sats (100_000)
        let plan = plan_opens(&config, &candidates, 50_000, 1);
        assert!(plan.is_empty());
    }

    #[test]
    fn test_plan_opens_respects_max_proposals() {
        let config = test_config();
        let candidates = vec![
            make_candidate("a", "1.2.3.4:9735", 100.0),
            make_candidate("b", "5.6.7.8:9735", 90.0),
            make_candidate("c", "9.10.11.12:9735", 80.0),
        ];
        let plan = plan_opens(&config, &candidates, 1_000_000, 2);
        assert!(plan.len() <= 2);
    }

    #[test]
    fn test_plan_opens_skips_no_address() {
        let config = test_config();
        let candidates = vec![
            make_candidate("a", "", 100.0), // No address
            make_candidate("b", "5.6.7.8:9735", 90.0),
        ];
        let plan = plan_opens(&config, &candidates, 500_000, 2);
        // Should skip "a" and only open with "b"
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].candidate.node_id, "b");
    }

    #[test]
    fn test_plan_opens_respects_max_channel_sats() {
        let mut config = test_config();
        config.autopilot.max_channel_sats = 200_000;
        let candidates = vec![make_candidate("a", "1.2.3.4:9735", 100.0)];
        let plan = plan_opens(&config, &candidates, 1_000_000, 1);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].amount_sats <= 200_000);
    }

    #[test]
    fn test_plan_opens_50_percent_cap() {
        let config = test_config();
        let candidates = vec![make_candidate("a", "1.2.3.4:9735", 100.0)];
        // With budget 400k and single candidate, 50% cap = 200k
        let plan = plan_opens(&config, &candidates, 400_000, 1);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].amount_sats <= 200_000);
    }

    #[test]
    fn test_plan_opens_empty_candidates() {
        let config = test_config();
        let plan = plan_opens(&config, &[], 1_000_000, 5);
        assert!(plan.is_empty());
    }
}
