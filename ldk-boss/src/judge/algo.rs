/// Port of CLBoss PeerJudge Algo.
///
/// Algorithm:
/// 1. For each peer, compute earned_per_size = total_earned / channel_size
/// 2. Compute weighted median of earned_per_size (weight = channel_size)
/// 3. Peers below median are closure candidates
/// 4. For each candidate:
///    improvement = median_rate * channel_size - actual_earned - reopen_cost
/// 5. If improvement > 0: recommend closure
///
/// Reference: clboss/Boss/Mod/PeerJudge/Algo.cpp, README.md

use log::debug;

/// Information about a peer's channel performance.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub counterparty_node_id: String,
    pub total_channel_sats: u64,
    pub total_earned_msat: i64,
}

/// A recommendation to close a channel.
#[derive(Debug, Clone)]
pub struct CloseRecommendation {
    pub counterparty_node_id: String,
    pub reason: String,
    pub expected_improvement_msat: i64,
}

/// Run the peer judgment algorithm.
pub fn judge(
    peers: &[PeerInfo],
    reopen_cost_sats: u64,
) -> Vec<CloseRecommendation> {
    if peers.is_empty() {
        return Vec::new();
    }

    // Compute earned_per_size for each peer
    let mut rated: Vec<(usize, f64)> = peers
        .iter()
        .enumerate()
        .filter(|(_, p)| p.total_channel_sats > 0)
        .map(|(i, p)| {
            let rate = p.total_earned_msat as f64 / (p.total_channel_sats as f64 * 1000.0);
            (i, rate)
        })
        .collect();

    if rated.is_empty() {
        return Vec::new();
    }

    // Sort by rate
    rated.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    // Compute weighted median (weight = channel_size)
    let median_rate = weighted_median(
        &rated.iter().map(|&(i, rate)| (rate, peers[i].total_channel_sats as f64)).collect::<Vec<_>>(),
    );

    debug!("Judge: weighted median earning rate = {:.6} msat/sat", median_rate);

    let reopen_cost_msat = (reopen_cost_sats * 1000) as i64;

    let mut recommendations = Vec::new();

    for &(idx, rate) in &rated {
        if rate >= median_rate {
            continue; // Above median, skip
        }

        let peer = &peers[idx];

        // Expected earnings if replaced with a median-performing channel
        let expected_earnings = (median_rate * peer.total_channel_sats as f64 * 1000.0) as i64;
        let improvement = expected_earnings - peer.total_earned_msat - reopen_cost_msat;

        if improvement > 0 {
            debug!(
                "Judge: peer {} rate={:.6}, expected={}, actual={}, improvement={}msat",
                peer.counterparty_node_id,
                rate,
                expected_earnings,
                peer.total_earned_msat,
                improvement,
            );
            recommendations.push(CloseRecommendation {
                counterparty_node_id: peer.counterparty_node_id.clone(),
                reason: format!(
                    "Underperforming: earned {} msat vs expected {} msat (improvement: {} msat after {} sat reopen cost)",
                    peer.total_earned_msat, expected_earnings, improvement, reopen_cost_sats
                ),
                expected_improvement_msat: improvement,
            });
        }
    }

    // Sort by improvement descending (close the worst first)
    recommendations.sort_by(|a, b| b.expected_improvement_msat.cmp(&a.expected_improvement_msat));

    recommendations
}

/// Compute the weighted median of a set of (value, weight) pairs.
/// The values must be sorted in ascending order.
fn weighted_median(data: &[(f64, f64)]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    if data.len() == 1 {
        return data[0].0;
    }

    let total_weight: f64 = data.iter().map(|(_, w)| w).sum();
    let half = total_weight / 2.0;

    let mut cumulative = 0.0;
    for &(value, weight) in data {
        cumulative += weight;
        if cumulative >= half {
            return value;
        }
    }

    // Fallback: return the last value
    data.last().unwrap().0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_weighted_median_simple() {
        let data = vec![(1.0, 1.0), (2.0, 1.0), (3.0, 1.0)];
        let median = weighted_median(&data);
        assert!((median - 2.0).abs() < 0.001);
    }

    #[test]
    fn test_weighted_median_weighted() {
        // Heavy weight on the first element should pull median down
        let data = vec![(1.0, 10.0), (2.0, 1.0), (3.0, 1.0)];
        let median = weighted_median(&data);
        assert!((median - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_judge_no_close_when_all_equal() {
        let peers = vec![
            PeerInfo {
                counterparty_node_id: "a".to_string(),
                total_channel_sats: 1_000_000,
                total_earned_msat: 10_000,
            },
            PeerInfo {
                counterparty_node_id: "b".to_string(),
                total_channel_sats: 1_000_000,
                total_earned_msat: 10_000,
            },
            PeerInfo {
                counterparty_node_id: "c".to_string(),
                total_channel_sats: 1_000_000,
                total_earned_msat: 10_000,
            },
        ];
        let recs = judge(&peers, 5000);
        assert!(recs.is_empty(), "Equal performers should not be closed");
    }

    #[test]
    fn test_judge_close_underperformer() {
        // Good peers earn 10M msat each on 1M sat channels.
        // Bad peer earns 0.
        // Reopen cost = 50 sats (50000 msat).
        // Median rate = 10M / (1M*1000) = 0.01 msat/msat.
        // Expected for bad = 0.01 * 1M * 1000 = 10M msat.
        // Improvement = 10M - 0 - 50000 = 9950000 > 0 => close.
        let peers = vec![
            PeerInfo {
                counterparty_node_id: "good1".to_string(),
                total_channel_sats: 1_000_000,
                total_earned_msat: 10_000_000,
            },
            PeerInfo {
                counterparty_node_id: "good2".to_string(),
                total_channel_sats: 1_000_000,
                total_earned_msat: 10_000_000,
            },
            PeerInfo {
                counterparty_node_id: "bad".to_string(),
                total_channel_sats: 1_000_000,
                total_earned_msat: 0,
            },
        ];
        let recs = judge(&peers, 50);
        assert!(!recs.is_empty(), "Zero-earning peer should be recommended for closure");
        assert_eq!(recs[0].counterparty_node_id, "bad");
    }

    #[test]
    fn test_judge_respects_reopen_cost() {
        let peers = vec![
            PeerInfo {
                counterparty_node_id: "good".to_string(),
                total_channel_sats: 100_000,
                total_earned_msat: 1000,
            },
            PeerInfo {
                counterparty_node_id: "ok".to_string(),
                total_channel_sats: 100_000,
                total_earned_msat: 500,
            },
            PeerInfo {
                counterparty_node_id: "bad".to_string(),
                total_channel_sats: 100_000,
                total_earned_msat: 100,
            },
        ];
        // With very high reopen cost, no closure should be recommended
        let recs = judge(&peers, 1_000_000);
        assert!(
            recs.is_empty(),
            "High reopen cost should prevent closures"
        );
    }
}
