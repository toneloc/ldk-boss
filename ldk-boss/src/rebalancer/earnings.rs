/// Port of CLBoss EarningsRebalancer.
///
/// Identifies channels that need rebalancing based on their spendable percentage
/// and net earnings, then executes circular rebalances via self-invoices.
///
/// Algorithm:
/// - Destinations: channels where spendable < 25% of total (need more outbound)
/// - Sources: channels where spendable > 27.5% of total (have excess outbound)
/// - Sort by net earnings (highest first)
/// - Pair top 20th percentile
/// - Execute via Bolt11Receive + Bolt11Send
///
/// Reference: clboss/Boss/Mod/EarningsRebalancer.cpp

use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use crate::tracker::earnings as earnings_tracker;
use ldk_server_protos::api::{Bolt11ReceiveRequest, Bolt11SendRequest};
use ldk_server_protos::types::{
    bolt11_invoice_description, Bolt11InvoiceDescription, Channel, RouteParametersConfig,
};
use log::{debug, info, warn};

/// Hard cap on rebalance fee per cycle (satoshis).
const ABS_MAX_REBALANCE_FEE_SATS: u64 = 50_000;
/// Top percentile of channels to rebalance.
const TOP_REBALANCING_PERCENTILE: f64 = 20.0;

struct ChannelBalance {
    counterparty_node_id: String,
    channel_id: String,
    spendable_msat: u64,
    total_msat: u64,
    spendable_percent: f64,
}

pub async fn run(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    channels: &[&Channel],
) -> anyhow::Result<()> {
    let max_spendable = config.rebalancer.max_spendable_percent;
    let source_gap = config.rebalancer.source_gap_percent;
    let target_pct = config.rebalancer.target_spendable_percent;
    let max_fee_ppm = config.rebalancer.max_fee_ppm;

    // Compute balances
    let balances: Vec<ChannelBalance> = channels
        .iter()
        .filter_map(|ch| {
            let total_msat = ch.channel_value_sats * 1000;
            if total_msat == 0 {
                return None;
            }
            let spendable_msat = ch.outbound_capacity_msat;
            let spendable_percent = (spendable_msat as f64 / total_msat as f64) * 100.0;
            Some(ChannelBalance {
                counterparty_node_id: ch.counterparty_node_id.clone(),
                channel_id: ch.channel_id.clone(),
                spendable_msat,
                total_msat,
                spendable_percent,
            })
        })
        .collect();

    // Classify into sources and destinations
    let since = chrono::Utc::now().timestamp() as f64 - 30.0 * 86400.0; // last 30 days

    let mut destinations: Vec<(usize, i64)> = Vec::new(); // (index, out_net_earnings)
    let mut sources: Vec<(usize, i64)> = Vec::new(); // (index, in_net_earnings)

    for (i, bal) in balances.iter().enumerate() {
        let peer_earnings = earnings_tracker::peer_earnings_since(
            db,
            &bal.counterparty_node_id,
            since,
        )?;

        if bal.spendable_percent < max_spendable {
            destinations.push((i, peer_earnings.out_net()));
        } else if bal.spendable_percent > max_spendable + source_gap {
            sources.push((i, peer_earnings.in_net()));
        }
    }

    if destinations.is_empty() || sources.is_empty() {
        debug!("Rebalancer: nothing to do (no source/destination pairs)");
        return Ok(());
    }

    // Sort destinations by out_net_earnings (highest first)
    destinations.sort_by(|a, b| b.1.cmp(&a.1));
    // Sort sources by in_net_earnings (highest first)
    sources.sort_by(|a, b| b.1.cmp(&a.1));

    // Pair the top percentile
    let num = destinations.len().min(sources.len());
    let num_rebalance = ((num as f64 * TOP_REBALANCING_PERCENTILE / 100.0) as usize).max(1);

    let max_total_fee = config
        .rebalancer
        .max_total_fee_sats
        .min(ABS_MAX_REBALANCE_FEE_SATS);
    let mut total_fee_spent: u64 = 0;

    for i in 0..num_rebalance {
        let (dst_idx, dst_earnings) = destinations[i];
        let (src_idx, _src_earnings) = sources[i];

        let dst = &balances[dst_idx];
        let src = &balances[src_idx];

        // If destination has negative out-earnings, skip (don't throw good money after bad)
        if dst_earnings <= 0 {
            info!(
                "Rebalancer: peer {} has negative net earnings ({}msat), skipping",
                dst.counterparty_node_id, dst_earnings
            );
            break; // List is sorted, so everything after is worse
        }

        // Compute amounts
        let dest_target_msat = (dst.total_msat as f64 * target_pct / 100.0) as u64;
        let dest_needed_msat = dest_target_msat.saturating_sub(dst.spendable_msat);

        let src_min_allowed_msat =
            (src.total_msat as f64 * (max_spendable + source_gap) / 100.0) as u64;
        let src_budget_msat = src.spendable_msat.saturating_sub(src_min_allowed_msat);

        let amount_msat = dest_needed_msat.min(src_budget_msat);
        if amount_msat == 0 {
            continue;
        }

        // Compute fee budget
        let fee_budget_msat = (amount_msat as f64 * max_fee_ppm as f64 / 1_000_000.0) as u64;
        // Cap at destination's net earnings
        let fee_budget_msat = fee_budget_msat.min(dst_earnings as u64);
        // Cap at remaining total budget
        let remaining_budget = (max_total_fee * 1000).saturating_sub(total_fee_spent);
        let fee_budget_msat = fee_budget_msat.min(remaining_budget);

        if fee_budget_msat == 0 {
            continue;
        }

        info!(
            "Rebalancer: {} -> {} ({} msat), max fee {} msat",
            src.counterparty_node_id, dst.counterparty_node_id, amount_msat, fee_budget_msat
        );

        if config.general.dry_run {
            info!("  (dry-run: not executing)");
            continue;
        }

        // Execute via self-invoice
        match execute_rebalance(client, amount_msat, fee_budget_msat).await {
            Ok(fee_paid) => {
                total_fee_spent += fee_paid;
                info!("Rebalancer: success, fee paid: {} msat", fee_paid);

                // Record in rebalance_costs
                let now_bucket = {
                    let now = chrono::Utc::now().timestamp();
                    now - (now % 86400)
                };
                let conn = db.conn();
                conn.execute(
                    "INSERT INTO rebalance_costs \
                     (channel_id, counterparty_node_id, day_bucket, fee_spent_msat, \
                      amount_rebalanced_msat, direction) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'out') \
                     ON CONFLICT(channel_id, day_bucket, direction) DO UPDATE SET \
                     fee_spent_msat = fee_spent_msat + ?4, \
                     amount_rebalanced_msat = amount_rebalanced_msat + ?5",
                    rusqlite::params![
                        src.channel_id,
                        src.counterparty_node_id,
                        now_bucket,
                        fee_paid,
                        amount_msat,
                    ],
                )?;
            }
            Err(e) => {
                warn!("Rebalancer: failed: {}", e);
            }
        }
    }

    Ok(())
}

/// Execute a circular rebalance: create a self-invoice and pay it.
async fn execute_rebalance(
    client: &(impl LdkClient + Sync),
    amount_msat: u64,
    max_fee_msat: u64,
) -> anyhow::Result<u64> {
    // Step 1: Create self-invoice
    let invoice_resp = client
        .bolt11_receive(Bolt11ReceiveRequest {
            amount_msat: Some(amount_msat),
            description: Some(Bolt11InvoiceDescription {
                kind: Some(
                    bolt11_invoice_description::Kind::Direct(
                        "ldk-boss rebalance".to_string(),
                    ),
                ),
            }),
            expiry_secs: 600, // 10 minutes
        })
        .await?;

    // Step 2: Pay the self-invoice with fee constraints
    let _send_resp = client
        .bolt11_send(Bolt11SendRequest {
            invoice: invoice_resp.invoice,
            amount_msat: None, // Amount is in the invoice
            route_parameters: Some(RouteParametersConfig {
                max_total_routing_fee_msat: Some(max_fee_msat),
                max_total_cltv_expiry_delta: 1008,
                max_path_count: 3,
                max_channel_saturation_power_of_half: 2,
            }),
        })
        .await?;

    // NOTE: We record max_fee_msat as the fee paid because Bolt11SendResponse
    // does not include the actual routing fee. This overstates costs slightly,
    // which means the rebalancer is conservative with its fee budget.
    // TODO: Query ListPayments after payment to get exact fee.
    Ok(max_fee_msat)
}
