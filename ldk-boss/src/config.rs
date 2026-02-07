use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub autopilot: AutopilotConfig,
    #[serde(default)]
    pub fees: FeesConfig,
    #[serde(default)]
    pub rebalancer: RebalancerConfig,
    #[serde(default)]
    pub judge: JudgeConfig,
    #[serde(default)]
    pub onchain_fees: OnchainFeesConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    /// LDK Server REST endpoint (host:port, no scheme)
    pub base_url: String,
    /// HMAC-SHA256 API key (hex string)
    pub api_key: String,
    /// Path to LDK Server's TLS certificate
    pub tls_cert_path: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct GeneralConfig {
    /// Path to LDKBoss's SQLite database
    #[serde(default = "default_database_path")]
    pub database_path: PathBuf,
    /// Logging level
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Bitcoin network
    #[serde(default = "default_network")]
    pub network: String,
    /// Master enable/disable
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Dry-run mode: log decisions but execute nothing
    #[serde(default)]
    pub dry_run: bool,
    /// Control loop interval in seconds
    #[serde(default = "default_loop_interval")]
    pub loop_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
pub struct AutopilotConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Minimum channels before backoff to single-proposal mode
    #[serde(default = "default_min_channels_to_backoff")]
    pub min_channels_to_backoff: usize,
    /// Maximum proposals per cycle
    #[serde(default = "default_max_proposals")]
    pub max_proposals: usize,
    /// Minimum channel size in satoshis
    #[serde(default = "default_min_channel_sats")]
    pub min_channel_sats: u64,
    /// Maximum channel size in satoshis
    #[serde(default = "default_max_channel_sats")]
    pub max_channel_sats: u64,
    /// On-chain reserve (satoshis) to always keep
    #[serde(default = "default_onchain_reserve")]
    pub onchain_reserve_sats: u64,
    /// Minimum on-chain % before opening channels
    #[serde(default = "default_min_onchain_percent")]
    pub min_onchain_percent: f64,
    /// Max on-chain % before opening even in high-fee regime
    #[serde(default = "default_max_onchain_percent")]
    pub max_onchain_percent: f64,
    /// Whether channels should be announced
    #[serde(default = "default_true")]
    pub announce_channels: bool,
    /// External node ranking API URL (empty = disabled)
    #[serde(default)]
    pub ranking_api_url: String,
    /// Specific nodes to always consider (node_id@host:port)
    #[serde(default)]
    pub seed_nodes: Vec<String>,
    /// Nodes to never open channels with (node_id hex)
    #[serde(default)]
    pub blacklist: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeesConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Default base fee in millisatoshis
    #[serde(default = "default_base_msat")]
    pub default_base_msat: u32,
    /// Default proportional fee in PPM
    #[serde(default = "default_ppm")]
    pub default_ppm: u32,
    /// Enable balance-based fee modulation
    #[serde(default = "default_true")]
    pub balance_modder_enabled: bool,
    /// Preferred bin size for balance modder (satoshis)
    #[serde(default = "default_preferred_bin_size")]
    pub preferred_bin_size_sats: u64,
    /// Enable price theory card-game optimizer
    #[serde(default = "default_true")]
    pub price_theory_enabled: bool,
    /// Card lifetime in ticks (~10min each)
    #[serde(default = "default_card_lifetime")]
    pub price_theory_card_lifetime_ticks: u32,
    /// Max price step from center
    #[serde(default = "default_price_step")]
    pub price_theory_max_step: i32,
}

#[derive(Debug, Deserialize)]
pub struct RebalancerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Probability of triggering per hourly tick (0.0 to 1.0)
    #[serde(default = "default_trigger_probability")]
    pub trigger_probability: f64,
    /// Threshold: channels below this % spendable are destinations
    #[serde(default = "default_max_spendable_percent")]
    pub max_spendable_percent: f64,
    /// Gap to prevent source becoming destination
    #[serde(default = "default_source_gap")]
    pub source_gap_percent: f64,
    /// Target spendable % for destinations
    #[serde(default = "default_target_spendable")]
    pub target_spendable_percent: f64,
    /// Maximum fee per rebalance in PPM
    #[serde(default = "default_rebalance_fee_ppm")]
    pub max_fee_ppm: u32,
    /// Maximum total fee budget per cycle (satoshis)
    #[serde(default = "default_max_total_fee")]
    pub max_total_fee_sats: u64,
}

#[derive(Debug, Deserialize)]
pub struct JudgeConfig {
    /// Disabled by default -- must explicitly opt-in
    #[serde(default)]
    pub enabled: bool,
    /// Minimum channel age in days before judgment
    #[serde(default = "default_min_age_days")]
    pub min_age_days: u64,
    /// Evaluation window in days
    #[serde(default = "default_eval_window")]
    pub evaluation_window_days: u64,
    /// Estimated cost to reopen a channel (satoshis)
    #[serde(default = "default_reopen_cost")]
    pub estimated_reopen_cost_sats: u64,
    /// Use cooperative close (true) or force close (false)
    #[serde(default = "default_true")]
    pub cooperative_close: bool,
}

#[derive(Debug, Deserialize)]
pub struct OnchainFeesConfig {
    /// Provider: "mempool" or "none"
    #[serde(default = "default_fee_provider")]
    pub provider: String,
    /// Mempool.space API URL
    #[serde(default = "default_mempool_url")]
    pub mempool_api_url: String,
    /// Percentile threshold: high -> low fee regime
    #[serde(default = "default_hi_to_lo")]
    pub hi_to_lo_percentile: f64,
    /// Percentile threshold: low -> high fee regime
    #[serde(default = "default_lo_to_hi")]
    pub lo_to_hi_percentile: f64,
}

// Default value functions
fn default_database_path() -> PathBuf {
    PathBuf::from("ldkboss.db")
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_network() -> String {
    "bitcoin".to_string()
}
fn default_true() -> bool {
    true
}
fn default_loop_interval() -> u64 {
    600
}
fn default_min_channels_to_backoff() -> usize {
    4
}
fn default_max_proposals() -> usize {
    5
}
fn default_min_channel_sats() -> u64 {
    100_000
}
fn default_max_channel_sats() -> u64 {
    16_777_215
}
fn default_onchain_reserve() -> u64 {
    30_000
}
fn default_min_onchain_percent() -> f64 {
    10.0
}
fn default_max_onchain_percent() -> f64 {
    25.0
}
fn default_base_msat() -> u32 {
    1000
}
fn default_ppm() -> u32 {
    100
}
fn default_preferred_bin_size() -> u64 {
    200_000
}
fn default_card_lifetime() -> u32 {
    288
}
fn default_price_step() -> i32 {
    2
}
fn default_trigger_probability() -> f64 {
    0.5
}
fn default_max_spendable_percent() -> f64 {
    25.0
}
fn default_source_gap() -> f64 {
    2.5
}
fn default_target_spendable() -> f64 {
    75.0
}
fn default_rebalance_fee_ppm() -> u32 {
    1000
}
fn default_max_total_fee() -> u64 {
    10_000
}
fn default_min_age_days() -> u64 {
    90
}
fn default_eval_window() -> u64 {
    30
}
fn default_reopen_cost() -> u64 {
    5000
}
fn default_fee_provider() -> String {
    "mempool".to_string()
}
fn default_mempool_url() -> String {
    "https://mempool.space/api".to_string()
}
fn default_hi_to_lo() -> f64 {
    17.0
}
fn default_lo_to_hi() -> f64 {
    23.0
}

// Default implementations
impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            database_path: default_database_path(),
            log_level: default_log_level(),
            network: default_network(),
            enabled: true,
            dry_run: false,
            loop_interval_secs: default_loop_interval(),
        }
    }
}

impl Default for AutopilotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_channels_to_backoff: default_min_channels_to_backoff(),
            max_proposals: default_max_proposals(),
            min_channel_sats: default_min_channel_sats(),
            max_channel_sats: default_max_channel_sats(),
            onchain_reserve_sats: default_onchain_reserve(),
            min_onchain_percent: default_min_onchain_percent(),
            max_onchain_percent: default_max_onchain_percent(),
            announce_channels: true,
            ranking_api_url: String::new(),
            seed_nodes: Vec::new(),
            blacklist: Vec::new(),
        }
    }
}

impl Default for FeesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_base_msat: default_base_msat(),
            default_ppm: default_ppm(),
            balance_modder_enabled: true,
            preferred_bin_size_sats: default_preferred_bin_size(),
            price_theory_enabled: true,
            price_theory_card_lifetime_ticks: default_card_lifetime(),
            price_theory_max_step: default_price_step(),
        }
    }
}

impl Default for RebalancerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_probability: default_trigger_probability(),
            max_spendable_percent: default_max_spendable_percent(),
            source_gap_percent: default_source_gap(),
            target_spendable_percent: default_target_spendable(),
            max_fee_ppm: default_rebalance_fee_ppm(),
            max_total_fee_sats: default_max_total_fee(),
        }
    }
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_age_days: default_min_age_days(),
            evaluation_window_days: default_eval_window(),
            estimated_reopen_cost_sats: default_reopen_cost(),
            cooperative_close: true,
        }
    }
}

impl Default for OnchainFeesConfig {
    fn default() -> Self {
        Self {
            provider: default_fee_provider(),
            mempool_api_url: default_mempool_url(),
            hi_to_lo_percentile: default_hi_to_lo(),
            lo_to_hi_percentile: default_lo_to_hi(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        // Hard limits (non-configurable safety rails)
        const ABS_MIN_CHANNEL_SATS: u64 = 20_000;
        const ABS_MAX_CHANNEL_SATS: u64 = 16_777_215;
        const ABS_MAX_FEE_PPM: u32 = 50_000;
        const ABS_MAX_PROPOSALS: usize = 5;

        if self.autopilot.min_channel_sats < ABS_MIN_CHANNEL_SATS {
            anyhow::bail!(
                "min_channel_sats ({}) below absolute minimum ({})",
                self.autopilot.min_channel_sats,
                ABS_MIN_CHANNEL_SATS
            );
        }
        if self.autopilot.max_channel_sats > ABS_MAX_CHANNEL_SATS {
            anyhow::bail!(
                "max_channel_sats ({}) above absolute maximum ({})",
                self.autopilot.max_channel_sats,
                ABS_MAX_CHANNEL_SATS
            );
        }
        if self.autopilot.min_channel_sats > self.autopilot.max_channel_sats {
            anyhow::bail!("min_channel_sats > max_channel_sats");
        }
        if self.autopilot.max_proposals > ABS_MAX_PROPOSALS {
            anyhow::bail!(
                "max_proposals ({}) above absolute maximum ({})",
                self.autopilot.max_proposals,
                ABS_MAX_PROPOSALS
            );
        }
        if self.fees.default_ppm > ABS_MAX_FEE_PPM {
            anyhow::bail!(
                "default_ppm ({}) above absolute maximum ({})",
                self.fees.default_ppm,
                ABS_MAX_FEE_PPM
            );
        }
        if self.rebalancer.trigger_probability < 0.0
            || self.rebalancer.trigger_probability > 1.0
        {
            anyhow::bail!("trigger_probability must be between 0.0 and 1.0");
        }
        if self.rebalancer.max_spendable_percent >= 100.0
            || self.rebalancer.max_spendable_percent <= 0.0
        {
            anyhow::bail!("max_spendable_percent must be between 0 and 100");
        }
        if !self.server.tls_cert_path.exists() {
            anyhow::bail!(
                "TLS cert not found at: {}",
                self.server.tls_cert_path.display()
            );
        }
        Ok(())
    }

    /// Create a config with all defaults for testing purposes.
    /// The TLS cert path is set to the provided path (must exist for validation).
    #[cfg(test)]
    pub fn test_default(tls_cert_path: std::path::PathBuf) -> Self {
        Self {
            server: ServerConfig {
                base_url: "localhost:3002".to_string(),
                api_key: "deadbeef".to_string(),
                tls_cert_path,
            },
            general: GeneralConfig::default(),
            autopilot: AutopilotConfig::default(),
            fees: FeesConfig::default(),
            rebalancer: RebalancerConfig::default(),
            judge: JudgeConfig::default(),
            onchain_fees: OnchainFeesConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_valid_config() -> Config {
        // Use /dev/null as a path that always exists on macOS/Linux
        Config::test_default(std::path::PathBuf::from("/dev/null"))
    }

    #[test]
    fn test_validate_defaults_pass() {
        let config = make_valid_config();
        assert!(config.validate().is_ok(), "{}", config.validate().unwrap_err());
    }

    #[test]
    fn test_validate_min_channel_too_small() {
        let mut config = make_valid_config();
        config.autopilot.min_channel_sats = 10_000; // below ABS_MIN of 20_000
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("min_channel_sats"));
    }

    #[test]
    fn test_validate_max_channel_too_large() {
        let mut config = make_valid_config();
        config.autopilot.max_channel_sats = 20_000_000; // above ABS_MAX of 16_777_215
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_channel_sats"));
    }

    #[test]
    fn test_validate_min_greater_than_max_channel() {
        let mut config = make_valid_config();
        config.autopilot.min_channel_sats = 1_000_000;
        config.autopilot.max_channel_sats = 500_000;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("min_channel_sats > max_channel_sats"));
    }

    #[test]
    fn test_validate_max_proposals_too_high() {
        let mut config = make_valid_config();
        config.autopilot.max_proposals = 10; // above ABS_MAX of 5
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_proposals"));
    }

    #[test]
    fn test_validate_fee_ppm_too_high() {
        let mut config = make_valid_config();
        config.fees.default_ppm = 60_000; // above ABS_MAX of 50_000
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("default_ppm"));
    }

    #[test]
    fn test_validate_trigger_probability_out_of_range() {
        let mut config = make_valid_config();
        config.rebalancer.trigger_probability = 1.5;
        assert!(config.validate().is_err());

        config.rebalancer.trigger_probability = -0.1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_spendable_percent_out_of_range() {
        let mut config = make_valid_config();
        config.rebalancer.max_spendable_percent = 100.0;
        assert!(config.validate().is_err());

        let mut config = make_valid_config();
        config.rebalancer.max_spendable_percent = 0.0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_tls_cert_missing() {
        let mut config = make_valid_config();
        config.server.tls_cert_path = PathBuf::from("/nonexistent/path/cert.pem");
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("TLS cert not found"));
    }

    #[test]
    fn test_toml_deserialize_minimal() {
        let toml_str = r#"
[server]
base_url = "localhost:3002"
api_key = "deadbeef"
tls_cert_path = "/tmp/fake.crt"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.base_url, "localhost:3002");
        // Defaults should be applied
        assert!(config.autopilot.enabled);
        assert!(!config.judge.enabled);
        assert_eq!(config.general.loop_interval_secs, 600);
        assert_eq!(config.fees.default_ppm, 100);
    }
}
