use anyhow::Context;
use rusqlite::Connection;
use std::path::Path;

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open database at {}", path.display()))?;

        // Enable WAL mode for crash safety
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    fn migrate(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(SCHEMA)?;
        Ok(())
    }
}

const SCHEMA: &str = r#"
-- Forwarding earnings per channel, bucketed by day
CREATE TABLE IF NOT EXISTS earnings (
    channel_id TEXT NOT NULL,
    counterparty_node_id TEXT NOT NULL,
    day_bucket INTEGER NOT NULL,
    fee_earned_msat INTEGER NOT NULL DEFAULT 0,
    amount_forwarded_msat INTEGER NOT NULL DEFAULT 0,
    direction TEXT NOT NULL CHECK (direction IN ('in', 'out')),
    PRIMARY KEY (channel_id, day_bucket, direction)
);
CREATE INDEX IF NOT EXISTS idx_earnings_node_day
    ON earnings(counterparty_node_id, day_bucket);

-- Rebalancing expenditures per channel
CREATE TABLE IF NOT EXISTS rebalance_costs (
    channel_id TEXT NOT NULL,
    counterparty_node_id TEXT NOT NULL,
    day_bucket INTEGER NOT NULL,
    fee_spent_msat INTEGER NOT NULL DEFAULT 0,
    amount_rebalanced_msat INTEGER NOT NULL DEFAULT 0,
    direction TEXT NOT NULL CHECK (direction IN ('in', 'out')),
    PRIMARY KEY (channel_id, day_bucket, direction)
);

-- Channel lifecycle tracking
CREATE TABLE IF NOT EXISTS channel_history (
    channel_id TEXT NOT NULL PRIMARY KEY,
    user_channel_id TEXT NOT NULL,
    counterparty_node_id TEXT NOT NULL,
    channel_value_sats INTEGER NOT NULL,
    first_seen_at REAL NOT NULL,
    last_seen_at REAL NOT NULL,
    is_open INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_channel_history_node
    ON channel_history(counterparty_node_id);

-- Price theory card game: center price per peer
CREATE TABLE IF NOT EXISTS price_theory_center (
    counterparty_node_id TEXT PRIMARY KEY,
    price INTEGER NOT NULL DEFAULT 0
);

-- Price theory card game: individual cards
CREATE TABLE IF NOT EXISTS price_theory_cards (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    counterparty_node_id TEXT NOT NULL,
    position INTEGER NOT NULL DEFAULT 0,
    deck_order INTEGER NOT NULL,
    price INTEGER NOT NULL,
    lifetime INTEGER NOT NULL,
    earnings_msat INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_cards_node_pos
    ON price_theory_cards(counterparty_node_id, position, deck_order);

-- On-chain fee samples for fee regime detection
CREATE TABLE IF NOT EXISTS onchain_fee_samples (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    feerate_sat_per_vb REAL NOT NULL,
    sampled_at REAL NOT NULL
);

-- Channels opened by autopilot (audit trail)
CREATE TABLE IF NOT EXISTS autopilot_opens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id TEXT,
    counterparty_node_id TEXT NOT NULL,
    amount_sats INTEGER NOT NULL,
    opened_at REAL NOT NULL,
    reason TEXT
);

-- Channels closed by judge (audit trail)
CREATE TABLE IF NOT EXISTS judge_closures (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id TEXT NOT NULL,
    counterparty_node_id TEXT NOT NULL,
    closed_at REAL NOT NULL,
    reason TEXT NOT NULL
);

-- Pagination cursor and other sync state
CREATE TABLE IF NOT EXISTS sync_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Known peer addresses for reconnection
CREATE TABLE IF NOT EXISTS peer_addresses (
    node_id TEXT NOT NULL PRIMARY KEY,
    address TEXT NOT NULL,
    last_connected_at REAL,
    source TEXT NOT NULL DEFAULT 'autopilot'
);

-- General run state
CREATE TABLE IF NOT EXISTS run_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_in_memory() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.conn().is_autocommit());
    }

    #[test]
    fn test_schema_tables_exist() {
        let db = Database::open_in_memory().unwrap();
        let tables: Vec<String> = {
            let mut stmt = db
                .conn()
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };

        let expected = vec![
            "autopilot_opens",
            "channel_history",
            "earnings",
            "judge_closures",
            "onchain_fee_samples",
            "peer_addresses",
            "price_theory_cards",
            "price_theory_center",
            "rebalance_costs",
            "run_state",
            "sync_state",
        ];

        for table in &expected {
            assert!(
                tables.contains(&table.to_string()),
                "Missing table: {}. Found: {:?}",
                table,
                tables
            );
        }
    }

    #[test]
    fn test_migrate_idempotent() {
        let db = Database::open_in_memory().unwrap();
        // Running migrate again should not fail
        db.migrate().unwrap();
    }
}
