pub mod channels;
pub mod earnings;
pub mod onchain_fees;

use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use crate::state::NodeState;

/// Update all trackers with fresh data from the current cycle.
pub async fn update(
    db: &Database,
    client: &(impl LdkClient + Sync),
    state: &NodeState,
    config: &Config,
) -> anyhow::Result<()> {
    channels::update(db, &state.channels)?;
    earnings::ingest(db, client).await?;
    onchain_fees::update(db, &config.onchain_fees).await?;
    Ok(())
}
