use crate::db::Database;
use ldk_server_protos::types::Channel;
use log::{debug, info};
use std::collections::HashSet;

/// Update channel_history table: detect new channels, mark closed ones.
pub fn update(db: &Database, channels: &[Channel]) -> anyhow::Result<()> {
    let conn = db.conn();
    let now = chrono::Utc::now().timestamp() as f64;

    // Get currently-known open channels
    let mut known_open: HashSet<String> = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT channel_id FROM channel_history WHERE is_open = 1")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            known_open.insert(row?);
        }
    }

    let mut seen: HashSet<String> = HashSet::new();

    for ch in channels {
        let channel_id = &ch.channel_id;
        seen.insert(channel_id.clone());

        if known_open.contains(channel_id) {
            // Update last_seen
            conn.execute(
                "UPDATE channel_history SET last_seen_at = ?1 WHERE channel_id = ?2",
                rusqlite::params![now, channel_id],
            )?;
        } else {
            // New channel detected
            info!(
                "New channel detected: {} with peer {} ({}sat)",
                channel_id, ch.counterparty_node_id, ch.channel_value_sats
            );
            conn.execute(
                "INSERT OR REPLACE INTO channel_history \
                 (channel_id, user_channel_id, counterparty_node_id, channel_value_sats, \
                  first_seen_at, last_seen_at, is_open) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                rusqlite::params![
                    channel_id,
                    ch.user_channel_id,
                    ch.counterparty_node_id,
                    ch.channel_value_sats,
                    now,
                    now,
                ],
            )?;
        }
    }

    // Mark channels no longer present as closed
    for channel_id in &known_open {
        if !seen.contains(channel_id) {
            info!("Channel closed: {}", channel_id);
            conn.execute(
                "UPDATE channel_history SET is_open = 0, last_seen_at = ?1 WHERE channel_id = ?2",
                rusqlite::params![now, channel_id],
            )?;
        }
    }

    debug!(
        "Channel tracker: {} open, {} newly detected",
        seen.len(),
        seen.len().saturating_sub(known_open.len())
    );

    Ok(())
}

/// Get channel age in days for a given channel_id.
#[allow(dead_code)]
pub fn channel_age_days(db: &Database, channel_id: &str) -> anyhow::Result<Option<f64>> {
    let conn = db.conn();
    let now = chrono::Utc::now().timestamp() as f64;
    let result = conn.query_row(
        "SELECT first_seen_at FROM channel_history WHERE channel_id = ?1",
        [channel_id],
        |row| {
            let first_seen: f64 = row.get(0)?;
            Ok((now - first_seen) / 86400.0)
        },
    );
    match result {
        Ok(days) => Ok(Some(days)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn make_channel(id: &str, peer: &str, value_sats: u64) -> Channel {
        Channel {
            channel_id: id.to_string(),
            counterparty_node_id: peer.to_string(),
            user_channel_id: format!("user_{}", id),
            channel_value_sats: value_sats,
            is_usable: true,
            ..Default::default()
        }
    }

    #[test]
    fn test_new_channels_detected() {
        let db = Database::open_in_memory().unwrap();
        let channels = vec![
            make_channel("ch1", "peer_a", 1_000_000),
            make_channel("ch2", "peer_b", 500_000),
        ];

        update(&db, &channels).unwrap();

        // Verify both channels recorded
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM channel_history WHERE is_open = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_channel_closure_detected() {
        let db = Database::open_in_memory().unwrap();

        // First update: 2 channels
        let channels = vec![
            make_channel("ch1", "peer_a", 1_000_000),
            make_channel("ch2", "peer_b", 500_000),
        ];
        update(&db, &channels).unwrap();

        // Second update: only ch1 remains
        let channels = vec![make_channel("ch1", "peer_a", 1_000_000)];
        update(&db, &channels).unwrap();

        // ch2 should be marked closed
        let is_open: bool = db
            .conn()
            .query_row(
                "SELECT is_open FROM channel_history WHERE channel_id = 'ch2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!is_open);

        // ch1 should still be open
        let is_open: bool = db
            .conn()
            .query_row(
                "SELECT is_open FROM channel_history WHERE channel_id = 'ch1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(is_open);
    }

    #[test]
    fn test_channel_updates_last_seen() {
        let db = Database::open_in_memory().unwrap();

        let channels = vec![make_channel("ch1", "peer_a", 1_000_000)];
        update(&db, &channels).unwrap();

        let first_seen: f64 = db
            .conn()
            .query_row(
                "SELECT last_seen_at FROM channel_history WHERE channel_id = 'ch1'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Small sleep to ensure timestamp changes
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Update again
        update(&db, &channels).unwrap();

        let second_seen: f64 = db
            .conn()
            .query_row(
                "SELECT last_seen_at FROM channel_history WHERE channel_id = 'ch1'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        assert!(second_seen >= first_seen);
    }

    #[test]
    fn test_channel_age_days_unknown() {
        let db = Database::open_in_memory().unwrap();
        let age = channel_age_days(&db, "nonexistent").unwrap();
        assert!(age.is_none());
    }

    #[test]
    fn test_channel_age_days_known() {
        let db = Database::open_in_memory().unwrap();
        let channels = vec![make_channel("ch1", "peer_a", 1_000_000)];
        update(&db, &channels).unwrap();

        let age = channel_age_days(&db, "ch1").unwrap();
        assert!(age.is_some());
        // Just created, age should be very close to 0
        assert!(age.unwrap() < 0.01);
    }

    #[test]
    fn test_empty_channel_list() {
        let db = Database::open_in_memory().unwrap();

        // First: add channels
        let channels = vec![make_channel("ch1", "peer_a", 1_000_000)];
        update(&db, &channels).unwrap();

        // Then: empty list = all channels closed
        update(&db, &[]).unwrap();

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM channel_history WHERE is_open = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
