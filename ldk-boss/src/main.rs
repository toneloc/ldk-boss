#![allow(dead_code)]

mod autopilot;
mod client;
mod config;
mod db;
mod fees;
mod judge;
mod rebalancer;
mod scheduler;
mod state;
mod tracker;

use crate::client::LdkClient;
use clap::{Parser, Subcommand};
use config::Config;
use log::{error, info, warn};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::watch;

#[derive(Parser)]
#[command(name = "ldk-boss", about = "Autopilot daemon for LDK Server")]
struct Cli {
    /// Path to ldkboss.toml config file
    #[arg(short, long, default_value = "ldkboss.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as a background daemon (default)
    Daemon,
    /// Execute a single control cycle and exit
    RunOnce,
    /// Print current status from the database
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let config = Config::load(&cli.config)?;

    // Initialize logging
    let log_level = config.general.log_level.clone();
    env_logger::Builder::new()
        .filter_level(log_level.parse().unwrap_or(log::LevelFilter::Info))
        .format_timestamp_secs()
        .init();

    info!("LDKBoss v{} starting", env!("CARGO_PKG_VERSION"));

    if config.general.dry_run {
        warn!("DRY-RUN MODE: No actions will be executed");
    }
    if !config.general.enabled {
        warn!("Master switch is OFF -- exiting");
        return Ok(());
    }

    let config = Arc::new(config);

    // Initialize components
    let client = client::LdkBossClient::new(&config)?;
    let db = db::Database::open(&config.general.database_path)?;

    match cli.command.unwrap_or(Commands::Daemon) {
        Commands::Daemon => run_daemon(config, client, db).await,
        Commands::RunOnce => run_once(config, client, db).await,
        Commands::Status => print_status(db),
    }
}

async fn run_daemon(
    config: Arc<Config>,
    client: impl LdkClient,
    db: db::Database,
) -> anyhow::Result<()> {
    // Startup connectivity check
    info!("Verifying LDK Server connectivity...");
    match client.get_node_info().await {
        Ok(info) => {
            info!(
                "Connected to LDK Server node: {}",
                info.node_id
            );
        }
        Err(e) => {
            error!("Cannot reach LDK Server: {}. Aborting.", e);
            return Err(e.into());
        }
    }

    // Shutdown signal
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = signal::ctrl_c().await;
        info!("Received shutdown signal, finishing current cycle...");
        let _ = shutdown_tx.send(true);
    });

    let mut sched = scheduler::Scheduler::new(&config);
    let interval = std::time::Duration::from_secs(config.general.loop_interval_secs);

    info!(
        "Entering main loop (interval: {}s)",
        config.general.loop_interval_secs
    );

    loop {
        if *shutdown_rx.borrow() {
            info!("Shutting down gracefully");
            break;
        }

        if let Err(e) = run_cycle(&config, &client, &db, &mut sched).await {
            error!("Cycle error: {:#}", e);
        }

        sched.tick();

        tokio::select! {
            _ = tokio::time::sleep(interval) => {},
            _ = shutdown_rx.changed() => {
                info!("Shutting down gracefully");
                break;
            }
        }
    }

    Ok(())
}

async fn run_once(
    config: Arc<Config>,
    client: impl LdkClient,
    db: db::Database,
) -> anyhow::Result<()> {
    info!("Running single cycle...");
    let mut sched = scheduler::Scheduler::new_force_all(&config);
    run_cycle(&config, &client, &db, &mut sched).await?;
    info!("Single cycle complete");
    Ok(())
}

pub async fn run_cycle(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &db::Database,
    sched: &mut scheduler::Scheduler,
) -> anyhow::Result<()> {
    // Phase 1: Collect node state
    let node_state = state::NodeState::collect(client, db).await?;

    // Phase 2: Update trackers
    tracker::update(db, client, &node_state).await?;

    // Phase 3: Fee management
    if config.fees.enabled {
        if let Err(e) = fees::run(config, client, db, &node_state).await {
            error!("Fee management error: {:#}", e);
        }
    }

    // Phase 4: Channel autopilot
    if config.autopilot.enabled && sched.should_run_autopilot() {
        if let Err(e) = autopilot::run(config, client, db, &node_state).await {
            error!("Autopilot error: {:#}", e);
        }
    }

    // Phase 5: Rebalancing
    if config.rebalancer.enabled && sched.should_run_rebalancer() {
        if let Err(e) = rebalancer::run(config, client, db, &node_state).await {
            error!("Rebalancer error: {:#}", e);
        }
    }

    // Phase 6: Peer judgment
    if config.judge.enabled && sched.should_run_judge() {
        if let Err(e) = judge::run(config, client, db, &node_state).await {
            error!("Judge error: {:#}", e);
        }
    }

    Ok(())
}

fn print_status(db: db::Database) -> anyhow::Result<()> {
    let conn = db.conn();

    // Channel count
    let open_channels: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM channel_history WHERE is_open = 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    // Total earnings
    let total_earned: i64 = conn
        .query_row("SELECT COALESCE(SUM(fee_earned_msat), 0) FROM earnings", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);

    // Autopilot opens
    let total_opens: i64 = conn
        .query_row("SELECT COUNT(*) FROM autopilot_opens", [], |r| r.get(0))
        .unwrap_or(0);

    // Judge closures
    let total_closures: i64 = conn
        .query_row("SELECT COUNT(*) FROM judge_closures", [], |r| r.get(0))
        .unwrap_or(0);

    println!("LDKBoss Status");
    println!("==============");
    println!("Open channels tracked:  {}", open_channels);
    println!(
        "Total fees earned:      {} msat ({:.3} sat)",
        total_earned,
        total_earned as f64 / 1000.0
    );
    println!("Autopilot opens:        {}", total_opens);
    println!("Judge closures:         {}", total_closures);

    Ok(())
}

#[cfg(test)]
mod integration_tests {
    use crate::client::mock::MockLdkClient;
    use crate::config::Config;
    use crate::db::Database;
    use crate::scheduler::Scheduler;
    use crate::tracker::onchain_fees;
    use ldk_server_protos::api::{GetBalancesResponse, ListChannelsResponse};
    use ldk_server_protos::types::{Channel, ChannelConfig};

    fn test_config() -> Config {
        let mut config = Config::test_default(std::path::PathBuf::from("/dev/null"));
        config.general.dry_run = false;
        config
    }

    fn make_channel(id: &str, peer: &str, value_sats: u64, outbound_msat: u64) -> Channel {
        Channel {
            channel_id: id.to_string(),
            counterparty_node_id: peer.to_string(),
            user_channel_id: format!("user_{}", id),
            channel_value_sats: value_sats,
            outbound_capacity_msat: outbound_msat,
            inbound_capacity_msat: value_sats * 1000 - outbound_msat,
            is_usable: true,
            is_channel_ready: true,
            channel_config: Some(ChannelConfig {
                forwarding_fee_base_msat: Some(1000),
                forwarding_fee_proportional_millionths: Some(100),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // Test 1: Empty node cycle
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_cycle_empty_node() {
        let db = Database::open_in_memory().unwrap();
        let config = test_config();
        let mut sched = Scheduler::new_force_all(&config);

        let mut mock = MockLdkClient::new();
        mock.balances = GetBalancesResponse {
            spendable_onchain_balance_sats: 50_000,
            total_onchain_balance_sats: 50_000,
            total_lightning_balance_sats: 0,
            ..Default::default()
        };

        let result = super::run_cycle(&config, &mock, &db, &mut sched).await;
        assert!(result.is_ok(), "Cycle should succeed with empty node: {:?}", result.err());

        // No channels → no fee updates
        assert!(mock.update_config_calls.lock().unwrap().is_empty());
        // No channels to close
        assert!(mock.close_channel_calls.lock().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 2: Fee adjustment on channels with different balance ratios
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_cycle_fee_adjustment() {
        let db = Database::open_in_memory().unwrap();
        let mut config = test_config();
        config.fees.enabled = true;
        config.fees.balance_modder_enabled = true;
        config.fees.price_theory_enabled = false; // Isolate balance modder
        config.autopilot.enabled = false;
        config.rebalancer.enabled = false;
        config.judge.enabled = false;

        let mut sched = Scheduler::new_force_all(&config);

        let mut mock = MockLdkClient::new();
        mock.channels = ListChannelsResponse {
            channels: vec![
                // Channel 1: heavily outbound (90% ours) → should lower fees
                make_channel("ch1", "peer_a", 1_000_000, 900_000_000),
                // Channel 2: heavily inbound (10% ours) → should raise fees
                make_channel("ch2", "peer_b", 1_000_000, 100_000_000),
            ],
        };
        mock.balances = GetBalancesResponse {
            total_lightning_balance_sats: 2_000_000,
            ..Default::default()
        };

        let result = super::run_cycle(&config, &mock, &db, &mut sched).await;
        assert!(result.is_ok());

        let calls = mock.update_config_calls.lock().unwrap();
        // Both channels should get fee updates (different from default 1000/100)
        // Channel 1 (outbound-heavy): lower fees → PPM < 100
        // Channel 2 (inbound-heavy): higher fees → PPM > 100
        assert_eq!(calls.len(), 2, "Both channels should get fee updates");

        let ch1_call = calls.iter().find(|c| c.user_channel_id == "user_ch1").unwrap();
        let ch2_call = calls.iter().find(|c| c.user_channel_id == "user_ch2").unwrap();

        let ch1_ppm = ch1_call.channel_config.as_ref().unwrap()
            .forwarding_fee_proportional_millionths.unwrap();
        let ch2_ppm = ch2_call.channel_config.as_ref().unwrap()
            .forwarding_fee_proportional_millionths.unwrap();

        assert!(
            ch1_ppm < ch2_ppm,
            "Outbound-heavy channel ({}) should have lower fees than inbound-heavy ({})",
            ch1_ppm, ch2_ppm
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: Autopilot opens channels when conditions met
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_cycle_autopilot_opens() {
        let db = Database::open_in_memory().unwrap();
        let mut config = test_config();
        config.autopilot.enabled = true;
        config.fees.enabled = false;
        config.rebalancer.enabled = false;
        config.judge.enabled = false;

        // Set low fee regime so autopilot proceeds
        onchain_fees::save_regime(&db, onchain_fees::FeeRegime::Low).unwrap();
        // Insert a fee sample so regime detection works
        db.conn().execute(
            "INSERT INTO onchain_fee_samples (feerate_sat_per_vb, sampled_at) VALUES (5.0, ?1)",
            [chrono::Utc::now().timestamp() as f64],
        ).unwrap();

        let mut sched = Scheduler::new_force_all(&config);

        let mut mock = MockLdkClient::new();
        mock.balances = GetBalancesResponse {
            spendable_onchain_balance_sats: 500_000,
            total_onchain_balance_sats: 500_000,
            total_lightning_balance_sats: 0,
            ..Default::default()
        };
        // No existing channels
        mock.channels = ListChannelsResponse { channels: vec![] };

        let result = super::run_cycle(&config, &mock, &db, &mut sched).await;
        assert!(result.is_ok());

        // Should have attempted to open channels
        let open_calls = mock.open_channel_calls.lock().unwrap();
        assert!(
            !open_calls.is_empty(),
            "Autopilot should have opened at least one channel"
        );

        // Verify audit trail
        let audit_count: i64 = db.conn()
            .query_row("SELECT COUNT(*) FROM autopilot_opens", [], |r| r.get(0))
            .unwrap();
        assert!(audit_count > 0, "Autopilot opens should be recorded");
    }

    // -----------------------------------------------------------------------
    // Test 4: Judge closes underperforming peer
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_cycle_judge_closes_underperformer() {
        let db = Database::open_in_memory().unwrap();
        let mut config = test_config();
        config.autopilot.enabled = false;
        config.fees.enabled = false;
        config.rebalancer.enabled = false;
        config.judge.enabled = true;
        config.judge.min_age_days = 0; // Disable age check for test
        config.judge.evaluation_window_days = 365;
        config.judge.estimated_reopen_cost_sats = 50;

        let mut sched = Scheduler::new_force_all(&config);

        // 4 peers, 3 good earners + 1 bad
        let mut mock = MockLdkClient::new();
        mock.channels = ListChannelsResponse {
            channels: vec![
                make_channel("ch1", "good1", 1_000_000, 500_000_000),
                make_channel("ch2", "good2", 1_000_000, 500_000_000),
                make_channel("ch3", "good3", 1_000_000, 500_000_000),
                make_channel("ch4", "bad_peer", 1_000_000, 500_000_000),
            ],
        };
        mock.balances = GetBalancesResponse {
            total_lightning_balance_sats: 4_000_000,
            ..Default::default()
        };

        // Seed channel history (mark all as old enough)
        let old_time = chrono::Utc::now().timestamp() as f64 - 200.0 * 86400.0;
        for (ch_id, peer) in &[("ch1", "good1"), ("ch2", "good2"), ("ch3", "good3"), ("ch4", "bad_peer")] {
            db.conn().execute(
                "INSERT INTO channel_history (channel_id, user_channel_id, counterparty_node_id, \
                 channel_value_sats, first_seen_at, last_seen_at, is_open) \
                 VALUES (?1, ?2, ?3, 1000000, ?4, ?5, 1)",
                rusqlite::params![ch_id, format!("user_{}", ch_id), peer, old_time, old_time + 100.0],
            ).unwrap();
        }

        // Seed earnings: good peers earned a lot, bad peer earned nothing
        let bucket = {
            let now = chrono::Utc::now().timestamp();
            now - (now % 86400)
        };
        for peer in &["good1", "good2", "good3"] {
            db.conn().execute(
                "INSERT INTO earnings (channel_id, counterparty_node_id, day_bucket, \
                 fee_earned_msat, amount_forwarded_msat, direction) \
                 VALUES (?1, ?2, ?3, 10000000, 1000000000, 'in')",
                rusqlite::params![format!("ch_{}", peer), peer, bucket],
            ).unwrap();
        }
        // bad_peer: zero earnings (no row needed)

        let result = super::run_cycle(&config, &mock, &db, &mut sched).await;
        assert!(result.is_ok());

        let close_calls = mock.close_channel_calls.lock().unwrap();
        assert_eq!(close_calls.len(), 1, "Judge should close exactly 1 channel");
        assert_eq!(
            close_calls[0].counterparty_node_id, "bad_peer",
            "Should close the underperforming peer"
        );

        // Verify audit trail
        let closure_count: i64 = db.conn()
            .query_row("SELECT COUNT(*) FROM judge_closures", [], |r| r.get(0))
            .unwrap();
        assert_eq!(closure_count, 1);
    }

    // -----------------------------------------------------------------------
    // Test 5: Dry-run mode makes no API mutations
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_cycle_dry_run_no_mutations() {
        let db = Database::open_in_memory().unwrap();
        let mut config = test_config();
        config.general.dry_run = true;
        config.fees.enabled = true;
        config.fees.price_theory_enabled = false;
        config.autopilot.enabled = true;
        config.judge.enabled = false;
        config.rebalancer.enabled = false;

        // Set low fee regime
        onchain_fees::save_regime(&db, onchain_fees::FeeRegime::Low).unwrap();
        db.conn().execute(
            "INSERT INTO onchain_fee_samples (feerate_sat_per_vb, sampled_at) VALUES (5.0, ?1)",
            [chrono::Utc::now().timestamp() as f64],
        ).unwrap();

        let mut sched = Scheduler::new_force_all(&config);

        let mut mock = MockLdkClient::new();
        mock.channels = ListChannelsResponse {
            channels: vec![
                make_channel("ch1", "peer_a", 1_000_000, 900_000_000),
            ],
        };
        mock.balances = GetBalancesResponse {
            spendable_onchain_balance_sats: 500_000,
            total_onchain_balance_sats: 500_000,
            total_lightning_balance_sats: 1_000_000,
            ..Default::default()
        };

        let result = super::run_cycle(&config, &mock, &db, &mut sched).await;
        assert!(result.is_ok());

        // Dry-run: NO mutations should happen
        assert!(
            mock.update_config_calls.lock().unwrap().is_empty(),
            "Dry-run should not update channel config"
        );
        assert!(
            mock.open_channel_calls.lock().unwrap().is_empty(),
            "Dry-run should not open channels"
        );
        assert!(
            mock.close_channel_calls.lock().unwrap().is_empty(),
            "Dry-run should not close channels"
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: Disabled modules are skipped
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_cycle_skips_disabled_modules() {
        let db = Database::open_in_memory().unwrap();
        let mut config = test_config();
        config.fees.enabled = false;
        config.autopilot.enabled = false;
        config.rebalancer.enabled = false;
        config.judge.enabled = false;

        let mut sched = Scheduler::new_force_all(&config);

        let mut mock = MockLdkClient::new();
        mock.channels = ListChannelsResponse {
            channels: vec![
                make_channel("ch1", "peer_a", 1_000_000, 500_000_000),
            ],
        };
        mock.balances = GetBalancesResponse {
            total_lightning_balance_sats: 1_000_000,
            ..Default::default()
        };

        let result = super::run_cycle(&config, &mock, &db, &mut sched).await;
        assert!(result.is_ok());

        // All modules disabled: no API mutations
        assert!(mock.update_config_calls.lock().unwrap().is_empty());
        assert!(mock.open_channel_calls.lock().unwrap().is_empty());
        assert!(mock.close_channel_calls.lock().unwrap().is_empty());
        assert!(mock.connect_peer_calls.lock().unwrap().is_empty());
    }
}
