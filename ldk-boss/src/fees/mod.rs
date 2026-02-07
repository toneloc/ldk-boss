pub mod balance_modder;
pub mod price_theory;
pub mod setter;

use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use crate::state::NodeState;
use log::{debug, info};

/// Hard limits on fee values
pub const ABS_MIN_FEE_PPM: u32 = 1;
pub const ABS_MAX_FEE_PPM: u32 = 50_000;

/// Run the fee management module: compute and apply fees for all usable channels.
pub async fn run(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    state: &NodeState,
) -> anyhow::Result<()> {
    let usable_channels: Vec<_> = state.channels.iter().filter(|c| c.is_usable).collect();

    if usable_channels.is_empty() {
        debug!("Fee management: no usable channels");
        return Ok(());
    }

    info!("Fee management: evaluating {} usable channels", usable_channels.len());

    for channel in &usable_channels {
        let channel_value_sats = channel.channel_value_sats;
        if channel_value_sats == 0 {
            continue;
        }

        // Compute balance ratio: our outbound / total
        let our_balance_ratio = channel.outbound_capacity_msat as f64
            / (channel_value_sats as f64 * 1000.0);

        // Phase 1: Balance-based fee modifier
        let balance_mult = if config.fees.balance_modder_enabled {
            balance_modder::get_ratio_binned(
                our_balance_ratio,
                channel_value_sats,
                config.fees.preferred_bin_size_sats,
            )
        } else {
            1.0
        };

        // Phase 2: Price theory modifier
        let price_mult = if config.fees.price_theory_enabled {
            price_theory::get_fee_modifier(db, &channel.counterparty_node_id)?
        } else {
            1.0
        };

        let combined_mult = balance_mult * price_mult;

        // Compute final fees
        let base_msat = ((config.fees.default_base_msat as f64) * combined_mult) as u32;
        let ppm = ((config.fees.default_ppm as f64) * combined_mult) as u32;

        // Clamp to hard limits
        let ppm = ppm.max(ABS_MIN_FEE_PPM).min(ABS_MAX_FEE_PPM);
        let base_msat = base_msat.max(0);

        // Apply if different from current
        setter::apply_if_changed(
            config,
            client,
            channel,
            base_msat,
            ppm,
        )
        .await?;
    }

    // Update price theory tick
    if config.fees.price_theory_enabled {
        let peer_ids: Vec<String> = usable_channels
            .iter()
            .map(|c| c.counterparty_node_id.clone())
            .collect();
        price_theory::update_tick(db, &peer_ids, &config.fees)?;
    }

    Ok(())
}
