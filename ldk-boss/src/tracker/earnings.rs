use crate::client::LdkClient;
use crate::db::Database;
use ldk_server_protos::types::PageToken;
use log::{debug, info};

/// Day bucket: start-of-day Unix timestamp for a given time.
fn day_bucket(timestamp_secs: f64) -> i64 {
    let secs = timestamp_secs as i64;
    secs - (secs % 86400)
}

/// Incrementally fetch new forwarded payments and record earnings.
pub async fn ingest(db: &Database, client: &(impl LdkClient + Sync)) -> anyhow::Result<()> {
    let conn = db.conn();

    // Load pagination cursor
    let saved_token = load_page_token(conn)?;
    let mut page_token = saved_token;
    let mut total_ingested = 0u64;

    loop {
        let resp = client.list_forwarded_payments(page_token.clone()).await?;

        for fwd in &resp.forwarded_payments {
            let fee_msat = fwd.total_fee_earned_msat.unwrap_or(0);
            let amount_msat = fwd.outbound_amount_forwarded_msat.unwrap_or(0);
            let now_bucket = day_bucket(chrono::Utc::now().timestamp() as f64);

            // Record incoming side (prev_channel_id)
            if !fwd.prev_channel_id.is_empty() {
                conn.execute(
                    "INSERT INTO earnings (channel_id, counterparty_node_id, day_bucket, \
                     fee_earned_msat, amount_forwarded_msat, direction) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'in') \
                     ON CONFLICT(channel_id, day_bucket, direction) DO UPDATE SET \
                     fee_earned_msat = fee_earned_msat + ?4, \
                     amount_forwarded_msat = amount_forwarded_msat + ?5",
                    rusqlite::params![
                        fwd.prev_channel_id,
                        fwd.prev_node_id,
                        now_bucket,
                        fee_msat,
                        amount_msat,
                    ],
                )?;
            }

            // Record outgoing side (next_channel_id)
            if !fwd.next_channel_id.is_empty() {
                conn.execute(
                    "INSERT INTO earnings (channel_id, counterparty_node_id, day_bucket, \
                     fee_earned_msat, amount_forwarded_msat, direction) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'out') \
                     ON CONFLICT(channel_id, day_bucket, direction) DO UPDATE SET \
                     fee_earned_msat = fee_earned_msat + ?4, \
                     amount_forwarded_msat = amount_forwarded_msat + ?5",
                    rusqlite::params![
                        fwd.next_channel_id,
                        fwd.next_node_id,
                        now_bucket,
                        fee_msat,
                        amount_msat,
                    ],
                )?;
            }

            total_ingested += 1;
        }

        // Save pagination state
        if let Some(ref token) = resp.next_page_token {
            save_page_token(conn, token)?;
            page_token = Some(token.clone());
        } else {
            // No more pages
            break;
        }

        // If we got fewer results than a typical page, we're at the end
        if resp.forwarded_payments.is_empty() {
            break;
        }
    }

    if total_ingested > 0 {
        info!("Earnings tracker: ingested {} new forwarded payments", total_ingested);
    } else {
        debug!("Earnings tracker: no new forwarded payments");
    }

    Ok(())
}

/// Query total earnings for a channel since a given timestamp.
pub fn earnings_since(
    db: &Database,
    channel_id: &str,
    since_timestamp: f64,
) -> anyhow::Result<(i64, i64)> {
    let conn = db.conn();
    let bucket = day_bucket(since_timestamp);
    let row = conn.query_row(
        "SELECT COALESCE(SUM(fee_earned_msat), 0), COALESCE(SUM(amount_forwarded_msat), 0) \
         FROM earnings WHERE channel_id = ?1 AND day_bucket >= ?2",
        rusqlite::params![channel_id, bucket],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    Ok(row)
}

/// Query total earnings for a peer (across all their channels) since a given timestamp.
pub fn peer_earnings_since(
    db: &Database,
    counterparty_node_id: &str,
    since_timestamp: f64,
) -> anyhow::Result<PeerEarnings> {
    let conn = db.conn();
    let bucket = day_bucket(since_timestamp);

    let in_earned: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(fee_earned_msat), 0) FROM earnings \
             WHERE counterparty_node_id = ?1 AND day_bucket >= ?2 AND direction = 'in'",
            rusqlite::params![counterparty_node_id, bucket],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let out_earned: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(fee_earned_msat), 0) FROM earnings \
             WHERE counterparty_node_id = ?1 AND day_bucket >= ?2 AND direction = 'out'",
            rusqlite::params![counterparty_node_id, bucket],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let in_rebalance_cost: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(fee_spent_msat), 0) FROM rebalance_costs \
             WHERE counterparty_node_id = ?1 AND day_bucket >= ?2 AND direction = 'in'",
            rusqlite::params![counterparty_node_id, bucket],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let out_rebalance_cost: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(fee_spent_msat), 0) FROM rebalance_costs \
             WHERE counterparty_node_id = ?1 AND day_bucket >= ?2 AND direction = 'out'",
            rusqlite::params![counterparty_node_id, bucket],
            |r| r.get(0),
        )
        .unwrap_or(0);

    Ok(PeerEarnings {
        in_earnings_msat: in_earned,
        out_earnings_msat: out_earned,
        in_expenditures_msat: in_rebalance_cost,
        out_expenditures_msat: out_rebalance_cost,
    })
}

pub struct PeerEarnings {
    pub in_earnings_msat: i64,
    pub out_earnings_msat: i64,
    pub in_expenditures_msat: i64,
    pub out_expenditures_msat: i64,
}

impl PeerEarnings {
    /// Net in-earnings (earned - spent on rebalancing inbound)
    pub fn in_net(&self) -> i64 {
        self.in_earnings_msat - self.in_expenditures_msat
    }
    /// Net out-earnings (earned - spent on rebalancing outbound)
    pub fn out_net(&self) -> i64 {
        self.out_earnings_msat - self.out_expenditures_msat
    }
    /// Total net earnings across both directions
    pub fn total_net(&self) -> i64 {
        self.in_net() + self.out_net()
    }
}

fn load_page_token(conn: &rusqlite::Connection) -> anyhow::Result<Option<PageToken>> {
    let result = conn.query_row(
        "SELECT value FROM sync_state WHERE key = 'forwarded_payments_token'",
        [],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(json_str) => {
            // Simple token storage: "index:token" format
            let parts: Vec<&str> = json_str.splitn(2, ':').collect();
            if parts.len() == 2 {
                Ok(Some(PageToken {
                    index: parts[0].parse().unwrap_or(0),
                    token: parts[1].to_string(),
                }))
            } else {
                Ok(None)
            }
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn save_page_token(conn: &rusqlite::Connection, token: &PageToken) -> anyhow::Result<()> {
    let value = format!("{}:{}", token.index, token.token);
    conn.execute(
        "INSERT OR REPLACE INTO sync_state (key, value) VALUES ('forwarded_payments_token', ?1)",
        [&value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_day_bucket_at_midnight() {
        // Midnight UTC = should return itself
        let midnight = 1704067200.0; // 2024-01-01 00:00:00 UTC
        assert_eq!(day_bucket(midnight), 1704067200);
    }

    #[test]
    fn test_day_bucket_truncates() {
        // 2024-01-01 12:30:45 UTC
        let mid_day = 1704067200.0 + 12.0 * 3600.0 + 30.0 * 60.0 + 45.0;
        assert_eq!(day_bucket(mid_day), 1704067200);
    }

    #[test]
    fn test_day_bucket_end_of_day() {
        // 2024-01-01 23:59:59 UTC
        let end_of_day = 1704067200.0 + 86399.0;
        assert_eq!(day_bucket(end_of_day), 1704067200);
    }

    #[test]
    fn test_day_bucket_next_day() {
        // 2024-01-02 00:00:00 UTC
        let next_day = 1704067200.0 + 86400.0;
        assert_eq!(day_bucket(next_day), 1704067200 + 86400);
    }

    #[test]
    fn test_peer_earnings_net_calculations() {
        let pe = PeerEarnings {
            in_earnings_msat: 10000,
            out_earnings_msat: 8000,
            in_expenditures_msat: 3000,
            out_expenditures_msat: 2000,
        };
        assert_eq!(pe.in_net(), 7000);
        assert_eq!(pe.out_net(), 6000);
        assert_eq!(pe.total_net(), 13000);
    }

    #[test]
    fn test_peer_earnings_negative_net() {
        let pe = PeerEarnings {
            in_earnings_msat: 1000,
            out_earnings_msat: 500,
            in_expenditures_msat: 5000,
            out_expenditures_msat: 3000,
        };
        assert_eq!(pe.in_net(), -4000);
        assert_eq!(pe.out_net(), -2500);
        assert_eq!(pe.total_net(), -6500);
    }

    #[test]
    fn test_peer_earnings_zero() {
        let pe = PeerEarnings {
            in_earnings_msat: 0,
            out_earnings_msat: 0,
            in_expenditures_msat: 0,
            out_expenditures_msat: 0,
        };
        assert_eq!(pe.total_net(), 0);
    }

    #[test]
    fn test_load_page_token_round_trip() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let conn = db.conn();

        // Initially no token
        assert!(load_page_token(conn).unwrap().is_none());

        // Save and load
        let token = PageToken {
            index: 42,
            token: "abc123".to_string(),
        };
        save_page_token(conn, &token).unwrap();

        let loaded = load_page_token(conn).unwrap().unwrap();
        assert_eq!(loaded.index, 42);
        assert_eq!(loaded.token, "abc123");
    }

    #[test]
    fn test_earnings_since_empty_db() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let (fees, amount) = earnings_since(&db, "nonexistent_channel", 0.0).unwrap();
        assert_eq!(fees, 0);
        assert_eq!(amount, 0);
    }

    #[test]
    fn test_earnings_since_with_data() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let conn = db.conn();

        // Insert earnings
        conn.execute(
            "INSERT INTO earnings (channel_id, counterparty_node_id, day_bucket, fee_earned_msat, amount_forwarded_msat, direction) \
             VALUES ('ch1', 'peer1', 1704067200, 5000, 100000, 'in')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO earnings (channel_id, counterparty_node_id, day_bucket, fee_earned_msat, amount_forwarded_msat, direction) \
             VALUES ('ch1', 'peer1', 1704153600, 3000, 80000, 'out')",
            [],
        ).unwrap();

        let (fees, amount) = earnings_since(&db, "ch1", 1704067200.0).unwrap();
        assert_eq!(fees, 8000);
        assert_eq!(amount, 180000);
    }

    #[test]
    fn test_peer_earnings_since_with_data() {
        let db = crate::db::Database::open_in_memory().unwrap();
        let conn = db.conn();

        conn.execute(
            "INSERT INTO earnings (channel_id, counterparty_node_id, day_bucket, fee_earned_msat, amount_forwarded_msat, direction) \
             VALUES ('ch1', 'peer1', 1704067200, 5000, 100000, 'in')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO earnings (channel_id, counterparty_node_id, day_bucket, fee_earned_msat, amount_forwarded_msat, direction) \
             VALUES ('ch2', 'peer1', 1704067200, 3000, 80000, 'out')",
            [],
        ).unwrap();

        let pe = peer_earnings_since(&db, "peer1", 1704067200.0).unwrap();
        assert_eq!(pe.in_earnings_msat, 5000);
        assert_eq!(pe.out_earnings_msat, 3000);
        assert_eq!(pe.total_net(), 8000);
    }
}
