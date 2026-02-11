use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use crate::judge::algo::CloseRecommendation;
use crate::state::NodeState;
use ldk_server_protos::api::{CloseChannelRequest, ForceCloseChannelRequest};
use log::{error, info};

/// Execute a channel closure based on judge recommendation.
///
/// Safety: Only closes ONE channel per cycle (hard limit).
pub async fn execute_closure(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    state: &NodeState,
    recommendation: &CloseRecommendation,
) -> anyhow::Result<()> {
    // Find the channel(s) with this peer
    let peer_channels: Vec<_> = state
        .channels
        .iter()
        .filter(|c| c.counterparty_node_id == recommendation.counterparty_node_id && c.is_usable)
        .collect();

    if peer_channels.is_empty() {
        info!(
            "Judge: peer {} has no usable channels to close",
            recommendation.counterparty_node_id
        );
        return Ok(());
    }

    // Close the smallest channel with this peer first
    let channel = peer_channels
        .iter()
        .min_by_key(|c| c.channel_value_sats)
        .unwrap();

    info!(
        "Judge: closing channel {} with peer {} ({} sat) -- {}",
        channel.channel_id,
        recommendation.counterparty_node_id,
        channel.channel_value_sats,
        recommendation.reason,
    );

    if config.general.dry_run {
        info!("  (dry-run: not executing)");
        return Ok(());
    }

    let result = if config.judge.cooperative_close {
        client
            .close_channel(CloseChannelRequest {
                user_channel_id: channel.user_channel_id.clone(),
                counterparty_node_id: channel.counterparty_node_id.clone(),
            })
            .await
            .map(|_| ())
    } else {
        client
            .force_close_channel(ForceCloseChannelRequest {
                user_channel_id: channel.user_channel_id.clone(),
                counterparty_node_id: channel.counterparty_node_id.clone(),
                force_close_reason: Some(recommendation.reason.clone()),
            })
            .await
            .map(|_| ())
    };

    match result {
        Ok(()) => {
            info!(
                "Judge: successfully closed channel {} with {}",
                channel.channel_id, recommendation.counterparty_node_id
            );

            // Record in audit trail
            let now = chrono::Utc::now().timestamp() as f64;
            db.conn().execute(
                "INSERT INTO judge_closures \
                 (channel_id, counterparty_node_id, closed_at, reason) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    channel.channel_id,
                    recommendation.counterparty_node_id,
                    now,
                    recommendation.reason,
                ],
            )?;
        }
        Err(e) => {
            error!(
                "Judge: failed to close channel {} with {}: {}",
                channel.channel_id, recommendation.counterparty_node_id, e
            );
        }
    }

    Ok(())
}
