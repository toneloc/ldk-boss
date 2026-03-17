/// Advisory mode: collects recommendations from all modules and prints
/// a structured report without executing any actions.

use crate::autopilot::{candidate, decider, opener};
use crate::client::LdkClient;
use crate::config::Config;
use crate::db::Database;
use crate::fees::{balance_modder, competitor, price_theory, size_modder, ABS_MAX_FEE_PPM, ABS_MIN_FEE_PPM};
use crate::judge::{algo as judge_algo, gatherer as judge_gatherer};
use crate::state::NodeState;
use crate::tracker::earnings as earnings_tracker;
use serde::Serialize;
use std::collections::HashSet;

// ───────────────────────────────────────────────────────────
// Advisory data structures
// ───────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct Advisory {
    pub timestamp: String,
    pub node_id: String,
    pub total_capacity_sats: u64,
    pub onchain_sats: u64,
    pub channel_count: usize,
    pub fees: Vec<FeeAdvice>,
    pub opens: Vec<OpenAdvice>,
    pub closes: Vec<CloseAdvice>,
    pub rebalances: Vec<RebalanceAdvice>,
    pub reconnects: Vec<ReconnectAdvice>,
}

#[derive(Serialize)]
pub struct FeeAdvice {
    pub channel_id: String,
    pub peer: String,
    pub channel_sats: u64,
    pub current_base_msat: u32,
    pub current_ppm: u32,
    pub suggested_base_msat: u32,
    pub suggested_ppm: u32,
    pub balance_mult: f64,
    pub price_mult: f64,
    pub size_mult: f64,
    pub competitor_base_ppm: Option<u32>,
    pub changed: bool,
}

#[derive(Serialize)]
pub struct OpenAdvice {
    pub node_id: String,
    pub address: String,
    pub amount_sats: u64,
    pub source: String,
    pub score: f64,
}

#[derive(Serialize)]
pub struct CloseAdvice {
    pub peer: String,
    pub channel_sats: u64,
    pub earned_msat: i64,
    pub expected_msat: i64,
    pub improvement_msat: i64,
    pub reason: String,
}

#[derive(Serialize)]
pub struct RebalanceAdvice {
    pub source_peer: String,
    pub source_spendable_pct: f64,
    pub dest_peer: String,
    pub dest_spendable_pct: f64,
    pub amount_msat: u64,
    pub max_fee_msat: u64,
}

#[derive(Serialize)]
pub struct ReconnectAdvice {
    pub peer: String,
    pub address: String,
}

// ───────────────────────────────────────────────────────────
// Collection logic
// ───────────────────────────────────────────────────────────

pub async fn collect(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    state: &NodeState,
) -> anyhow::Result<Advisory> {
    let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string();
    let node_id = state.node_info.node_id.clone();
    let own_node_id = &state.node_info.node_id;
    let own_capacity_sats = state.total_channel_capacity_sats();

    let fees = collect_fee_advice(config, client, db, state, own_node_id, own_capacity_sats).await;
    let opens = collect_open_advice(config, client, db, state).await;
    let closes = collect_close_advice(config, db, state);
    let rebalances = collect_rebalance_advice(config, db, state);
    let reconnects = collect_reconnect_advice(state, db);

    Ok(Advisory {
        timestamp,
        node_id,
        total_capacity_sats: own_capacity_sats,
        onchain_sats: state.balances.spendable_onchain_balance_sats,
        channel_count: state.channels.len(),
        fees,
        opens,
        closes,
        rebalances,
        reconnects,
    })
}

async fn collect_fee_advice(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    state: &NodeState,
    own_node_id: &str,
    own_capacity_sats: u64,
) -> Vec<FeeAdvice> {
    let mut advice = Vec::new();

    for channel in state.channels.iter().filter(|c| c.is_usable) {
        let channel_value_sats = channel.channel_value_sats;
        if channel_value_sats == 0 {
            continue;
        }

        // Competitor baseline
        let (base_ppm, base_base_msat, competitor_ppm) = if config.fees.competitor_fee_enabled {
            match competitor::get_competitor_fees(
                client,
                &channel.counterparty_node_id,
                own_node_id,
            )
            .await
            {
                Some(cf) => (cf.median_ppm, cf.median_base_msat, Some(cf.median_ppm)),
                None => (config.fees.default_ppm, config.fees.default_base_msat, None),
            }
        } else {
            (config.fees.default_ppm, config.fees.default_base_msat, None)
        };

        // Balance ratio
        let our_balance_ratio = channel.outbound_capacity_msat as f64
            / (channel_value_sats as f64 * 1000.0);

        let balance_mult = if config.fees.balance_modder_enabled {
            balance_modder::get_ratio_binned(
                our_balance_ratio,
                channel_value_sats,
                config.fees.preferred_bin_size_sats,
            )
        } else {
            1.0
        };

        let price_mult = if config.fees.price_theory_enabled {
            price_theory::get_fee_modifier(db, &channel.counterparty_node_id).unwrap_or(1.0)
        } else {
            1.0
        };

        let size_mult = if config.fees.size_modder_enabled {
            size_modder::get_size_modifier(
                client,
                &channel.counterparty_node_id,
                own_node_id,
                own_capacity_sats,
            )
            .await
            .unwrap_or(1.0)
        } else {
            1.0
        };

        let combined = balance_mult * price_mult * size_mult;
        let base_msat = ((base_base_msat as f64) * combined) as u32;
        let ppm = ((base_ppm as f64) * combined) as u32;
        let ppm = ppm.clamp(ABS_MIN_FEE_PPM, ABS_MAX_FEE_PPM);

        let current = channel.channel_config.as_ref();
        let current_base = current.and_then(|c| c.forwarding_fee_base_msat).unwrap_or(0);
        let current_ppm = current
            .and_then(|c| c.forwarding_fee_proportional_millionths)
            .unwrap_or(0);

        advice.push(FeeAdvice {
            channel_id: channel.channel_id.clone(),
            peer: channel.counterparty_node_id.clone(),
            channel_sats: channel_value_sats,
            current_base_msat: current_base,
            current_ppm,
            suggested_base_msat: base_msat,
            suggested_ppm: ppm,
            balance_mult,
            price_mult,
            size_mult,
            competitor_base_ppm: competitor_ppm,
            changed: current_base != base_msat || current_ppm != ppm,
        });
    }

    advice
}

async fn collect_open_advice(
    config: &Config,
    client: &(impl LdkClient + Sync),
    db: &Database,
    state: &NodeState,
) -> Vec<OpenAdvice> {
    let budget = match decider::should_open(config, db, state) {
        Ok(Some(b)) => b,
        _ => return Vec::new(),
    };

    let existing_peers: HashSet<String> = state
        .channels
        .iter()
        .map(|c| c.counterparty_node_id.clone())
        .collect();

    let candidates = match candidate::get_candidates(config, client, db, &existing_peers).await {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    if candidates.is_empty() {
        return Vec::new();
    }

    let max_proposals = if state.usable_channel_count() >= config.autopilot.min_channels_to_backoff
    {
        1
    } else {
        config.autopilot.max_proposals
    };

    let plan = opener::plan_opens(config, &candidates, budget, max_proposals);

    plan.into_iter()
        .map(|p| OpenAdvice {
            node_id: p.candidate.node_id,
            address: p.candidate.address,
            amount_sats: p.amount_sats,
            source: format!("{:?}", p.candidate.source),
            score: p.candidate.score,
        })
        .collect()
}

fn collect_close_advice(config: &Config, db: &Database, state: &NodeState) -> Vec<CloseAdvice> {
    let peer_infos = match judge_gatherer::gather(config, db, state) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    if peer_infos.len() < 3 {
        return Vec::new();
    }

    let recs = judge_algo::judge(&peer_infos, config.judge.estimated_reopen_cost_sats);

    recs.into_iter()
        .map(|r| {
            let peer_info = peer_infos
                .iter()
                .find(|p| p.counterparty_node_id == r.counterparty_node_id);
            let (channel_sats, earned_msat) = peer_info
                .map(|p| (p.total_channel_sats, p.total_earned_msat))
                .unwrap_or((0, 0));

            CloseAdvice {
                peer: r.counterparty_node_id,
                channel_sats,
                earned_msat,
                expected_msat: earned_msat + r.expected_improvement_msat,
                improvement_msat: r.expected_improvement_msat,
                reason: r.reason,
            }
        })
        .collect()
}

fn collect_rebalance_advice(config: &Config, db: &Database, state: &NodeState) -> Vec<RebalanceAdvice> {
    let usable: Vec<_> = state.channels.iter().filter(|c| c.is_usable).collect();
    if usable.len() < 2 {
        return Vec::new();
    }

    let max_spendable = config.rebalancer.max_spendable_percent;
    let source_gap = config.rebalancer.source_gap_percent;
    let target_pct = config.rebalancer.target_spendable_percent;
    let max_fee_ppm = config.rebalancer.max_fee_ppm;
    let since = chrono::Utc::now().timestamp() as f64 - 30.0 * 86400.0;

    struct Bal {
        peer: String,
        spendable_msat: u64,
        total_msat: u64,
        spendable_pct: f64,
    }

    let balances: Vec<Bal> = usable
        .iter()
        .filter_map(|ch| {
            let total_msat = ch.channel_value_sats * 1000;
            if total_msat == 0 {
                return None;
            }
            let spendable_pct = (ch.outbound_capacity_msat as f64 / total_msat as f64) * 100.0;
            Some(Bal {
                peer: ch.counterparty_node_id.clone(),
                spendable_msat: ch.outbound_capacity_msat,
                total_msat,
                spendable_pct,
            })
        })
        .collect();

    let mut destinations: Vec<(usize, i64)> = Vec::new();
    let mut sources: Vec<(usize, i64)> = Vec::new();

    for (i, bal) in balances.iter().enumerate() {
        let peer_earnings =
            match earnings_tracker::peer_earnings_since(db, &bal.peer, since) {
                Ok(e) => e,
                Err(_) => continue,
            };

        if bal.spendable_pct < max_spendable {
            destinations.push((i, peer_earnings.out_net()));
        } else if bal.spendable_pct > max_spendable + source_gap {
            sources.push((i, peer_earnings.in_net()));
        }
    }

    if destinations.is_empty() || sources.is_empty() {
        return Vec::new();
    }

    destinations.sort_by(|a, b| b.1.cmp(&a.1));
    sources.sort_by(|a, b| b.1.cmp(&a.1));

    let num = destinations.len().min(sources.len());
    let num_rebalance = ((num as f64 * 20.0 / 100.0) as usize).max(1);

    let mut advice = Vec::new();

    for i in 0..num_rebalance {
        let (dst_idx, dst_earnings) = destinations[i];
        let (src_idx, _) = sources[i];

        if dst_earnings <= 0 {
            break;
        }

        let dst = &balances[dst_idx];
        let src = &balances[src_idx];

        let dest_target_msat = (dst.total_msat as f64 * target_pct / 100.0) as u64;
        let dest_needed_msat = dest_target_msat.saturating_sub(dst.spendable_msat);

        let src_min_allowed_msat =
            (src.total_msat as f64 * (max_spendable + source_gap) / 100.0) as u64;
        let src_budget_msat = src.spendable_msat.saturating_sub(src_min_allowed_msat);

        let amount_msat = dest_needed_msat.min(src_budget_msat);
        if amount_msat == 0 {
            continue;
        }

        let fee_budget_msat = (amount_msat as f64 * max_fee_ppm as f64 / 1_000_000.0) as u64;
        let fee_budget_msat = fee_budget_msat.min(dst_earnings as u64);

        if fee_budget_msat == 0 {
            continue;
        }

        advice.push(RebalanceAdvice {
            source_peer: src.peer.clone(),
            source_spendable_pct: src.spendable_pct,
            dest_peer: dst.peer.clone(),
            dest_spendable_pct: dst.spendable_pct,
            amount_msat,
            max_fee_msat: fee_budget_msat,
        });
    }

    advice
}

fn collect_reconnect_advice(state: &NodeState, db: &Database) -> Vec<ReconnectAdvice> {
    let disconnected: Vec<_> = state
        .channels
        .iter()
        .filter(|ch| ch.is_channel_ready && !ch.is_usable)
        .collect();

    let conn = db.conn();
    let mut advice = Vec::new();

    for ch in disconnected {
        if advice.iter().any(|a: &ReconnectAdvice| a.peer == ch.counterparty_node_id) {
            continue;
        }

        let address: String = conn
            .query_row(
                "SELECT address FROM peer_addresses WHERE node_id = ?1",
                [&ch.counterparty_node_id],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "unknown".to_string());

        advice.push(ReconnectAdvice {
            peer: ch.counterparty_node_id.clone(),
            address,
        });
    }

    advice
}

// ───────────────────────────────────────────────────────────
// Display
// ───────────────────────────────────────────────────────────

impl Advisory {
    pub fn print_text(&self) {
        let w = 60;
        println!("{}", "=".repeat(w));
        println!("  LDKBoss Advisory Report");
        println!("  Node: {}", abbreviate(&self.node_id));
        println!("  {}", self.timestamp);
        println!(
            "  Capacity: {} sat across {} channels, {} sat on-chain",
            fmt_sats(self.total_capacity_sats),
            self.channel_count,
            fmt_sats(self.onchain_sats),
        );
        println!("{}", "=".repeat(w));

        // Fees
        let fee_changes: Vec<_> = self.fees.iter().filter(|f| f.changed).collect();
        println!();
        println!("-- Fees ({} changes) {}", fee_changes.len(), "-".repeat(w.saturating_sub(22)));
        if fee_changes.is_empty() {
            println!("  No changes recommended.");
        }
        for f in &fee_changes {
            println!(
                "  {} with {}",
                abbreviate(&f.channel_id),
                abbreviate(&f.peer),
            );
            println!(
                "    {} sat channel, {:.0}% outbound",
                fmt_sats(f.channel_sats),
                // Rough estimate from balance_mult (inverse of the formula)
                if f.balance_mult > 0.0 {
                    50.0 - (f.balance_mult.ln() / 50.0_f64.ln())
                } else {
                    50.0
                } * 100.0
            );
            println!(
                "    Current:   {} msat base, {} ppm",
                f.current_base_msat, f.current_ppm
            );
            println!(
                "    Suggested: {} msat base, {} ppm",
                f.suggested_base_msat, f.suggested_ppm
            );
            println!(
                "    Modifiers: balance={:.2}x price={:.2}x size={:.2}x{}",
                f.balance_mult,
                f.price_mult,
                f.size_mult,
                f.competitor_base_ppm
                    .map(|p| format!(" competitor={}ppm", p))
                    .unwrap_or_default(),
            );
        }

        // Opens
        println!();
        println!("-- Open Channels ({}) {}", self.opens.len(), "-".repeat(w.saturating_sub(25)));
        if self.opens.is_empty() {
            println!("  No opens recommended.");
        }
        for (i, o) in self.opens.iter().enumerate() {
            println!(
                "  {}. Open {} sat with {}",
                i + 1,
                fmt_sats(o.amount_sats),
                abbreviate(&o.node_id),
            );
            println!("     Address: {}", o.address);
            println!("     Source:  {} (score: {:.1})", o.source, o.score);
        }

        // Closes
        println!();
        println!("-- Close Channels ({}) {}", self.closes.len(), "-".repeat(w.saturating_sub(26)));
        if self.closes.is_empty() {
            println!("  No closes recommended.");
        }
        for (i, c) in self.closes.iter().enumerate() {
            println!(
                "  {}. Close {} with {} ({} sat)",
                i + 1,
                abbreviate(&c.peer),
                fmt_sats(c.channel_sats),
                c.channel_sats,
            );
            println!(
                "     Earned: {} msat vs expected {} msat",
                c.earned_msat, c.expected_msat
            );
            println!("     Improvement: {} msat after reopen cost", c.improvement_msat);
        }

        // Rebalances
        println!();
        println!("-- Rebalance ({}) {}", self.rebalances.len(), "-".repeat(w.saturating_sub(21)));
        if self.rebalances.is_empty() {
            println!("  No rebalances recommended.");
        }
        for (i, r) in self.rebalances.iter().enumerate() {
            println!(
                "  {}. Move ~{} sat toward {}",
                i + 1,
                fmt_sats(r.amount_msat / 1000),
                abbreviate(&r.dest_peer),
            );
            println!(
                "     Dest:   {} ({:.0}% spendable, needs outbound)",
                abbreviate(&r.dest_peer),
                r.dest_spendable_pct,
            );
            println!(
                "     Source:  {} ({:.0}% spendable, excess outbound)",
                abbreviate(&r.source_peer),
                r.source_spendable_pct,
            );
            println!("     Max fee: {} sat", r.max_fee_msat / 1000);
        }

        // Reconnects
        println!();
        println!("-- Reconnect ({}) {}", self.reconnects.len(), "-".repeat(w.saturating_sub(22)));
        if self.reconnects.is_empty() {
            println!("  All peers connected.");
        }
        for r in &self.reconnects {
            println!("  {} at {}", abbreviate(&r.peer), r.address);
        }

        // Summary
        let total_actions = fee_changes.len()
            + self.opens.len()
            + self.closes.len()
            + self.rebalances.len()
            + self.reconnects.len();
        println!();
        println!("{}", "=".repeat(w));
        println!(
            "  Summary: {} fee changes, {} opens, {} closes, {} rebalances, {} reconnects",
            fee_changes.len(),
            self.opens.len(),
            self.closes.len(),
            self.rebalances.len(),
            self.reconnects.len(),
        );
        if total_actions == 0 {
            println!("  Nothing to do. Node is healthy.");
        }
        println!("{}", "=".repeat(w));
    }

    pub fn print_json(&self) {
        println!(
            "{}",
            serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\": \"{}\"}}", e))
        );
    }
}

fn abbreviate(s: &str) -> String {
    if s.len() > 16 {
        format!("{}...{}", &s[..8], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

fn fmt_sats(sats: u64) -> String {
    if sats >= 100_000_000 {
        format!("{:.2} BTC", sats as f64 / 100_000_000.0)
    } else if sats >= 1_000_000 {
        format!("{:.1}M", sats as f64 / 1_000_000.0)
    } else if sats >= 1_000 {
        format!("{}k", sats / 1_000)
    } else {
        format!("{}", sats)
    }
}
