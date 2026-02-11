use crate::config::OnchainFeesConfig;
use crate::db::Database;
use log::{debug, warn};
use serde::Deserialize;

/// On-chain fee regime: low fees are favorable for channel operations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FeeRegime {
    Low,
    High,
}

/// Mempool.space recommended fees response.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MempoolFees {
    fastest_fee: f64,
    half_hour_fee: f64,
    hour_fee: f64,
    economy_fee: f64,
    minimum_fee: f64,
}

/// Poll fee estimator for current fee estimates and record a sample.
pub async fn update(db: &Database, config: &OnchainFeesConfig) -> anyhow::Result<()> {
    if config.provider == "none" {
        debug!("On-chain fee provider disabled");
        return Ok(());
    }

    // Try to fetch from mempool.space (or configured URL)
    let feerate = match fetch_mempool_fee(&config.mempool_api_url).await {
        Ok(fee) => fee,
        Err(e) => {
            warn!("Failed to fetch on-chain fees from mempool.space: {}", e);
            return Ok(());
        }
    };

    let conn = db.conn();
    let now = chrono::Utc::now().timestamp() as f64;

    conn.execute(
        "INSERT INTO onchain_fee_samples (feerate_sat_per_vb, sampled_at) VALUES (?1, ?2)",
        rusqlite::params![feerate, now],
    )?;

    debug!("On-chain fee sample: {:.1} sat/vB", feerate);

    // Prune old samples (keep last 7 days = ~1008 10-minute samples)
    let cutoff = now - (7.0 * 86400.0);
    conn.execute(
        "DELETE FROM onchain_fee_samples WHERE sampled_at < ?1",
        [cutoff],
    )?;

    Ok(())
}

/// Determine the current fee regime using the CLBoss algorithm:
/// Track historical fees and use percentile-based hysteresis.
///
/// If the current fee is below the `hi_to_lo_percentile` of history: Low regime.
/// If above `lo_to_hi_percentile`: High regime.
/// Otherwise: maintain previous state (hysteresis).
pub fn current_regime(
    db: &Database,
    hi_to_lo_pct: f64,
    lo_to_hi_pct: f64,
) -> anyhow::Result<FeeRegime> {
    let conn = db.conn();

    // Get all samples ordered by feerate
    let mut stmt = conn.prepare(
        "SELECT feerate_sat_per_vb FROM onchain_fee_samples ORDER BY feerate_sat_per_vb ASC",
    )?;
    let feerates: Vec<f64> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    if feerates.is_empty() {
        // No data yet -- assume high fee regime to be conservative
        // (CLBoss initializes with low-fee history to be conservative;
        // we go the other direction since we don't want to open channels
        // before we have fee data)
        return Ok(FeeRegime::High);
    }

    let n = feerates.len();

    // Get the latest fee
    let latest: f64 = conn
        .query_row(
            "SELECT feerate_sat_per_vb FROM onchain_fee_samples ORDER BY sampled_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);

    // Compute percentile thresholds
    let lo_idx = ((hi_to_lo_pct / 100.0) * n as f64) as usize;
    let hi_idx = ((lo_to_hi_pct / 100.0) * n as f64) as usize;

    let lo_threshold = feerates[lo_idx.min(n - 1)];
    let hi_threshold = feerates[hi_idx.min(n - 1)];

    if latest <= lo_threshold {
        Ok(FeeRegime::Low)
    } else if latest >= hi_threshold {
        Ok(FeeRegime::High)
    } else {
        // Hysteresis: check saved state
        let saved = conn
            .query_row(
                "SELECT value FROM run_state WHERE key = 'fee_regime'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_else(|_| "high".to_string());

        if saved == "low" {
            Ok(FeeRegime::Low)
        } else {
            Ok(FeeRegime::High)
        }
    }
}

/// Save the current fee regime for hysteresis.
pub fn save_regime(db: &Database, regime: FeeRegime) -> anyhow::Result<()> {
    let value = match regime {
        FeeRegime::Low => "low",
        FeeRegime::High => "high",
    };
    db.conn().execute(
        "INSERT OR REPLACE INTO run_state (key, value) VALUES ('fee_regime', ?1)",
        [value],
    )?;
    Ok(())
}

/// Insert a fee sample directly (for testing).
#[cfg(test)]
fn insert_sample(db: &Database, feerate: f64, sampled_at: f64) {
    db.conn()
        .execute(
            "INSERT INTO onchain_fee_samples (feerate_sat_per_vb, sampled_at) VALUES (?1, ?2)",
            rusqlite::params![feerate, sampled_at],
        )
        .unwrap();
}

async fn fetch_mempool_fee(api_url: &str) -> anyhow::Result<f64> {
    let url = format!("{}/v1/fees/recommended", api_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp: MempoolFees = client
        .get(&url)
        .send()
        .await?
        .json()
        .await?;

    // Use the "hour" fee as our reference (moderate urgency)
    Ok(resp.hour_fee)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn test_regime_no_data_defaults_high() {
        let db = Database::open_in_memory().unwrap();
        let regime = current_regime(&db, 17.0, 23.0).unwrap();
        assert_eq!(regime, FeeRegime::High);
    }

    #[test]
    fn test_regime_low_when_latest_below_threshold() {
        let db = Database::open_in_memory().unwrap();
        let now = 1704067200.0;

        // Insert 100 samples at various feerates (1 to 100 sat/vB)
        for i in 1..=100 {
            insert_sample(&db, i as f64, now - (100 - i) as f64 * 600.0);
        }
        // Insert a very low latest sample
        insert_sample(&db, 1.0, now + 1.0);

        let regime = current_regime(&db, 17.0, 23.0).unwrap();
        assert_eq!(regime, FeeRegime::Low);
    }

    #[test]
    fn test_regime_high_when_latest_above_threshold() {
        let db = Database::open_in_memory().unwrap();
        let now = 1704067200.0;

        // Insert 100 samples at various feerates (1 to 100 sat/vB)
        for i in 1..=100 {
            insert_sample(&db, i as f64, now - (100 - i) as f64 * 600.0);
        }
        // Insert a very high latest sample
        insert_sample(&db, 99.0, now + 1.0);

        let regime = current_regime(&db, 17.0, 23.0).unwrap();
        assert_eq!(regime, FeeRegime::High);
    }

    #[test]
    fn test_regime_hysteresis_preserves_state() {
        let db = Database::open_in_memory().unwrap();
        let now = 1704067200.0;

        // Insert samples from 1 to 100
        for i in 1..=100 {
            insert_sample(&db, i as f64, now - (100 - i) as f64 * 600.0);
        }
        // Latest fee at 20 (between 17th and 23rd percentile thresholds)
        insert_sample(&db, 20.0, now + 1.0);

        // Default state is "high" (no saved state)
        let regime = current_regime(&db, 17.0, 23.0).unwrap();
        assert_eq!(regime, FeeRegime::High);

        // Save "low" state and check hysteresis preserves it
        save_regime(&db, FeeRegime::Low).unwrap();
        let regime = current_regime(&db, 17.0, 23.0).unwrap();
        assert_eq!(regime, FeeRegime::Low);
    }

    #[test]
    fn test_save_and_load_regime() {
        let db = Database::open_in_memory().unwrap();

        save_regime(&db, FeeRegime::Low).unwrap();
        let val: String = db
            .conn()
            .query_row(
                "SELECT value FROM run_state WHERE key = 'fee_regime'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "low");

        save_regime(&db, FeeRegime::High).unwrap();
        let val: String = db
            .conn()
            .query_row(
                "SELECT value FROM run_state WHERE key = 'fee_regime'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "high");
    }

    #[test]
    fn test_regime_single_sample() {
        let db = Database::open_in_memory().unwrap();
        // Single sample: latest is 5.0, only data point
        // lo_threshold = feerates[0] = 5.0, latest <= lo_threshold → Low
        insert_sample(&db, 5.0, 1704067200.0);
        let regime = current_regime(&db, 17.0, 23.0).unwrap();
        assert_eq!(regime, FeeRegime::Low);
    }
}
