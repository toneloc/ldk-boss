pub mod algo;
pub mod executioner;
pub mod gatherer;

use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use crate::state::NodeState;
use log::{debug, info};

/// Run the peer judge: evaluate channel performance and close underperformers.
pub async fn run(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    state: &NodeState,
) -> anyhow::Result<()> {
    // Gather data for all peers with channels
    let peer_infos = gatherer::gather(config, db, state)?;

    if peer_infos.len() < 3 {
        debug!("Judge: need at least 3 peers to evaluate (have {})", peer_infos.len());
        return Ok(());
    }

    // Run the judgment algorithm
    let recommendations = algo::judge(
        &peer_infos,
        config.judge.estimated_reopen_cost_sats,
    );

    if recommendations.is_empty() {
        debug!("Judge: no channels recommended for closure");
        return Ok(());
    }

    info!(
        "Judge: {} channels recommended for closure",
        recommendations.len()
    );

    // Execute at most 1 closure per cycle (safety rail)
    if let Some(first) = recommendations.first() {
        executioner::execute_closure(config, client, db, state, first).await?;
    }

    Ok(())
}
