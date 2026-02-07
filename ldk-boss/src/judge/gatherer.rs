use crate::config::Config;
use crate::db::Database;
use crate::judge::algo::PeerInfo;
use crate::state::NodeState;
use crate::tracker::{channels as channel_tracker, earnings as earnings_tracker};
use log::debug;

/// Gather peer performance data for the judge algorithm.
///
/// Only includes peers whose channels are old enough (min_age_days).
pub fn gather(
    config: &Config,
    db: &Database,
    state: &NodeState,
) -> anyhow::Result<Vec<PeerInfo>> {
    let min_age = config.judge.min_age_days as f64;
    let eval_window = config.judge.evaluation_window_days;
    let since = chrono::Utc::now().timestamp() as f64 - (eval_window as f64 * 86400.0);

    let peers_channels = state.channels_by_peer();
    let mut infos = Vec::new();

    for (peer_id, channels) in &peers_channels {
        // Only consider usable channels
        let usable: Vec<_> = channels.iter().filter(|c| c.is_usable).collect();
        if usable.is_empty() {
            continue;
        }

        // Check channel age: use the oldest channel with this peer
        let mut oldest_age: f64 = 0.0;
        for ch in &usable {
            if let Some(age) = channel_tracker::channel_age_days(db, &ch.channel_id)? {
                if age > oldest_age {
                    oldest_age = age;
                }
            }
        }

        if oldest_age < min_age {
            debug!(
                "Judge gatherer: peer {} channel age {:.0} days < min {} days, skipping",
                peer_id, oldest_age, min_age
            );
            continue;
        }

        // Sum channel capacity
        let total_sats: u64 = usable.iter().map(|c| c.channel_value_sats).sum();

        // Get earnings in evaluation window
        let peer_earnings = earnings_tracker::peer_earnings_since(db, peer_id, since)?;
        let total_earned = peer_earnings.total_net();

        infos.push(PeerInfo {
            counterparty_node_id: peer_id.to_string(),
            total_channel_sats: total_sats,
            total_earned_msat: total_earned,
        });
    }

    debug!("Judge gatherer: {} peers eligible for evaluation", infos.len());

    Ok(infos)
}
