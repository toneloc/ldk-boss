use crate::autopilot::candidate::{parse_node_address, HARDCODED_NODES};
use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use crate::state::NodeState;
use ldk_server_protos::api::ConnectPeerRequest;
use log::{debug, info, warn};
use std::collections::HashSet;

/// Reconnect to peers that have channels but appear offline.
///
/// Runs every cycle (reconnecting is cheap and important for routing).
/// Looks for channels where is_channel_ready=true but is_usable=false,
/// which indicates the peer is disconnected.
pub async fn run(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    state: &NodeState,
) -> anyhow::Result<()> {
    // Seed known addresses from config and hardcoded nodes (idempotent)
    seed_addresses(config, db)?;

    // Find peers with ready-but-not-usable channels (likely disconnected)
    let disconnected_peers: HashSet<String> = state
        .channels
        .iter()
        .filter(|ch| ch.is_channel_ready && !ch.is_usable)
        .map(|ch| ch.counterparty_node_id.clone())
        .collect();

    if disconnected_peers.is_empty() {
        debug!("Reconnector: all peers connected");
        return Ok(());
    }

    info!(
        "Reconnector: {} peers appear disconnected, attempting reconnection",
        disconnected_peers.len()
    );

    let conn = db.conn();

    for peer_id in &disconnected_peers {
        // Look up address
        let address: Option<String> = conn
            .query_row(
                "SELECT address FROM peer_addresses WHERE node_id = ?1",
                [peer_id],
                |row| row.get(0),
            )
            .ok();

        let address = match address {
            Some(addr) => addr,
            None => {
                debug!(
                    "Reconnector: no known address for peer {}, skipping",
                    peer_id
                );
                continue;
            }
        };

        if config.general.dry_run {
            info!(
                "Reconnector: would reconnect to {} at {} (dry-run)",
                peer_id, address
            );
            continue;
        }

        match client
            .connect_peer(ConnectPeerRequest {
                node_pubkey: peer_id.clone(),
                address: address.clone(),
                persist: true,
            })
            .await
        {
            Ok(_) => {
                info!("Reconnector: reconnected to {} at {}", peer_id, address);
                // Update last_connected_at
                let now = chrono::Utc::now().timestamp() as f64;
                let _ = conn.execute(
                    "UPDATE peer_addresses SET last_connected_at = ?1 WHERE node_id = ?2",
                    rusqlite::params![now, peer_id],
                );
            }
            Err(e) => {
                warn!(
                    "Reconnector: failed to reconnect to {} at {}: {}",
                    peer_id, address, e
                );
            }
        }
    }

    Ok(())
}

/// Seed the peer_addresses table from config seed_nodes and hardcoded nodes.
fn seed_addresses(config: &Config, db: &Database) -> anyhow::Result<()> {
    let conn = db.conn();

    // Seed from user-configured seed nodes
    for seed in &config.autopilot.seed_nodes {
        if let Some((node_id, address)) = parse_node_address(seed) {
            conn.execute(
                "INSERT OR IGNORE INTO peer_addresses (node_id, address, source) \
                 VALUES (?1, ?2, 'config')",
                rusqlite::params![node_id, address],
            )?;
        }
    }

    // Seed from hardcoded nodes
    for (node_id, address) in HARDCODED_NODES {
        conn.execute(
            "INSERT OR IGNORE INTO peer_addresses (node_id, address, source) \
             VALUES (?1, ?2, 'hardcoded')",
            rusqlite::params![node_id, address],
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::mock::MockLdkClient;
    use crate::config::Config;
    use crate::db::Database;
    use ldk_server_protos::api::GetBalancesResponse;
    use ldk_server_protos::types::Channel;

    fn test_config() -> Config {
        Config::test_default(std::path::PathBuf::from("/dev/null"))
    }

    fn make_channel(id: &str, peer: &str, ready: bool, usable: bool) -> Channel {
        Channel {
            channel_id: id.to_string(),
            counterparty_node_id: peer.to_string(),
            user_channel_id: format!("user_{}", id),
            channel_value_sats: 1_000_000,
            is_channel_ready: ready,
            is_usable: usable,
            ..Default::default()
        }
    }

    #[test]
    fn test_seed_addresses_from_config() {
        let db = Database::open_in_memory().unwrap();
        let mut config = test_config();
        config.autopilot.seed_nodes = vec![
            "03abc@1.2.3.4:9735".to_string(),
        ];

        seed_addresses(&config, &db).unwrap();

        let addr: String = db
            .conn()
            .query_row(
                "SELECT address FROM peer_addresses WHERE node_id = '03abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(addr, "1.2.3.4:9735");
    }

    #[test]
    fn test_seed_addresses_hardcoded() {
        let db = Database::open_in_memory().unwrap();
        let config = test_config();

        seed_addresses(&config, &db).unwrap();

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM peer_addresses", [], |r| r.get(0))
            .unwrap();
        // Should have seeded hardcoded nodes
        assert!(count >= 10, "Expected at least 10 hardcoded nodes, got {}", count);
    }

    #[test]
    fn test_seed_addresses_idempotent() {
        let db = Database::open_in_memory().unwrap();
        let config = test_config();

        seed_addresses(&config, &db).unwrap();
        let count1: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM peer_addresses", [], |r| r.get(0))
            .unwrap();

        seed_addresses(&config, &db).unwrap();
        let count2: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM peer_addresses", [], |r| r.get(0))
            .unwrap();

        assert_eq!(count1, count2);
    }

    #[tokio::test]
    async fn test_reconnector_all_connected() {
        let db = Database::open_in_memory().unwrap();
        let config = test_config();
        let mock = MockLdkClient::new();

        let state = NodeState {
            node_info: mock.node_info.clone(),
            balances: GetBalancesResponse::default(),
            channels: vec![make_channel("ch1", "peer_a", true, true)],
        };

        run(&config, &mock, &db, &state).await.unwrap();

        // All connected — no connect_peer calls
        assert!(mock.connect_peer_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_reconnector_reconnects_disconnected() {
        let db = Database::open_in_memory().unwrap();
        let config = test_config();
        let mock = MockLdkClient::new();

        // Seed an address for peer_a
        db.conn()
            .execute(
                "INSERT INTO peer_addresses (node_id, address, source) VALUES ('peer_a', '1.2.3.4:9735', 'test')",
                [],
            )
            .unwrap();

        let state = NodeState {
            node_info: mock.node_info.clone(),
            balances: GetBalancesResponse::default(),
            channels: vec![make_channel("ch1", "peer_a", true, false)], // ready but not usable
        };

        run(&config, &mock, &db, &state).await.unwrap();

        let calls = mock.connect_peer_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].node_pubkey, "peer_a");
        assert_eq!(calls[0].address, "1.2.3.4:9735");
    }

    #[tokio::test]
    async fn test_reconnector_skips_unknown_address() {
        let db = Database::open_in_memory().unwrap();
        let config = test_config();
        let mock = MockLdkClient::new();

        // No address seeded for peer_a
        let state = NodeState {
            node_info: mock.node_info.clone(),
            balances: GetBalancesResponse::default(),
            channels: vec![make_channel("ch1", "peer_a", true, false)],
        };

        run(&config, &mock, &db, &state).await.unwrap();

        // No address → no connect_peer calls
        assert!(mock.connect_peer_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_reconnector_dry_run() {
        let db = Database::open_in_memory().unwrap();
        let mut config = test_config();
        config.general.dry_run = true;
        let mock = MockLdkClient::new();

        db.conn()
            .execute(
                "INSERT INTO peer_addresses (node_id, address, source) VALUES ('peer_a', '1.2.3.4:9735', 'test')",
                [],
            )
            .unwrap();

        let state = NodeState {
            node_info: mock.node_info.clone(),
            balances: GetBalancesResponse::default(),
            channels: vec![make_channel("ch1", "peer_a", true, false)],
        };

        run(&config, &mock, &db, &state).await.unwrap();

        // Dry-run: no actual connect_peer calls
        assert!(mock.connect_peer_calls.lock().unwrap().is_empty());
    }
}
