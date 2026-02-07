use crate::config::Config;
use crate::db::Database;
use log::{debug, warn};
use std::collections::HashSet;

/// A candidate node to open a channel with.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub node_id: String,
    pub address: String,
    pub score: f64,
    pub source: CandidateSource,
}

#[derive(Debug, Clone)]
pub enum CandidateSource {
    Hardcoded,
    SeedNode,
    Earnings,
    External,
}

/// Well-known, highly-connected Lightning routing nodes.
/// These serve as a fallback when no external ranking API is configured.
const HARDCODED_NODES: &[(&str, &str)] = &[
    // ACINQ
    (
        "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
        "3.33.236.230:9735",
    ),
    // Kraken
    (
        "02f1a8c87607f415c8f22c00571c93e301a0ab6e73e38bfa3eb97ee71f96aab5f6",
        "52.13.118.208:9735",
    ),
    // River Financial
    (
        "03037dc08e9ac63b82581f79b662a4d0ceca8a8ca162b1af3551595b8f2d97b70a",
        "104.196.249.140:9735",
    ),
    // Wallet of Satoshi
    (
        "035e4ff418fc8b5554c5d9eea66396c227bd3a1a07c54c2b7b8d8dfdfc0e0a941b",
        "170.75.163.209:9735",
    ),
    // Bitfinex
    (
        "033d8656219478701227199cbd6f670335c8d408a92ae88b962c49d4dc0e83e025",
        "3.33.236.230:9735",
    ),
    // OpenNode
    (
        "028d98b9969fbed53784a36617eb489a59ab6dc9b9d77571a4a3e5cba4a0c71284",
        "18.221.23.28:9735",
    ),
    // Fold
    (
        "02816caed43171d3c9854e3b0ab2dee0a029c7290e2dd04cf4a68df1e8a0586cac",
        "35.238.153.25:9735",
    ),
    // Boltz
    (
        "026165850492521f4ac8abd9bd8088123446d126f648ca35e60f88177dc149ceb2",
        "24.249.146.89:9735",
    ),
    // Zero Fee Routing
    (
        "038863cf8ab91046230f561cd5b386cbff8309fa02e3f0c3ed161a3aeb64a643b9",
        "203.132.95.10:9735",
    ),
    // LNBig
    (
        "0331f80652fb840239df8dc99205792bba2e559a05469915804c08420230e23c7c",
        "138.68.14.104:9735",
    ),
];

/// Get a ranked list of channel candidates.
pub async fn get_candidates(
    config: &Config,
    db: &Database,
    existing_peers: &HashSet<String>,
) -> anyhow::Result<Vec<Candidate>> {
    let mut candidates = Vec::new();

    // Source 1: User-configured seed nodes
    for seed in &config.autopilot.seed_nodes {
        if let Some((node_id, address)) = parse_node_address(seed) {
            if !existing_peers.contains(&node_id) && !is_blacklisted(config, &node_id) {
                candidates.push(Candidate {
                    node_id,
                    address,
                    score: 100.0, // Highest priority
                    source: CandidateSource::SeedNode,
                });
            }
        }
    }

    // Source 2: Earnings-based candidates (nodes we route through often)
    let earnings_candidates = get_earnings_candidates(db, existing_peers)?;
    for c in earnings_candidates {
        if !is_blacklisted(config, &c.node_id) {
            candidates.push(c);
        }
    }

    // Source 3: External ranking API (if configured)
    if !config.autopilot.ranking_api_url.is_empty() {
        match fetch_external_candidates(&config.autopilot.ranking_api_url).await {
            Ok(external) => {
                for c in external {
                    if !existing_peers.contains(&c.node_id) && !is_blacklisted(config, &c.node_id)
                    {
                        candidates.push(c);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to fetch external candidates: {}", e);
            }
        }
    }

    // Source 4: Hardcoded well-known nodes
    for (node_id, address) in HARDCODED_NODES {
        let node_id = node_id.to_string();
        if !existing_peers.contains(&node_id) && !is_blacklisted(config, &node_id) {
            // Only add if not already in candidates
            if !candidates.iter().any(|c| c.node_id == node_id) {
                candidates.push(Candidate {
                    node_id,
                    address: address.to_string(),
                    score: 10.0,
                    source: CandidateSource::Hardcoded,
                });
            }
        }
    }

    // Sort by score descending
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    debug!("Autopilot: {} candidates available", candidates.len());

    Ok(candidates)
}

/// Find nodes that appear frequently in our forwarding history.
fn get_earnings_candidates(
    db: &Database,
    existing_peers: &HashSet<String>,
) -> anyhow::Result<Vec<Candidate>> {
    let conn = db.conn();
    let mut candidates = Vec::new();

    // Find nodes that appear in our forwarding history but aren't our peers
    let mut stmt = conn.prepare(
        "SELECT counterparty_node_id, SUM(fee_earned_msat) as total_earned \
         FROM earnings \
         GROUP BY counterparty_node_id \
         HAVING total_earned > 0 \
         ORDER BY total_earned DESC \
         LIMIT 20",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
        ))
    })?;

    for row in rows {
        let (node_id, earned) = row?;
        if !existing_peers.contains(&node_id) {
            candidates.push(Candidate {
                node_id,
                address: String::new(), // Will need to look up or skip
                score: (earned as f64).sqrt() / 100.0, // Moderate priority
                source: CandidateSource::Earnings,
            });
        }
    }

    Ok(candidates)
}

fn is_blacklisted(config: &Config, node_id: &str) -> bool {
    config.autopilot.blacklist.iter().any(|b| b == node_id)
}

fn parse_node_address(s: &str) -> Option<(String, String)> {
    // Format: node_id@host:port
    let parts: Vec<&str> = s.splitn(2, '@').collect();
    if parts.len() == 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

async fn fetch_external_candidates(_url: &str) -> anyhow::Result<Vec<Candidate>> {
    // Placeholder for external API integration.
    // Could integrate with 1ML, Amboss, or a custom ranking service.
    // For now, return empty.
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_config() -> Config {
        Config::test_default(std::path::PathBuf::from("/dev/null"))
    }

    #[test]
    fn test_parse_node_address_valid() {
        let result = parse_node_address("03abc123@1.2.3.4:9735");
        assert_eq!(
            result,
            Some(("03abc123".to_string(), "1.2.3.4:9735".to_string()))
        );
    }

    #[test]
    fn test_parse_node_address_no_at() {
        assert_eq!(parse_node_address("03abc123"), None);
    }

    #[test]
    fn test_parse_node_address_empty() {
        assert_eq!(parse_node_address(""), None);
    }

    #[test]
    fn test_parse_node_address_with_onion() {
        let result = parse_node_address("03abc@nodehost.onion:9735");
        assert_eq!(
            result,
            Some(("03abc".to_string(), "nodehost.onion:9735".to_string()))
        );
    }

    #[test]
    fn test_is_blacklisted() {
        let mut config = test_config();
        config.autopilot.blacklist = vec!["badnode123".to_string()];
        assert!(is_blacklisted(&config, "badnode123"));
        assert!(!is_blacklisted(&config, "goodnode456"));
    }

    #[test]
    fn test_is_blacklisted_empty() {
        let config = test_config();
        assert!(!is_blacklisted(&config, "anynode"));
    }
}
