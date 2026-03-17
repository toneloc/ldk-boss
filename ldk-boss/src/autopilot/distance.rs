/// Port of CLBoss ChannelFinderByDistance.
///
/// Uses a bounded Dijkstra shortest-path algorithm over the gossip graph
/// to find structurally distant nodes. Distant/leaf nodes in the shortest-path
/// tree represent underserved areas of the network — opening channels to them
/// improves overall network connectivity and routing diversity.
///
/// Algorithm:
/// 1. Start Dijkstra from our node with cost=0
/// 2. For each frontier node, fetch its channels from the graph
/// 3. Edge cost = base_fee + (prop_fee × reference_amount) + (delay × cost_per_block)
/// 4. Explore up to MAX_NODES nodes (bounded to limit API calls)
/// 5. Extract leaf nodes from the shortest-path tree
/// 6. Return the most distant leaves as candidates (weighted by cost)
///
/// Reference: clboss/Boss/Mod/ChannelFinderByDistance.cpp

use crate::autopilot::candidate::{resolve_node_address, Candidate, CandidateSource};
use crate::client::LdkClient;
use ldk_server_protos::api::{GraphGetChannelRequest, GraphGetNodeRequest};
use log::{debug, info};
use rand::seq::SliceRandom;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

// --- CLBoss cost function constants ---

/// Reference payment amount for fee calculation (10,000 sat = 10M msat).
const REFERENCE_AMOUNT_MSAT: f64 = 10_000_000.0;
/// Cost per block of CLTV delay (in msat).
const MSAT_PER_BLOCK: f64 = 1.0;
/// Maximum edge cost before skipping (50 sat = 50,000 msat, i.e., 0.5% of reference).
const MAX_EDGE_COST_MSAT: f64 = 50_000.0;

// --- Exploration budget ---

/// Maximum nodes to explore in the Dijkstra traversal.
const MAX_NODES_TO_EXPLORE: usize = 50;
/// Maximum channels to sample per node during exploration.
const MAX_CHANNELS_PER_NODE: usize = 5;
/// Maximum candidates to return from distance finder.
const MAX_DISTANCE_CANDIDATES: usize = 10;

/// Entry in the Dijkstra priority queue.
struct QueueEntry {
    cost: f64,
    node_id: String,
}

impl Eq for QueueEntry {}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.node_id == other.node_id
    }
}

// Reverse ordering for min-heap (BinaryHeap is a max-heap by default).
impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cost
            .partial_cmp(&self.cost)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.node_id.cmp(&other.node_id))
    }
}

impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A node in the shortest-path tree.
struct PathNode {
    cost: f64,
    parent: Option<String>,
}

/// Run a bounded Dijkstra over the gossip graph and return distance-based
/// channel candidates.
pub async fn get_distance_candidates(
    client: &impl LdkClient,
    own_node_id: &str,
    existing_peers: &HashSet<String>,
) -> anyhow::Result<Vec<Candidate>> {
    let tree = run_dijkstra(client, own_node_id).await?;

    if tree.len() <= 1 {
        debug!("Distance: Dijkstra tree has only root, no candidates");
        return Ok(Vec::new());
    }

    // Find leaf nodes: nodes that are not anyone's parent and are not root
    // Also exclude direct neighbors of root (they're too close)
    let parent_set: HashSet<&str> = tree
        .values()
        .filter_map(|n| n.parent.as_deref())
        .collect();

    let mut leaves: Vec<(String, f64)> = tree
        .iter()
        .filter(|(id, node)| {
            id.as_str() != own_node_id // Not root
                && !parent_set.contains(id.as_str()) // Has no children (leaf)
                && node.parent.as_deref() != Some(own_node_id) // Not direct neighbor
        })
        .map(|(id, node)| (id.clone(), node.cost))
        .collect();

    if leaves.is_empty() {
        debug!("Distance: no leaf nodes found (all explored nodes have children)");
        return Ok(Vec::new());
    }

    // Sort by cost descending (most distant first), matching CLBoss behavior
    leaves.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));

    info!(
        "Distance: found {} leaf nodes, max cost {:.0} msat",
        leaves.len(),
        leaves.first().map(|(_, c)| *c).unwrap_or(0.0),
    );

    // Take the most distant leaves and resolve their addresses
    let mut candidates = Vec::new();
    let max_cost = leaves.first().map(|(_, c)| *c).unwrap_or(1.0);

    for (node_id, cost) in leaves.into_iter().take(MAX_DISTANCE_CANDIDATES * 2) {
        if existing_peers.contains(&node_id) {
            continue;
        }

        if let Some(address) = resolve_node_address(client, &node_id).await {
            // Score: proportional to distance (more distant = higher score)
            // Normalize to [15, 40] range
            let normalized = if max_cost > 0.0 {
                cost / max_cost
            } else {
                0.5
            };
            let score = 15.0 + normalized * 25.0;

            candidates.push(Candidate {
                node_id,
                address,
                score,
                source: CandidateSource::GraphDistance,
            });

            if candidates.len() >= MAX_DISTANCE_CANDIDATES {
                break;
            }
        }
    }

    info!(
        "Distance: returning {} distance-based candidates",
        candidates.len()
    );
    Ok(candidates)
}

/// Run bounded Dijkstra from the given root node over the gossip graph.
///
/// Returns a shortest-path tree as a map of node_id → PathNode.
async fn run_dijkstra(
    client: &impl LdkClient,
    root_id: &str,
) -> anyhow::Result<HashMap<String, PathNode>> {
    let mut tree: HashMap<String, PathNode> = HashMap::new();
    let mut heap: BinaryHeap<QueueEntry> = BinaryHeap::new();
    let mut closed: HashSet<String> = HashSet::new();
    let mut rng = rand::thread_rng();

    // Initialize with root
    tree.insert(
        root_id.to_string(),
        PathNode {
            cost: 0.0,
            parent: None,
        },
    );
    heap.push(QueueEntry {
        cost: 0.0,
        node_id: root_id.to_string(),
    });

    while let Some(current) = heap.pop() {
        // Skip already-closed nodes (stale entries in heap)
        if closed.contains(&current.node_id) {
            continue;
        }
        closed.insert(current.node_id.clone());

        if closed.len() >= MAX_NODES_TO_EXPLORE {
            debug!(
                "Distance: exploration budget exhausted ({} nodes)",
                closed.len()
            );
            break;
        }

        // Get current node's channels from graph
        let node_resp = match client
            .graph_get_node(GraphGetNodeRequest {
                node_id: current.node_id.clone(),
            })
            .await
        {
            Ok(r) => r,
            Err(e) => {
                debug!(
                    "Distance: graph_get_node failed for {}: {}",
                    current.node_id, e
                );
                continue;
            }
        };

        let node = match node_resp.node {
            Some(n) => n,
            None => continue,
        };

        // Sample a subset of channels to limit API calls
        let mut channel_ids = node.channels;
        channel_ids.shuffle(&mut rng);
        channel_ids.truncate(MAX_CHANNELS_PER_NODE);

        for scid in channel_ids {
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

            // Find the neighbor (other end of the channel)
            let neighbor_id = if ch.node_one == current.node_id {
                ch.node_two.clone()
            } else if ch.node_two == current.node_id {
                ch.node_one.clone()
            } else {
                // Channel doesn't involve current node (shouldn't happen)
                continue;
            };

            // Skip already-closed nodes
            if closed.contains(&neighbor_id) {
                continue;
            }

            // Compute edge cost using CLBoss formula
            let edge_cost = match compute_edge_cost(&ch, &current.node_id) {
                Some(c) => c,
                None => continue, // Disabled channel or cost too high
            };

            let total_cost = current.cost + edge_cost;

            // Check if this is a new or better path
            let dominated = tree
                .get(&neighbor_id)
                .map_or(false, |existing| total_cost >= existing.cost);

            if !dominated {
                tree.insert(
                    neighbor_id.clone(),
                    PathNode {
                        cost: total_cost,
                        parent: Some(current.node_id.clone()),
                    },
                );
                heap.push(QueueEntry {
                    cost: total_cost,
                    node_id: neighbor_id,
                });
            }
        }
    }

    debug!(
        "Distance: Dijkstra explored {} nodes, tree has {} entries",
        closed.len(),
        tree.len()
    );
    Ok(tree)
}

/// Compute the edge cost for traversing a channel in a given direction.
///
/// Cost = base_fee + (proportional_fee × reference_amount) + (delay × msat_per_block)
///
/// Returns None if the channel is disabled in this direction or cost exceeds max.
fn compute_edge_cost(
    ch: &ldk_server_protos::types::GraphChannel,
    from_node_id: &str,
) -> Option<f64> {
    // Determine which direction we're traversing
    let update = if ch.node_one == from_node_id {
        // Traversing from node_one to node_two: use one_to_two update
        ch.one_to_two.as_ref()
    } else {
        // Traversing from node_two to node_one: use two_to_one update
        ch.two_to_one.as_ref()
    };

    let update = match update {
        Some(u) if u.enabled => u,
        _ => return None, // Disabled or missing
    };

    let fees = update.fees.as_ref()?;

    let base_fee = fees.base_msat as f64;
    let prop_fee = (fees.proportional_millionths as f64 / 1_000_000.0) * REFERENCE_AMOUNT_MSAT;
    let delay_cost = update.cltv_expiry_delta as f64 * MSAT_PER_BLOCK;

    let total = base_fee + prop_fee + delay_cost;

    if total > MAX_EDGE_COST_MSAT {
        return None; // Too expensive, skip this edge
    }

    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ldk_server_protos::types::{GraphChannel, GraphChannelUpdate, GraphRoutingFees};

    #[test]
    fn test_compute_edge_cost_typical() {
        let ch = GraphChannel {
            node_one: "A".to_string(),
            node_two: "B".to_string(),
            capacity_sats: Some(1_000_000),
            one_to_two: Some(GraphChannelUpdate {
                last_update: 0,
                enabled: true,
                cltv_expiry_delta: 144,
                htlc_minimum_msat: 1000,
                htlc_maximum_msat: 500_000_000,
                fees: Some(GraphRoutingFees {
                    base_msat: 1000,
                    proportional_millionths: 100,
                }),
            }),
            two_to_one: None,
        };

        let cost = compute_edge_cost(&ch, "A").unwrap();
        // base=1000 + prop=(100/1M)*10M=1000 + delay=144*1=144 = 2144
        assert!(
            (cost - 2144.0).abs() < 0.01,
            "Expected ~2144, got {}",
            cost
        );
    }

    #[test]
    fn test_compute_edge_cost_disabled() {
        let ch = GraphChannel {
            node_one: "A".to_string(),
            node_two: "B".to_string(),
            capacity_sats: Some(1_000_000),
            one_to_two: Some(GraphChannelUpdate {
                last_update: 0,
                enabled: false,
                cltv_expiry_delta: 40,
                htlc_minimum_msat: 1000,
                htlc_maximum_msat: 500_000_000,
                fees: Some(GraphRoutingFees {
                    base_msat: 1000,
                    proportional_millionths: 100,
                }),
            }),
            two_to_one: None,
        };

        assert!(compute_edge_cost(&ch, "A").is_none());
    }

    #[test]
    fn test_compute_edge_cost_too_expensive() {
        let ch = GraphChannel {
            node_one: "A".to_string(),
            node_two: "B".to_string(),
            capacity_sats: Some(1_000_000),
            one_to_two: Some(GraphChannelUpdate {
                last_update: 0,
                enabled: true,
                cltv_expiry_delta: 40,
                htlc_minimum_msat: 1000,
                htlc_maximum_msat: 500_000_000,
                fees: Some(GraphRoutingFees {
                    base_msat: 50_000, // 50 sat base fee alone
                    proportional_millionths: 5000, // + 0.5% of 10K = 50 sat
                }),
            }),
            two_to_one: None,
        };

        // base=50000 + prop=(5000/1M)*10M=50000 + delay=40 = 100040 > 50000
        assert!(compute_edge_cost(&ch, "A").is_none());
    }

    #[test]
    fn test_compute_edge_cost_reverse_direction() {
        let ch = GraphChannel {
            node_one: "A".to_string(),
            node_two: "B".to_string(),
            capacity_sats: Some(1_000_000),
            one_to_two: None,
            two_to_one: Some(GraphChannelUpdate {
                last_update: 0,
                enabled: true,
                cltv_expiry_delta: 40,
                htlc_minimum_msat: 1000,
                htlc_maximum_msat: 500_000_000,
                fees: Some(GraphRoutingFees {
                    base_msat: 500,
                    proportional_millionths: 50,
                }),
            }),
        };

        // From B→A: uses two_to_one
        let cost = compute_edge_cost(&ch, "B").unwrap();
        // base=500 + prop=(50/1M)*10M=500 + delay=40 = 1040
        assert!((cost - 1040.0).abs() < 0.01, "Expected ~1040, got {}", cost);

        // From A→B: one_to_two is None → disabled
        assert!(compute_edge_cost(&ch, "A").is_none());
    }

    #[tokio::test]
    async fn test_dijkstra_simple_graph() {
        use crate::client::mock::MockLdkClient;
        use ldk_server_protos::api::GraphGetNodeResponse;
        use ldk_server_protos::types::GraphNode;

        let mut mock = MockLdkClient::new();
        let own_id = mock.node_info.node_id.clone();

        // Build a simple graph:
        // own → A (cost 1000) → B (cost 1000) → C (cost 1000)
        //                      → D (cost 2000)
        // Leaves should be C and D

        // Own node channels
        mock.graph_node_details.insert(
            own_id.clone(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![100],
                    announcement_info: None,
                }),
            },
        );

        // Channel 100: own ↔ A
        mock.graph_channel_details.insert(
            100,
            make_test_channel(&own_id, "A", 1000, 0, 0),
        );

        // Node A channels
        mock.graph_node_details.insert(
            "A".to_string(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![100, 101, 102],
                    announcement_info: None,
                }),
            },
        );

        // Channel 101: A ↔ B
        mock.graph_channel_details.insert(
            101,
            make_test_channel("A", "B", 1000, 0, 0),
        );
        // Channel 102: A ↔ D
        mock.graph_channel_details.insert(
            102,
            make_test_channel("A", "D", 2000, 0, 0),
        );

        // Node B channels
        mock.graph_node_details.insert(
            "B".to_string(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![101, 103],
                    announcement_info: None,
                }),
            },
        );

        // Channel 103: B ↔ C
        mock.graph_channel_details.insert(
            103,
            make_test_channel("B", "C", 1000, 0, 0),
        );

        // Node C (leaf)
        mock.graph_node_details.insert(
            "C".to_string(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![103],
                    announcement_info: None,
                }),
            },
        );

        // Node D (leaf)
        mock.graph_node_details.insert(
            "D".to_string(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![102],
                    announcement_info: None,
                }),
            },
        );

        let tree = run_dijkstra(&mock, &own_id).await.unwrap();

        // Should have 5 nodes: own, A, B, C, D
        assert_eq!(tree.len(), 5, "Tree should have 5 nodes, got {}", tree.len());

        // A is a direct neighbor (cost 1000)
        assert!(
            (tree["A"].cost - 1000.0).abs() < 0.01,
            "A cost should be ~1000, got {}",
            tree["A"].cost
        );

        // B is at cost 2000 (via A)
        assert!(
            (tree["B"].cost - 2000.0).abs() < 0.01,
            "B cost should be ~2000, got {}",
            tree["B"].cost
        );

        // C is at cost 3000 (via A→B)
        assert!(
            (tree["C"].cost - 3000.0).abs() < 0.01,
            "C cost should be ~3000, got {}",
            tree["C"].cost
        );

        // D is at cost 3000 (via A, edge cost 2000)
        assert!(
            (tree["D"].cost - 3000.0).abs() < 0.01,
            "D cost should be ~3000, got {}",
            tree["D"].cost
        );

        // C and D should be leaves (not parents of anyone, not direct neighbor of root)
        let parent_set: HashSet<&str> = tree
            .values()
            .filter_map(|n| n.parent.as_deref())
            .collect();

        assert!(!parent_set.contains("C"), "C should be a leaf");
        assert!(!parent_set.contains("D"), "D should be a leaf");
    }

    #[tokio::test]
    async fn test_get_distance_candidates() {
        use crate::client::mock::MockLdkClient;
        use ldk_server_protos::api::GraphGetNodeResponse;
        use ldk_server_protos::types::{GraphNode, GraphNodeAnnouncement};

        let mut mock = MockLdkClient::new();
        let own_id = mock.node_info.node_id.clone();

        // Simple chain: own → A → B → C
        mock.graph_node_details.insert(
            own_id.clone(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![100],
                    announcement_info: None,
                }),
            },
        );

        mock.graph_channel_details.insert(
            100,
            make_test_channel(&own_id, "A", 1000, 0, 0),
        );

        mock.graph_node_details.insert(
            "A".to_string(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![100, 101],
                    announcement_info: Some(GraphNodeAnnouncement {
                        last_update: 0,
                        alias: "Node A".to_string(),
                        rgb: String::new(),
                        addresses: vec!["1.1.1.1:9735".to_string()],
                    }),
                }),
            },
        );

        mock.graph_channel_details.insert(
            101,
            make_test_channel("A", "B", 1000, 0, 0),
        );

        mock.graph_node_details.insert(
            "B".to_string(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![101, 102],
                    announcement_info: Some(GraphNodeAnnouncement {
                        last_update: 0,
                        alias: "Node B".to_string(),
                        rgb: String::new(),
                        addresses: vec!["2.2.2.2:9735".to_string()],
                    }),
                }),
            },
        );

        mock.graph_channel_details.insert(
            102,
            make_test_channel("B", "C", 1000, 0, 0),
        );

        mock.graph_node_details.insert(
            "C".to_string(),
            GraphGetNodeResponse {
                node: Some(GraphNode {
                    channels: vec![102],
                    announcement_info: Some(GraphNodeAnnouncement {
                        last_update: 0,
                        alias: "Node C".to_string(),
                        rgb: String::new(),
                        addresses: vec!["3.3.3.3:9735".to_string()],
                    }),
                }),
            },
        );

        let existing_peers = HashSet::new();
        let candidates = get_distance_candidates(&mock, &own_id, &existing_peers)
            .await
            .unwrap();

        // C should be a candidate (leaf, not direct neighbor)
        // A is excluded (direct neighbor of root)
        // B might be excluded if it has children
        assert!(
            !candidates.is_empty(),
            "Should find at least one distance candidate"
        );
        assert!(
            candidates.iter().any(|c| c.node_id == "C"),
            "C should be a candidate (most distant leaf). Got: {:?}",
            candidates.iter().map(|c| &c.node_id).collect::<Vec<_>>()
        );
        // Verify all candidates have the right source
        for c in &candidates {
            assert!(matches!(c.source, CandidateSource::GraphDistance));
            assert!(!c.address.is_empty());
        }
    }

    #[tokio::test]
    async fn test_distance_candidates_empty_graph() {
        use crate::client::mock::MockLdkClient;

        let mock = MockLdkClient::new();
        let existing_peers = HashSet::new();
        let candidates = get_distance_candidates(&mock, "own_node", &existing_peers)
            .await
            .unwrap();
        assert!(candidates.is_empty());
    }

    /// Helper to create a test channel with bidirectional fees.
    fn make_test_channel(
        node_one: &str,
        node_two: &str,
        base_msat: u32,
        prop_millionths: u32,
        cltv_delta: u32,
    ) -> ldk_server_protos::api::GraphGetChannelResponse {
        let update = GraphChannelUpdate {
            last_update: 0,
            enabled: true,
            cltv_expiry_delta: cltv_delta,
            htlc_minimum_msat: 1000,
            htlc_maximum_msat: 500_000_000,
            fees: Some(GraphRoutingFees {
                base_msat,
                proportional_millionths: prop_millionths,
            }),
        };
        ldk_server_protos::api::GraphGetChannelResponse {
            channel: Some(GraphChannel {
                node_one: node_one.to_string(),
                node_two: node_two.to_string(),
                capacity_sats: Some(1_000_000),
                one_to_two: Some(update.clone()),
                two_to_one: Some(update),
            }),
        }
    }
}
