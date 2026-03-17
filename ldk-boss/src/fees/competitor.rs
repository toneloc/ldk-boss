use crate::client::LdkClient;
use ldk_server_protos::api::{GraphGetChannelRequest, GraphGetNodeRequest};
use log::debug;
use rand::seq::SliceRandom;

/// Maximum channels to sample per peer for competitor fee survey.
const MAX_CHANNELS_TO_SAMPLE: usize = 10;
/// Minimum valid samples required to produce a reliable median.
const MIN_SAMPLES_FOR_MEDIAN: usize = 3;

/// Competitor fee survey results.
#[derive(Debug, Clone)]
pub struct CompetitorFees {
    pub median_ppm: u32,
    pub median_base_msat: u32,
}

/// Survey competitor fees for a given peer by examining their other channels
/// in the gossip graph.
///
/// Port of CLBoss `PeerCompetitorFeeMonitor::Surveyor`:
/// For each of the peer's channels (excluding ours), reads the fee rate
/// that the competitor charges *toward* the peer, and returns the median.
pub async fn get_competitor_fees(
    client: &impl LdkClient,
    peer_node_id: &str,
    own_node_id: &str,
) -> Option<CompetitorFees> {
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

    let mut ppm_samples = Vec::new();
    let mut base_samples = Vec::new();

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

        // Skip our own channel — we're not our own competitor
        if ch.node_one == own_node_id || ch.node_two == own_node_id {
            continue;
        }

        // We want the fee the competitor charges *toward* the peer.
        // If peer is node_one, the inbound direction is two_to_one (competitor→peer).
        // If peer is node_two, the inbound direction is one_to_two (competitor→peer).
        let update = if ch.node_one == peer_node_id {
            ch.two_to_one.as_ref()
        } else {
            ch.one_to_two.as_ref()
        };

        let update = match update {
            Some(u) if u.enabled => u,
            _ => continue,
        };

        if let Some(fees) = &update.fees {
            ppm_samples.push(fees.proportional_millionths);
            base_samples.push(fees.base_msat);
        }
    }

    if ppm_samples.len() < MIN_SAMPLES_FOR_MEDIAN {
        debug!(
            "Competitor: only {} samples for peer {} (need {}), skipping",
            ppm_samples.len(),
            peer_node_id,
            MIN_SAMPLES_FOR_MEDIAN,
        );
        return None;
    }

    let median_ppm = median(&mut ppm_samples);
    let median_base_msat = median(&mut base_samples);

    debug!(
        "Competitor: peer {} -- median {}ppm, {}msat base ({} samples)",
        peer_node_id,
        median_ppm,
        median_base_msat,
        ppm_samples.len(),
    );

    Some(CompetitorFees {
        median_ppm,
        median_base_msat,
    })
}

fn median(values: &mut [u32]) -> u32 {
    values.sort_unstable();
    let len = values.len();
    if len == 0 {
        return 0;
    }
    if len % 2 == 1 {
        values[len / 2]
    } else {
        (values[len / 2 - 1] + values[len / 2]) / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::mock::MockLdkClient;
    use ldk_server_protos::api::{GraphGetChannelResponse, GraphGetNodeResponse};
    use ldk_server_protos::types::{
        GraphChannel, GraphChannelUpdate, GraphNode, GraphNodeAnnouncement, GraphRoutingFees,
    };

    fn make_graph_node(channel_ids: Vec<u64>) -> GraphGetNodeResponse {
        GraphGetNodeResponse {
            node: Some(GraphNode {
                channels: channel_ids,
                announcement_info: Some(GraphNodeAnnouncement {
                    last_update: 0,
                    alias: String::new(),
                    rgb: String::new(),
                    addresses: vec!["1.2.3.4:9735".to_string()],
                }),
            }),
        }
    }

    fn make_channel_with_fees(
        node_one: &str,
        node_two: &str,
        one_to_two_ppm: u32,
        two_to_one_ppm: u32,
    ) -> GraphGetChannelResponse {
        GraphGetChannelResponse {
            channel: Some(GraphChannel {
                node_one: node_one.to_string(),
                node_two: node_two.to_string(),
                capacity_sats: Some(1_000_000),
                one_to_two: Some(GraphChannelUpdate {
                    last_update: 0,
                    enabled: true,
                    cltv_expiry_delta: 40,
                    htlc_minimum_msat: 1000,
                    htlc_maximum_msat: 500_000_000,
                    fees: Some(GraphRoutingFees {
                        base_msat: 1000,
                        proportional_millionths: one_to_two_ppm,
                    }),
                }),
                two_to_one: Some(GraphChannelUpdate {
                    last_update: 0,
                    enabled: true,
                    cltv_expiry_delta: 40,
                    htlc_minimum_msat: 1000,
                    htlc_maximum_msat: 500_000_000,
                    fees: Some(GraphRoutingFees {
                        base_msat: 1000,
                        proportional_millionths: two_to_one_ppm,
                    }),
                }),
            }),
        }
    }

    #[test]
    fn test_median_odd() {
        assert_eq!(median(&mut [5, 1, 3]), 3);
    }

    #[test]
    fn test_median_even() {
        assert_eq!(median(&mut [1, 2, 3, 4]), 2); // (2+3)/2 = 2 (integer)
    }

    #[test]
    fn test_median_single() {
        assert_eq!(median(&mut [42]), 42);
    }

    #[test]
    fn test_median_empty() {
        assert_eq!(median(&mut []), 0);
    }

    #[tokio::test]
    async fn test_competitor_fees_basic() {
        let mut mock = MockLdkClient::new();
        let own_id = mock.node_info.node_id.clone();
        let peer_id = "peer_node";

        // Peer has 5 channels to competitors
        mock.graph_node_details
            .insert(peer_id.to_string(), make_graph_node(vec![1, 2, 3, 4, 5]));

        // Channels: peer is node_one, so competitor fee toward peer = two_to_one
        // PPMs toward peer: 50, 100, 150, 200, 250 → median = 150
        for (scid, ppm) in [(1, 50), (2, 100), (3, 150), (4, 200), (5, 250)] {
            mock.graph_channel_details.insert(
                scid,
                make_channel_with_fees(
                    peer_id,
                    &format!("competitor_{}", scid),
                    999, // one_to_two: irrelevant (peer→competitor direction)
                    ppm, // two_to_one: competitor→peer (what we care about)
                ),
            );
        }

        let result = get_competitor_fees(&mock, peer_id, &own_id).await;
        assert!(result.is_some());
        let cf = result.unwrap();
        assert_eq!(cf.median_ppm, 150, "Median PPM should be 150");
        assert_eq!(cf.median_base_msat, 1000, "Median base should be 1000");
    }

    #[tokio::test]
    async fn test_competitor_fees_excludes_own_channels() {
        let mut mock = MockLdkClient::new();
        let own_id = mock.node_info.node_id.clone();
        let peer_id = "peer_node";

        // Peer has 5 channels: 2 to us (should be excluded), 3 to competitors
        mock.graph_node_details
            .insert(peer_id.to_string(), make_graph_node(vec![1, 2, 3, 4, 5]));

        // Channels 1,2: our channels (should be excluded)
        mock.graph_channel_details.insert(
            1,
            make_channel_with_fees(peer_id, &own_id, 50, 9999),
        );
        mock.graph_channel_details.insert(
            2,
            make_channel_with_fees(&own_id, peer_id, 9999, 50),
        );

        // Channels 3,4,5: competitor channels with PPMs 100, 200, 300
        for (scid, ppm) in [(3, 100), (4, 200), (5, 300)] {
            mock.graph_channel_details.insert(
                scid,
                make_channel_with_fees(
                    peer_id,
                    &format!("competitor_{}", scid),
                    999,
                    ppm,
                ),
            );
        }

        let result = get_competitor_fees(&mock, peer_id, &own_id).await;
        assert!(result.is_some());
        let cf = result.unwrap();
        assert_eq!(cf.median_ppm, 200, "Should exclude own channels, median = 200");
    }

    #[tokio::test]
    async fn test_competitor_fees_too_few_samples() {
        let mut mock = MockLdkClient::new();
        let own_id = mock.node_info.node_id.clone();
        let peer_id = "peer_node";

        // Peer has only 2 channels (below MIN_SAMPLES_FOR_MEDIAN of 3)
        mock.graph_node_details
            .insert(peer_id.to_string(), make_graph_node(vec![1, 2]));

        for (scid, ppm) in [(1, 100), (2, 200)] {
            mock.graph_channel_details.insert(
                scid,
                make_channel_with_fees(peer_id, &format!("comp_{}", scid), 999, ppm),
            );
        }

        let result = get_competitor_fees(&mock, peer_id, &own_id).await;
        assert!(result.is_none(), "Should return None with < 3 samples");
    }

    #[tokio::test]
    async fn test_competitor_fees_empty_graph() {
        let mock = MockLdkClient::new();
        let result = get_competitor_fees(&mock, "unknown_peer", "own_node").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_competitor_fees_peer_is_node_two() {
        let mut mock = MockLdkClient::new();
        let own_id = mock.node_info.node_id.clone();
        let peer_id = "peer_node";

        // Peer is node_two in all channels, so competitor fee = one_to_two
        mock.graph_node_details
            .insert(peer_id.to_string(), make_graph_node(vec![1, 2, 3]));

        // one_to_two PPMs: 80, 120, 160 → median = 120
        for (scid, ppm) in [(1, 80), (2, 120), (3, 160)] {
            mock.graph_channel_details.insert(
                scid,
                make_channel_with_fees(
                    &format!("competitor_{}", scid),
                    peer_id,  // peer is node_two
                    ppm,      // one_to_two: competitor→peer
                    999,      // two_to_one: irrelevant
                ),
            );
        }

        let result = get_competitor_fees(&mock, peer_id, &own_id).await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().median_ppm, 120);
    }
}
