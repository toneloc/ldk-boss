use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use ldk_server_protos::api::GraphGetNodeRequest;
use ldk_server_protos::api::GraphGetChannelRequest;
use log::{debug, info, warn};
use rand::seq::SliceRandom;
use std::collections::HashSet;

/// A candidate node to open a channel with.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub node_id: String,
    pub address: String,
    pub score: f64,
    pub source: CandidateSource,
}

#[derive(Debug, Clone)]
pub enum CandidateSource {
    Hardcoded,
    SeedNode,
    Earnings,
    External,
    GraphPopularity,
    GraphPeerOfEarner,
    GraphDistance,
}

/// Well-known, highly-connected Lightning routing nodes.
/// These serve as a fallback when no external ranking API is configured.
pub const HARDCODED_NODES: &[(&str, &str)] = &[
    // ACINQ
    (
        "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
        "3.33.236.230:9735",
    ),
    // Kraken
    (
        "02f1a8c87607f415c8f22c00571c93e301a0ab6e73e38bfa3eb97ee71f96aab5f6",
        "52.13.118.208:9735",
    ),
    // River Financial
    (
        "03037dc08e9ac63b82581f79b662a4d0ceca8a8ca162b1af3551595b8f2d97b70a",
        "104.196.249.140:9735",
    ),
    // Wallet of Satoshi
    (
        "035e4ff418fc8b5554c5d9eea66396c227bd3a1a07c54c2b7b8d8dfdfc0e0a941b",
        "170.75.163.209:9735",
    ),
    // Bitfinex
    (
        "033d8656219478701227199cbd6f670335c8d408a92ae88b962c49d4dc0e83e025",
        "3.33.236.230:9735",
    ),
    // OpenNode
    (
        "028d98b9969fbed53784a36617eb489a59ab6dc9b9d77571a4a3e5cba4a0c71284",
        "18.221.23.28:9735",
    ),
    // Fold
    (
        "02816caed43171d3c9854e3b0ab2dee0a029c7290e2dd04cf4a68df1e8a0586cac",
        "35.238.153.25:9735",
    ),
    // Boltz
    (
        "026165850492521f4ac8abd9bd8088123446d126f648ca35e60f88177dc149ceb2",
        "24.249.146.89:9735",
    ),
    // Zero Fee Routing
    (
        "038863cf8ab91046230f561cd5b386cbff8309fa02e3f0c3ed161a3aeb64a643b9",
        "203.132.95.10:9735",
    ),
    // LNBig
    (
        "0331f80652fb840239df8dc99205792bba2e559a05469915804c08420230e23c7c",
        "138.68.14.104:9735",
    ),
];

/// Maximum nodes to sample for popularity discovery.
const POPULARITY_SAMPLE_SIZE: usize = 50;
/// How many top-degree nodes to extract peers from.
const POPULARITY_TOP_N: usize = 3;
/// How many channels to sample per popular/earner node.
const CHANNELS_PER_NODE_SAMPLE: usize = 5;
/// How many top earners to consider.
const TOP_EARNERS_COUNT: usize = 5;
/// Earnings lookback window in seconds (30 days).
const EARNINGS_LOOKBACK_SECS: i64 = 30 * 86400;

/// Get a ranked list of channel candidates.
pub async fn get_candidates(
    config: &Config,
    client: &impl LdkClient,
    db: &Database,
    existing_peers: &HashSet<String>,
) -> anyhow::Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    let own_node_id = client
        .get_node_info()
        .await
        .map(|info| info.node_id)
        .unwrap_or_default();

    // Source 1: User-configured seed nodes
    for seed in &config.autopilot.seed_nodes {
        if let Some((node_id, address)) = parse_node_address(seed) {
            if !existing_peers.contains(&node_id) && !is_blacklisted(config, &node_id) {
                candidates.push(Candidate {
                    node_id,
                    address,
                    score: 100.0, // Highest priority
                    source: CandidateSource::SeedNode,
                });
            }
        }
    }

    // Source 2: Peers of our top-earning counterparties (graph-based)
    match get_earnings_candidates(client, db, existing_peers, &own_node_id).await {
        Ok(earner_candidates) => {
            for c in earner_candidates {
                if !is_blacklisted(config, &c.node_id)
                    && !candidates.iter().any(|e| e.node_id == c.node_id)
                {
                    candidates.push(c);
                }
            }
        }
        Err(e) => {
            warn!("Graph earnings candidate discovery failed: {}", e);
        }
    }

    // Source 3: Popular nodes from gossip graph
    match get_popularity_candidates(client, existing_peers, &own_node_id).await {
        Ok(pop_candidates) => {
            for c in pop_candidates {
                if !is_blacklisted(config, &c.node_id)
                    && !candidates.iter().any(|e| e.node_id == c.node_id)
                {
                    candidates.push(c);
                }
            }
        }
        Err(e) => {
            warn!("Graph popularity candidate discovery failed: {}", e);
        }
    }

    // Source 4: Distance-based candidates (Dijkstra over gossip graph)
    match super::distance::get_distance_candidates(client, &own_node_id, &existing_peers).await {
        Ok(dist_candidates) => {
            for c in dist_candidates {
                if !is_blacklisted(config, &c.node_id)
                    && !candidates.iter().any(|e| e.node_id == c.node_id)
                {
                    candidates.push(c);
                }
            }
        }
        Err(e) => {
            warn!("Graph distance candidate discovery failed: {}", e);
        }
    }

    // Source 5: External ranking API (if configured)
    if !config.autopilot.ranking_api_url.is_empty() {
        match fetch_external_candidates(&config.autopilot.ranking_api_url).await {
            Ok(external) => {
                for c in external {
                    if !existing_peers.contains(&c.node_id)
                        && !is_blacklisted(config, &c.node_id)
                        && !candidates.iter().any(|e| e.node_id == c.node_id)
                    {
                        candidates.push(c);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to fetch external candidates: {}", e);
            }
        }
    }

    // Source 6: Hardcoded well-known nodes
    for (node_id, address) in HARDCODED_NODES {
        let node_id = node_id.to_string();
        if !existing_peers.contains(&node_id)
            && !is_blacklisted(config, &node_id)
            && !candidates.iter().any(|c| c.node_id == node_id)
        {
            candidates.push(Candidate {
                node_id,
                address: address.to_string(),
                score: 10.0,
                source: CandidateSource::Hardcoded,
            });
        }
    }

    // Sort by score descending
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    debug!("Autopilot: {} candidates available", candidates.len());

    Ok(candidates)
}

/// Find peers of our highest-earning counterparties via the gossip graph.
///
/// Port of CLBoss `ChannelFinderByEarnedFee`: finds the peers with the highest
/// outgoing fee earnings, then proposes their graph neighbors as candidates.
async fn get_earnings_candidates(
    client: &impl LdkClient,
    db: &Database,
    existing_peers: &HashSet<String>,
    own_node_id: &str,
) -> anyhow::Result<Vec<Candidate>> {
    let since = chrono::Utc::now().timestamp() - EARNINGS_LOOKBACK_SECS;
    let since_bucket = since - (since % 86400);

    // Query top earners by outgoing fee (direction='out' means we forwarded through them)
    let top_earners: Vec<(String, i64)> = {
        let conn = db.conn();
        let mut stmt = conn.prepare(
            "SELECT counterparty_node_id, SUM(fee_earned_msat) as total_fee \
             FROM earnings \
             WHERE day_bucket >= ?1 AND direction = 'out' \
             GROUP BY counterparty_node_id \
             ORDER BY total_fee DESC \
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![since_bucket, TOP_EARNERS_COUNT as i64],
            |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            },
        )?;
        rows.filter_map(|r| r.ok())
            .filter(|(node_id, fee)| *fee > 0 && !existing_peers.contains(node_id.as_str()))
            .collect()
    };

    if top_earners.is_empty() {
        debug!("Autopilot: no earning peers found for graph discovery");
        return Ok(Vec::new());
    }

    info!(
        "Autopilot: discovering candidates via {} top-earning peers",
        top_earners.len()
    );

    let mut candidates = Vec::new();
    let mut rng = rand::thread_rng();

    for (rank, (earner_id, _fee)) in top_earners.iter().enumerate() {
        // Get the earner's channels from the graph
        let node_resp = match client
            .graph_get_node(GraphGetNodeRequest {
                node_id: earner_id.clone(),
            })
            .await
        {
            Ok(r) => r,
            Err(e) => {
                debug!("Autopilot: graph_get_node failed for earner {}: {}", earner_id, e);
                continue;
            }
        };

        let node = match node_resp.node {
            Some(n) => n,
            None => continue,
        };

        // Sample some of their channels to find peers
        let mut channel_ids = node.channels.clone();
        channel_ids.shuffle(&mut rng);
        let sample: Vec<u64> = channel_ids.into_iter().take(CHANNELS_PER_NODE_SAMPLE).collect();

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

            // The peer of our earner is the other end of this channel
            let peer_id = if ch.node_one == *earner_id {
                &ch.node_two
            } else {
                &ch.node_one
            };

            if peer_id == own_node_id
                || existing_peers.contains(peer_id)
                || candidates.iter().any(|c: &Candidate| c.node_id == *peer_id)
            {
                continue;
            }

            // Get the peer's address from graph announcement
            if let Some(address) = resolve_node_address(client, peer_id).await {
                // Score: 50.0 for rank 0, decreasing for lower ranks
                let score = 50.0 - (rank as f64 * 5.0);
                candidates.push(Candidate {
                    node_id: peer_id.to_string(),
                    address,
                    score: score.max(30.0),
                    source: CandidateSource::GraphPeerOfEarner,
                });
            }
        }
    }

    info!(
        "Autopilot: found {} earnings-based graph candidates",
        candidates.len()
    );
    Ok(candidates)
}

/// Find high-degree hub nodes in the gossip graph.
///
/// Adapted from CLBoss `ChannelFinderByPopularity`: samples random nodes,
/// measures their degree (channel count), and returns peers of the most
/// well-connected nodes as candidates.
async fn get_popularity_candidates(
    client: &impl LdkClient,
    existing_peers: &HashSet<String>,
    own_node_id: &str,
) -> anyhow::Result<Vec<Candidate>> {
    // Step 1: Get all node IDs
    let all_nodes = client.graph_list_nodes().await?;
    if all_nodes.node_ids.is_empty() {
        debug!("Autopilot: gossip graph is empty");
        return Ok(Vec::new());
    }

    info!(
        "Autopilot: sampling from {} gossip graph nodes",
        all_nodes.node_ids.len()
    );

    // Step 2: Random sample
    let mut rng = rand::thread_rng();
    let mut sampled_ids = all_nodes.node_ids.clone();
    sampled_ids.shuffle(&mut rng);
    sampled_ids.truncate(POPULARITY_SAMPLE_SIZE);

    // Step 3: Measure degree for each sampled node
    let mut node_degrees: Vec<(String, usize)> = Vec::new();
    for node_id in &sampled_ids {
        if node_id == own_node_id {
            continue;
        }
        let resp = match client
            .graph_get_node(GraphGetNodeRequest {
                node_id: node_id.clone(),
            })
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let Some(node) = resp.node {
            node_degrees.push((node_id.clone(), node.channels.len()));
        }
    }

    // Step 4: Sort by degree, take top N
    node_degrees.sort_by(|a, b| b.1.cmp(&a.1));
    let top_nodes: Vec<_> = node_degrees.into_iter().take(POPULARITY_TOP_N).collect();

    if top_nodes.is_empty() {
        return Ok(Vec::new());
    }

    let max_degree = top_nodes[0].1 as f64;

    // Step 5: For each popular node, find their peers as candidates
    let mut candidates = Vec::new();

    for (popular_id, degree) in &top_nodes {
        // The popular node itself is also a candidate
        if !existing_peers.contains(popular_id)
            && popular_id != own_node_id
            && !candidates.iter().any(|c: &Candidate| c.node_id == *popular_id)
        {
            if let Some(address) = resolve_node_address(client, popular_id).await {
                let score = 30.0 * (*degree as f64 / max_degree);
                candidates.push(Candidate {
                    node_id: popular_id.clone(),
                    address,
                    score: score.max(15.0),
                    source: CandidateSource::GraphPopularity,
                });
            }
        }

        // Get some of their peers
        let resp = match client
            .graph_get_node(GraphGetNodeRequest {
                node_id: popular_id.clone(),
            })
            .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };

        let node = match resp.node {
            Some(n) => n,
            None => continue,
        };

        let mut channel_ids = node.channels.clone();
        channel_ids.shuffle(&mut rng);

        for scid in channel_ids.into_iter().take(CHANNELS_PER_NODE_SAMPLE) {
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

            let peer_id = if ch.node_one == *popular_id {
                &ch.node_two
            } else {
                &ch.node_one
            };

            if peer_id == own_node_id
                || existing_peers.contains(peer_id)
                || candidates.iter().any(|c: &Candidate| c.node_id == *peer_id)
            {
                continue;
            }

            if let Some(address) = resolve_node_address(client, peer_id).await {
                // Peers of popular nodes get slightly lower score than the hub itself
                let score = 25.0 * (*degree as f64 / max_degree);
                candidates.push(Candidate {
                    node_id: peer_id.to_string(),
                    address,
                    score: score.max(12.0),
                    source: CandidateSource::GraphPopularity,
                });
            }
        }
    }

    info!(
        "Autopilot: found {} popularity-based graph candidates",
        candidates.len()
    );
    Ok(candidates)
}

/// Resolve a node's reachable address from its gossip graph announcement.
pub async fn resolve_node_address(client: &impl LdkClient, node_id: &str) -> Option<String> {
    let resp = client
        .graph_get_node(GraphGetNodeRequest {
            node_id: node_id.to_string(),
        })
        .await
        .ok()?;
    let node = resp.node?;
    let ann = node.announcement_info?;
    ann.addresses.into_iter().next()
}

fn is_blacklisted(config: &Config, node_id: &str) -> bool {
    config.autopilot.blacklist.iter().any(|b| b == node_id)
}

pub fn parse_node_address(s: &str) -> Option<(String, String)> {
    // Format: node_id@host:port
    let parts: Vec<&str> = s.splitn(2, '@').collect();
    if parts.len() == 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

async fn fetch_external_candidates(_url: &str) -> anyhow::Result<Vec<Candidate>> {
    // External ranking API integration is not yet implemented.
    // Could integrate with 1ML, Amboss, or a custom ranking service.
    warn!("External ranking API is not yet implemented; ranking_api_url config is ignored");
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::mock::MockLdkClient;
    use crate::config::Config;
    use ldk_server_protos::api::{GraphGetNodeResponse, GraphGetChannelResponse};
    use ldk_server_protos::types::{GraphNode, GraphNodeAnnouncement, GraphChannel};

    fn test_config() -> Config {
        Config::test_default(std::path::PathBuf::from("/dev/null"))
    }

    /// Helper to create a mock graph node with a given number of channels and an address.
    fn make_graph_node(channel_ids: Vec<u64>, address: &str) -> GraphGetNodeResponse {
        GraphGetNodeResponse {
            node: Some(GraphNode {
                channels: channel_ids,
                announcement_info: Some(GraphNodeAnnouncement {
                    last_update: 0,
                    alias: String::new(),
                    rgb: String::new(),
                    addresses: vec![address.to_string()],
                }),
            }),
        }
    }

    /// Helper to create a mock graph channel between two nodes.
    fn make_graph_channel(node_one: &str, node_two: &str) -> GraphGetChannelResponse {
        GraphGetChannelResponse {
            channel: Some(GraphChannel {
                node_one: node_one.to_string(),
                node_two: node_two.to_string(),
                capacity_sats: Some(1_000_000),
                one_to_two: None,
                two_to_one: None,
            }),
        }
    }

    #[test]
    fn test_parse_node_address_valid() {
        let result = parse_node_address("03abc123@1.2.3.4:9735");
        assert_eq!(
            result,
            Some(("03abc123".to_string(), "1.2.3.4:9735".to_string()))
        );
    }

    #[test]
    fn test_parse_node_address_no_at() {
        assert_eq!(parse_node_address("03abc123"), None);
    }

    #[test]
    fn test_parse_node_address_empty() {
        assert_eq!(parse_node_address(""), None);
    }

    #[test]
    fn test_parse_node_address_with_onion() {
        let result = parse_node_address("03abc@nodehost.onion:9735");
        assert_eq!(
            result,
            Some(("03abc".to_string(), "nodehost.onion:9735".to_string()))
        );
    }

    #[test]
    fn test_is_blacklisted() {
        let mut config = test_config();
        config.autopilot.blacklist = vec!["badnode123".to_string()];
        assert!(is_blacklisted(&config, "badnode123"));
        assert!(!is_blacklisted(&config, "goodnode456"));
    }

    #[test]
    fn test_is_blacklisted_empty() {
        let config = test_config();
        assert!(!is_blacklisted(&config, "anynode"));
    }

    #[tokio::test]
    async fn test_popularity_candidates_with_graph() {
        let mut mock = MockLdkClient::new();

        let own_id = mock.node_info.node_id.clone();

        // Set up graph: 5 nodes with varying channel counts
        // hub_a has 10 channels, hub_b has 5, others have 1-2
        mock.graph_nodes.node_ids = vec![
            "hub_a".to_string(),
            "hub_b".to_string(),
            "leaf_1".to_string(),
            "leaf_2".to_string(),
            "leaf_3".to_string(),
        ];

        // hub_a: 10 channels
        mock.graph_node_details.insert(
            "hub_a".to_string(),
            make_graph_node((100..110).collect(), "1.1.1.1:9735"),
        );
        // hub_b: 5 channels
        mock.graph_node_details.insert(
            "hub_b".to_string(),
            make_graph_node((200..205).collect(), "2.2.2.2:9735"),
        );
        // leaf nodes: 1 channel each
        mock.graph_node_details.insert(
            "leaf_1".to_string(),
            make_graph_node(vec![100], "3.3.3.3:9735"),
        );
        mock.graph_node_details.insert(
            "leaf_2".to_string(),
            make_graph_node(vec![201], "4.4.4.4:9735"),
        );
        mock.graph_node_details.insert(
            "leaf_3".to_string(),
            make_graph_node(vec![102], "5.5.5.5:9735"),
        );

        // Channel 100: hub_a <-> leaf_1
        mock.graph_channel_details
            .insert(100, make_graph_channel("hub_a", "leaf_1"));
        // Channel 101: hub_a <-> leaf_3
        mock.graph_channel_details
            .insert(101, make_graph_channel("hub_a", "leaf_3"));
        // More hub_a channels to other nodes
        for scid in 102..110 {
            mock.graph_channel_details.insert(
                scid,
                make_graph_channel("hub_a", &format!("remote_{}", scid)),
            );
            mock.graph_node_details.insert(
                format!("remote_{}", scid),
                make_graph_node(vec![scid], &format!("10.0.0.{}:9735", scid)),
            );
        }
        // hub_b channels
        for scid in 200..205 {
            mock.graph_channel_details.insert(
                scid,
                make_graph_channel("hub_b", &format!("remote_{}", scid)),
            );
            mock.graph_node_details.insert(
                format!("remote_{}", scid),
                make_graph_node(vec![scid], &format!("10.1.0.{}:9735", scid)),
            );
        }

        let existing_peers = HashSet::new();
        let candidates =
            get_popularity_candidates(&mock, &existing_peers, &own_id).await.unwrap();

        assert!(
            !candidates.is_empty(),
            "Should find candidates from graph"
        );
        // hub_a should appear (highest degree) or its peers should
        let has_hub_a = candidates.iter().any(|c| c.node_id == "hub_a");
        let has_hub_a_peers = candidates
            .iter()
            .any(|c| c.node_id.starts_with("remote_10") || c.node_id == "leaf_1");
        assert!(
            has_hub_a || has_hub_a_peers,
            "Should discover hub_a or its peers. Got: {:?}",
            candidates.iter().map(|c| &c.node_id).collect::<Vec<_>>()
        );
        // All candidates should have addresses
        for c in &candidates {
            assert!(!c.address.is_empty(), "Candidate {} has no address", c.node_id);
        }
    }

    #[tokio::test]
    async fn test_earnings_candidates_with_graph() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let mut mock = MockLdkClient::new();
        let own_id = mock.node_info.node_id.clone();

        // Insert earnings data: earner_a is our top earner
        let now = chrono::Utc::now().timestamp();
        let bucket = now - (now % 86400);
        db.conn()
            .execute(
                "INSERT INTO earnings (channel_id, counterparty_node_id, day_bucket, \
                 fee_earned_msat, amount_forwarded_msat, direction) \
                 VALUES ('ch_earn', 'earner_a', ?1, 50000, 5000000, 'out')",
                rusqlite::params![bucket],
            )
            .unwrap();

        // Set up graph: earner_a has channels to peer_x and peer_y
        mock.graph_node_details.insert(
            "earner_a".to_string(),
            make_graph_node(vec![500, 501], "9.9.9.9:9735"),
        );
        mock.graph_channel_details
            .insert(500, make_graph_channel("earner_a", "peer_x"));
        mock.graph_channel_details
            .insert(501, make_graph_channel("earner_a", "peer_y"));
        mock.graph_node_details.insert(
            "peer_x".to_string(),
            make_graph_node(vec![500, 600], "10.10.10.10:9735"),
        );
        mock.graph_node_details.insert(
            "peer_y".to_string(),
            make_graph_node(vec![501, 601], "11.11.11.11:9735"),
        );

        let existing_peers = HashSet::new();
        let candidates =
            get_earnings_candidates(&mock, &db, &existing_peers, &own_id).await.unwrap();

        assert!(!candidates.is_empty(), "Should find earnings-based candidates");
        // Should find peer_x and/or peer_y (peers of our top earner)
        let found_peers: HashSet<_> = candidates.iter().map(|c| c.node_id.as_str()).collect();
        assert!(
            found_peers.contains("peer_x") || found_peers.contains("peer_y"),
            "Should discover peers of top earner. Got: {:?}",
            found_peers
        );
        // All should be GraphPeerOfEarner source
        for c in &candidates {
            assert!(matches!(c.source, CandidateSource::GraphPeerOfEarner));
        }
    }

    #[tokio::test]
    async fn test_graph_api_failure_graceful_fallback() {
        let mock = MockLdkClient::new();
        // Empty graph data - should return empty, not error
        let existing_peers = HashSet::new();
        let candidates =
            get_popularity_candidates(&mock, &existing_peers, "own_node").await.unwrap();
        assert!(candidates.is_empty(), "Should gracefully return empty on empty graph");
    }

    #[tokio::test]
    async fn test_get_candidates_excludes_existing_peers() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let mut mock = MockLdkClient::new();
        let config = test_config();

        // Set up a node in graph
        mock.graph_nodes.node_ids = vec!["existing_peer".to_string()];
        mock.graph_node_details.insert(
            "existing_peer".to_string(),
            make_graph_node(vec![1], "1.1.1.1:9735"),
        );

        let mut existing_peers = HashSet::new();
        existing_peers.insert("existing_peer".to_string());

        let candidates = get_candidates(&config, &mock, &db, &existing_peers)
            .await
            .unwrap();

        assert!(
            !candidates.iter().any(|c| c.node_id == "existing_peer"),
            "Should not include existing peers"
        );
    }
}
