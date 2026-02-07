use crate::client::LdkClient;
use crate::config::Config;
use ldk_server_protos::api::UpdateChannelConfigRequest;
use ldk_server_protos::types::{Channel, ChannelConfig};
use log::{debug, info};

/// Apply fee configuration to a channel, but only if it differs from the current config.
pub async fn apply_if_changed(
    config: &Config,
    client: &(impl LdkClient + Sync),
    channel: &Channel,
    new_base_msat: u32,
    new_ppm: u32,
) -> anyhow::Result<()> {
    // Get current config
    let current = channel.channel_config.as_ref();
    let current_base = current.and_then(|c| c.forwarding_fee_base_msat).unwrap_or(0);
    let current_ppm = current
        .and_then(|c| c.forwarding_fee_proportional_millionths)
        .unwrap_or(0);

    if current_base == new_base_msat && current_ppm == new_ppm {
        debug!(
            "Fee setter: channel {} unchanged (base={}msat, ppm={})",
            channel.channel_id, new_base_msat, new_ppm
        );
        return Ok(());
    }

    info!(
        "Fee setter: channel {} with {} -- base: {}->{}msat, ppm: {}->{}",
        channel.channel_id,
        channel.counterparty_node_id,
        current_base,
        new_base_msat,
        current_ppm,
        new_ppm,
    );

    if config.general.dry_run {
        info!("  (dry-run: not applying)");
        return Ok(());
    }

    let request = UpdateChannelConfigRequest {
        user_channel_id: channel.user_channel_id.clone(),
        counterparty_node_id: channel.counterparty_node_id.clone(),
        channel_config: Some(ChannelConfig {
            forwarding_fee_base_msat: Some(new_base_msat),
            forwarding_fee_proportional_millionths: Some(new_ppm),
            // Preserve existing values for fields we don't manage
            cltv_expiry_delta: current.and_then(|c| c.cltv_expiry_delta),
            force_close_avoidance_max_fee_satoshis: current
                .and_then(|c| c.force_close_avoidance_max_fee_satoshis),
            accept_underpaying_htlcs: current.and_then(|c| c.accept_underpaying_htlcs),
            max_dust_htlc_exposure: current.and_then(|c| c.max_dust_htlc_exposure.clone()),
        }),
    };

    client.update_channel_config(request).await?;

    Ok(())
}
