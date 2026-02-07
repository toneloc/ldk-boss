pub mod earnings;

use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use crate::state::NodeState;
use log::debug;

/// Run the rebalancer: identify imbalanced channels and attempt circular rebalancing.
pub async fn run(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    state: &NodeState,
) -> anyhow::Result<()> {
    let usable: Vec<_> = state.channels.iter().filter(|c| c.is_usable).collect();

    if usable.len() < 2 {
        debug!("Rebalancer: need at least 2 usable channels");
        return Ok(());
    }

    earnings::run(config, client, db, &usable).await
}
