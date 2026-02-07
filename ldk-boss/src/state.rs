use crate::client::LdkClient;
use crate::db::Database;
use ldk_server_protos::api::{GetBalancesResponse, GetNodeInfoResponse};
use ldk_server_protos::types::Channel;
use log::debug;

/// Shared snapshot of node state collected at the start of each cycle.
pub struct NodeState {
    pub node_info: GetNodeInfoResponse,
    pub balances: GetBalancesResponse,
    pub channels: Vec<Channel>,
}

impl NodeState {
    /// Collect fresh node state from LDK Server.
    pub async fn collect(client: &(impl LdkClient + Sync), _db: &Database) -> anyhow::Result<Self> {
        let node_info = client.get_node_info().await?;
        let balances = client.get_balances().await?;
        let channels_resp = client.list_channels().await?;

        debug!(
            "Collected state: {} channels, {}sat onchain, {}sat lightning",
            channels_resp.channels.len(),
            balances.spendable_onchain_balance_sats,
            balances.total_lightning_balance_sats,
        );

        Ok(Self {
            node_info,
            balances,
            channels: channels_resp.channels,
        })
    }

    /// Total channel capacity in satoshis.
    pub fn total_channel_capacity_sats(&self) -> u64 {
        self.channels.iter().map(|c| c.channel_value_sats).sum()
    }

    /// Total funds (on-chain + lightning).
    pub fn total_funds_sats(&self) -> u64 {
        self.balances.total_onchain_balance_sats + self.balances.total_lightning_balance_sats
    }

    /// On-chain balance as percentage of total.
    pub fn onchain_percent(&self) -> f64 {
        let total = self.total_funds_sats();
        if total == 0 {
            return 100.0;
        }
        (self.balances.spendable_onchain_balance_sats as f64 / total as f64) * 100.0
    }

    /// Number of usable channels.
    pub fn usable_channel_count(&self) -> usize {
        self.channels.iter().filter(|c| c.is_usable).count()
    }

    /// Get channels grouped by counterparty node ID.
    pub fn channels_by_peer(&self) -> std::collections::HashMap<String, Vec<&Channel>> {
        let mut map: std::collections::HashMap<String, Vec<&Channel>> =
            std::collections::HashMap::new();
        for ch in &self.channels {
            map.entry(ch.counterparty_node_id.clone())
                .or_default()
                .push(ch);
        }
        map
    }
}
