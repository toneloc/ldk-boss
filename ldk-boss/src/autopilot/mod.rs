pub mod candidate;
pub mod decider;
pub mod opener;

use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use crate::state::NodeState;
use log::{debug, info};

/// Run the channel autopilot: evaluate whether to open channels, select candidates, execute.
pub async fn run(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    state: &NodeState,
) -> anyhow::Result<()> {
    // Phase 1: Decide if we should open channels
    let budget = match decider::should_open(config, db, state)? {
        Some(budget) => budget,
        None => {
            debug!("Autopilot: conditions not met for channel opening");
            return Ok(());
        }
    };

    info!(
        "Autopilot: budget of {} sats available for new channels",
        budget
    );

    // Phase 2: Select candidates
    let existing_peers: std::collections::HashSet<String> = state
        .channels
        .iter()
        .map(|c| c.counterparty_node_id.clone())
        .collect();

    let candidates = candidate::get_candidates(config, db, &existing_peers).await?;

    if candidates.is_empty() {
        info!("Autopilot: no suitable candidates found");
        return Ok(());
    }

    // Phase 3: Plan channel opens
    let max_proposals = if state.usable_channel_count() >= config.autopilot.min_channels_to_backoff
    {
        1 // Backoff mode: only 1 channel at a time
    } else {
        config.autopilot.max_proposals
    };

    let plan = opener::plan_opens(config, &candidates, budget, max_proposals);

    if plan.is_empty() {
        debug!("Autopilot: no viable opens planned");
        return Ok(());
    }

    info!("Autopilot: planning {} channel opens", plan.len());

    // Phase 4: Execute
    for open in &plan {
        opener::execute_open(config, client, db, open).await?;
    }

    Ok(())
}
