/// Port of CLBoss FeeModderBySize.
///
/// Adjusts fees based on our node's capacity relative to competitors
/// who also route through the same peer. Larger nodes charge higher fees
/// because they provide more reliable routing; smaller nodes discount to
/// attract traffic.
///
/// Algorithm (from CLBoss):
/// 1. For each peer, find their other channel partners (our competitors)
/// 2. Estimate each competitor's total capacity
/// 3. Count how many competitors are worse (smaller) or better (larger) than us
/// 4. Compute a multiplier based on our relative position:
///    - Smaller than most competitors: multiplier in [0.5, 1.0]
///    - Larger than most competitors: multiplier in [1.0, ~16+]
///
/// Reference: clboss/Boss/Mod/FeeModderBySize.cpp

use crate::client::LdkClient;
use ldk_server_protos::api::{GraphGetChannelRequest, GraphGetNodeRequest};
use log::debug;
use rand::seq::SliceRandom;
use std::collections::HashSet;

/// Maximum competitor channels to sample per peer.
const MAX_CHANNELS_TO_SAMPLE: usize = 10;
/// Minimum competitors needed for a meaningful comparison.
const MIN_COMPETITORS: u32 = 2;

/// Get the fee multiplier based on our relative size vs competitors routing
/// through the same peer.
///
/// `own_capacity_sats` is our total channel capacity across all channels.
/// Returns `Some(multiplier)` or `None` if insufficient data.
pub async fn get_size_modifier(
    client: &impl LdkClient,
    peer_node_id: &str,
    own_node_id: &str,
    own_capacity_sats: u64,
) -> Option<f64> {
    let resp = client
        .graph_get_node(GraphGetNodeRequest {
            node_id: peer_node_id.to_string(),
        })
        .await
        .ok()?;

    let node = resp.node?;
    if node.channels.is_empty() {
        return None;
    }

    // Sample a random subset of the peer's channels
    let mut rng = rand::thread_rng();
    let mut channel_ids = node.channels;
    channel_ids.shuffle(&mut rng);
    let sample: Vec<u64> = channel_ids
        .into_iter()
        .take(MAX_CHANNELS_TO_SAMPLE)
        .collect();

    let mut worse: u32 = 0; // competitors with less capacity than us
    let mut better: u32 = 0; // competitors with more capacity than us
    let mut seen_competitors: HashSet<String> = HashSet::new();

    for scid in sample {
        let ch_resp = match client
            .graph_get_channel(GraphGetChannelRequest {
                short_channel_id: scid,
            })
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };

        let ch = match ch_resp.channel {
            Some(c) => c,
            None => continue,
        };

        // Skip our own channel
        if ch.node_one == own_node_id || ch.node_two == own_node_id {
            continue;
        }

        // Find the competitor (the other end from the peer)
        let competitor_id = if ch.node_one == peer_node_id {
            &ch.node_two
        } else {
            &ch.node_one
        };

        if seen_competitors.contains(competitor_id) {
            continue;
        }
        seen_competitors.insert(competitor_id.to_string());

        // Estimate competitor's total capacity:
        // Use the channel capacity we can see × their total channel count.
        // This overestimates for nodes with mixed-size channels but maintains
        // correct relative ordering (which is what the algorithm needs).
        let channel_capacity = ch.capacity_sats.unwrap_or(0);
        if channel_capacity == 0 {
            continue;
        }

        let competitor_node = match client
            .graph_get_node(GraphGetNodeRequest {
                node_id: competitor_id.to_string(),
            })
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };

        let competitor_channel_count = competitor_node
            .node
            .map(|n| n.channels.len())
            .unwrap_or(0) as u64;
        if competitor_channel_count == 0 {
            continue;
        }

        let estimated_capacity = channel_capacity.saturating_mul(competitor_channel_count);

        if estimated_capacity < own_capacity_sats {
            worse += 1;
        } else if estimated_capacity > own_capacity_sats {
            better += 1;
        }
        // Equal: neither worse nor better
    }

    let total = worse + better;
    if total < MIN_COMPETITORS {
        debug!(
            "SizeModder: only {} competitors for peer {} (need {}), skipping",
            total, peer_node_id, MIN_COMPETITORS,
        );
        return None;
    }

    let mult = calculate_multiplier(worse, better, total);
    debug!(
        "SizeModder: peer {} -- worse={}, better={}, multiplier={:.3}",
        peer_node_id, worse, better, mult,
    );
    Some(mult)
}

/// Calculate fee multiplier from competitive position.
///
/// Port of CLBoss `FeeModderBySize::calculate_multiplier`.
///
/// - position 0.0: all competitors are larger → multiplier ~0.5
/// - position 0.5: equal split → multiplier ~1.0
/// - position 1.0: all competitors are smaller → multiplier up to ~16
fn calculate_multiplier(worse: u32, better: u32, total: u32) -> f64 {
    if total == 0 {
        return 1.0;
    }

    let position = worse as f64 / total as f64;

    if worse < better {
        // We're in the smaller half: remap [0, 0.5] → [0, 1]
        let pos = position * 2.0;
        let base = 0.5_f64.sqrt(); // ~0.7071
        let x = base * (1.0 - pos) + pos;
        x * x // Range: [0.5, 1.0]
    } else {
        // We're in the larger half: remap [0.5, 1.0] → [0, 1]
        let pos = position * 2.0 - 1.0;
        let log_inv_total = (1.0 / total as f64).ln();
        let lim = (16.0 + log_inv_total).max(2.0);
        let x = (1.0 - pos) + pos * lim.sqrt();
        x * x // Range: [1.0, lim]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multiplier_all_larger() {
        // We're the smallest: worse=0, better=10
        let m = calculate_multiplier(0, 10, 10);
        assert!((m - 0.5).abs() < 0.01, "Expected ~0.5, got {}", m);
    }

    #[test]
    fn test_multiplier_balanced() {
        // Equal split: worse=5, better=5
        let m = calculate_multiplier(5, 5, 10);
        assert!((m - 1.0).abs() < 0.01, "Expected ~1.0, got {}", m);
    }

    #[test]
    fn test_multiplier_all_smaller() {
        // We're the largest: worse=10, better=0
        let m = calculate_multiplier(10, 0, 10);
        assert!(m > 1.0, "Should be > 1.0, got {}", m);
    }

    #[test]
    fn test_multiplier_no_competitors() {
        let m = calculate_multiplier(0, 0, 0);
        assert!((m - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_multiplier_monotonic_increase() {
        // As our position improves (more worse, fewer better), multiplier should increase
        let m1 = calculate_multiplier(1, 9, 10);
        let m2 = calculate_multiplier(3, 7, 10);
        let m3 = calculate_multiplier(5, 5, 10);
        let m4 = calculate_multiplier(7, 3, 10);
        let m5 = calculate_multiplier(9, 1, 10);
        assert!(m1 < m2, "m1={} should be < m2={}", m1, m2);
        assert!(m2 < m3, "m2={} should be < m3={}", m2, m3);
        assert!(m3 < m4, "m3={} should be < m4={}", m3, m4);
        assert!(m4 < m5, "m4={} should be < m5={}", m4, m5);
    }

    #[test]
    fn test_multiplier_lim_scales_with_total() {
        // More competitors → lower maximum multiplier (CLBoss behavior)
        let m_few = calculate_multiplier(3, 0, 3);
        let m_many = calculate_multiplier(100, 0, 100);
        assert!(
            m_few > m_many,
            "Few competitors ({}) should allow higher mult than many ({})",
            m_few,
            m_many
        );
    }

    #[tokio::test]
    async fn test_size_modifier_with_mock() {
        use crate::client::mock::MockLdkClient;
        use ldk_server_protos::api::{GraphGetChannelResponse, GraphGetNodeResponse};
        use ldk_server_protos::types::{GraphChannel, GraphNode};

        let mut mock = MockLdkClient::new();
        let own_id = mock.node_info.node_id.clone();
        let peer_id = "peer_node";

        // Peer has 5 channels to competitors
        mock.graph_node_details.insert(
            peer_id.to_string(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![1, 2, 3, 4, 5],
                    announcement_info: None,
                }),
            },
        );

        // Competitors with various channel counts
        // Our capacity: 5M sats
        // Competitor 1: 2M channel × 3 channels = est 6M (bigger)
        // Competitor 2: 1M channel × 2 channels = est 2M (smaller)
        // Competitor 3: 500K channel × 1 channel = est 500K (smaller)
        // Competitor 4: 3M channel × 10 channels = est 30M (bigger)
        // Competitor 5: 1M channel × 5 channels = est 5M (equal)
        for (scid, comp_id, cap, count) in [
            (1u64, "comp_1", 2_000_000u64, 3usize),
            (2, "comp_2", 1_000_000, 2),
            (3, "comp_3", 500_000, 1),
            (4, "comp_4", 3_000_000, 10),
            (5, "comp_5", 1_000_000, 5),
        ] {
            mock.graph_channel_details.insert(
                scid,
                GraphGetChannelResponse {
                    channel: Some(GraphChannel {
                        node_one: peer_id.to_string(),
                        node_two: comp_id.to_string(),
                        capacity_sats: Some(cap),
                        one_to_two: None,
                        two_to_one: None,
                    }),
                },
            );
            mock.graph_node_details.insert(
                comp_id.to_string(),
                GraphGetNodeResponse {
                    node: Some(GraphNode {
                        channels: (0..count as u64).collect(),
                        announcement_info: None,
                    }),
                },
            );
        }

        let result = get_size_modifier(&mock, peer_id, &own_id, 5_000_000).await;
        assert!(result.is_some(), "Should get a multiplier");
        let mult = result.unwrap();
        // worse=2 (comp_2 2M, comp_3 500K), better=2 (comp_1 6M, comp_4 30M), equal=1
        // Position = 2/4 = 0.5, so multiplier should be ~1.0
        assert!(
            mult > 0.8 && mult < 1.2,
            "Expected near 1.0, got {}",
            mult
        );
    }

    #[tokio::test]
    async fn test_size_modifier_excludes_own_channels() {
        use crate::client::mock::MockLdkClient;
        use ldk_server_protos::api::{GraphGetChannelResponse, GraphGetNodeResponse};
        use ldk_server_protos::types::{GraphChannel, GraphNode};

        let mut mock = MockLdkClient::new();
        let own_id = mock.node_info.node_id.clone();
        let peer_id = "peer_node";

        // Peer has 5 channels: 2 to us (skip), 3 to competitors
        mock.graph_node_details.insert(
            peer_id.to_string(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![1, 2, 3, 4, 5],
                    announcement_info: None,
                }),
            },
        );

        // Our channels (should be excluded)
        mock.graph_channel_details.insert(
            1,
            GraphGetChannelResponse {
                channel: Some(GraphChannel {
                    node_one: peer_id.to_string(),
                    node_two: own_id.clone(),
                    capacity_sats: Some(10_000_000),
                    one_to_two: None,
                    two_to_one: None,
                }),
            },
        );
        mock.graph_channel_details.insert(
            2,
            GraphGetChannelResponse {
                channel: Some(GraphChannel {
                    node_one: own_id.clone(),
                    node_two: peer_id.to_string(),
                    capacity_sats: Some(10_000_000),
                    one_to_two: None,
                    two_to_one: None,
                }),
            },
        );

        // Competitor channels (all smaller → we're bigger → mult > 1.0)
        for (scid, comp_id) in [(3, "comp_3"), (4, "comp_4"), (5, "comp_5")] {
            mock.graph_channel_details.insert(
                scid,
                GraphGetChannelResponse {
                    channel: Some(GraphChannel {
                        node_one: peer_id.to_string(),
                        node_two: comp_id.to_string(),
                        capacity_sats: Some(100_000),
                        one_to_two: None,
                        two_to_one: None,
                    }),
                },
            );
            mock.graph_node_details.insert(
                comp_id.to_string(),
                GraphGetNodeResponse {
                    node: Some(GraphNode {
                        channels: vec![scid as u64],
                        announcement_info: None,
                    }),
                },
            );
        }

        let result = get_size_modifier(&mock, peer_id, &own_id, 5_000_000).await;
        assert!(result.is_some());
        let mult = result.unwrap();
        // We're 5M, competitors are ~100K each → we're much bigger → mult > 1.0
        assert!(mult > 1.0, "Should charge more as larger node, got {}", mult);
    }

    #[tokio::test]
    async fn test_size_modifier_insufficient_data() {
        use crate::client::mock::MockLdkClient;

        let mock = MockLdkClient::new();
        let result = get_size_modifier(&mock, "unknown_peer", "own_node", 1_000_000).await;
        assert!(result.is_none(), "Should return None for unknown peer");
    }
}
