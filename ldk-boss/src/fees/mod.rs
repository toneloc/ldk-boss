pub mod balance_modder;
pub mod competitor;
pub mod price_theory;
pub mod setter;
pub mod size_modder;

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

    let own_node_id = &state.node_info.node_id;
    let own_capacity_sats = state.total_channel_capacity_sats();

    for channel in &usable_channels {
        let channel_value_sats = channel.channel_value_sats;
        if channel_value_sats == 0 {
            continue;
        }

        // Phase 0: Competitor fee baseline (market-relative base fees)
        let (base_ppm, base_base_msat) = if config.fees.competitor_fee_enabled {
            match competitor::get_competitor_fees(
                client,
                &channel.counterparty_node_id,
                own_node_id,
            )
            .await
            {
                Some(cf) => {
                    debug!(
                        "Fee management: competitor baseline for {}: {}ppm, {}msat",
                        channel.counterparty_node_id, cf.median_ppm, cf.median_base_msat
                    );
                    (cf.median_ppm, cf.median_base_msat)
                }
                None => (config.fees.default_ppm, config.fees.default_base_msat),
            }
        } else {
            (config.fees.default_ppm, config.fees.default_base_msat)
        };

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

        // Phase 3: Size-based modifier (relative capacity vs competitors)
        let size_mult = if config.fees.size_modder_enabled {
            size_modder::get_size_modifier(
                client,
                &channel.counterparty_node_id,
                own_node_id,
                own_capacity_sats,
            )
            .await
            .unwrap_or(1.0)
        } else {
            1.0
        };

        let combined_mult = balance_mult * price_mult * size_mult;

        // Compute final fees using competitor baseline (or config default)
        let base_msat = ((base_base_msat as f64) * combined_mult) as u32;
        let ppm = ((base_ppm as f64) * combined_mult) as u32;

        // Clamp to hard limits
        let ppm = ppm.clamp(ABS_MIN_FEE_PPM, ABS_MAX_FEE_PPM);

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
