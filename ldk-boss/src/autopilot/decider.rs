/// Port of CLBoss ChannelCreationDecider logic.
///
/// Decides whether we should open new channels based on:
/// - Available on-chain balance (minus reserve)
/// - On-chain fee regime (low vs high)
/// - Percentage of funds on-chain vs in channels
///
/// Reference: clboss/Boss/Mod/ChannelCreationDecider.cpp

use crate::config::Config;
use crate::db::Database;
use crate::state::NodeState;
use crate::tracker::onchain_fees;
use log::{debug, info};

/// Returns Some(budget_sats) if we should open channels, None otherwise.
pub fn should_open(
    config: &Config,
    db: &Database,
    state: &NodeState,
) -> anyhow::Result<Option<u64>> {
    let onchain = state.balances.spendable_onchain_balance_sats;
    let reserve = config.autopilot.onchain_reserve_sats;

    // Must have more than the reserve
    if onchain <= reserve {
        debug!(
            "Autopilot decider: on-chain balance ({} sat) <= reserve ({} sat)",
            onchain, reserve
        );
        return Ok(None);
    }

    let available = onchain - reserve;

    // Must meet minimum channel size
    if available < config.autopilot.min_channel_sats {
        debug!(
            "Autopilot decider: available ({} sat) < min channel size ({} sat)",
            available, config.autopilot.min_channel_sats
        );
        return Ok(None);
    }

    // Check on-chain percentage
    let onchain_pct = state.onchain_percent();
    let total_funds = state.total_funds_sats();

    if total_funds == 0 {
        debug!("Autopilot decider: no funds at all");
        return Ok(None);
    }

    // If on-chain % is below minimum and we don't have excess, don't deploy more
    if onchain_pct < config.autopilot.min_onchain_percent {
        debug!(
            "Autopilot decider: on-chain {:.1}% < min {:.1}%, preserving on-chain funds",
            onchain_pct, config.autopilot.min_onchain_percent
        );
        return Ok(None);
    }

    // Check fee regime
    let regime = onchain_fees::current_regime(
        db,
        config.onchain_fees.hi_to_lo_percentile,
        config.onchain_fees.lo_to_hi_percentile,
    )?;

    match regime {
        onchain_fees::FeeRegime::Low => {
            info!(
                "Autopilot decider: low-fee regime, deploying {} sat",
                available
            );
            Ok(Some(available))
        }
        onchain_fees::FeeRegime::High => {
            // In high-fee regime, only deploy if we have excess on-chain
            if onchain_pct > config.autopilot.max_onchain_percent {
                info!(
                    "Autopilot decider: high-fee regime but on-chain {:.1}% > max {:.1}%, deploying {} sat",
                    onchain_pct, config.autopilot.max_onchain_percent, available
                );
                Ok(Some(available))
            } else {
                debug!(
                    "Autopilot decider: high-fee regime and on-chain {:.1}% <= max {:.1}%, waiting",
                    onchain_pct, config.autopilot.max_onchain_percent
                );
                Ok(None)
            }
        }
    }
}
