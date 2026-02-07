/// Port of CLBoss FeeModderByBalance.
///
/// Uses an exponential function to set fees based on channel balance:
/// - When we own 50% of the channel: multiplier = 1.0
/// - When we own 100% (all outbound): multiplier ~= 0.14 (cheap, encourage outbound)
/// - When we own 0% (all inbound): multiplier ~= 7.07 (expensive, discourage outbound)
///
/// This naturally encourages rebalancing through market forces:
/// channels with lots of outbound get cheap fees attracting outbound traffic,
/// channels with lots of inbound get expensive fees discouraging further outbound.
///
/// Reference: clboss/Boss/Mod/FeeModderByBalance.cpp

/// Core exponential ratio function from CLBoss.
/// `our_percentage` is 0.0 (0% ours) to 1.0 (100% ours).
pub fn get_ratio(our_percentage: f64) -> f64 {
    let log50 = 50.0_f64.ln();
    (log50 * (0.5 - our_percentage)).exp()
}

/// Get the fee multiplier using bin quantization.
///
/// Bins prevent exact balance leakage through fee observation.
/// The fee is set to the center of the bin, not the exact balance point.
pub fn get_ratio_by_bin(bin: usize, num_bins: usize) -> f64 {
    assert!(bin < num_bins);
    // Center of each bin
    let our_percentage = (1 + bin * 2) as f64 / (num_bins * 2) as f64;
    get_ratio(our_percentage)
}

/// Compute number of bins based on channel size.
/// Larger channels get more bins (finer granularity).
fn get_num_bins(channel_sats: u64, preferred_bin_size_sats: u64) -> usize {
    if preferred_bin_size_sats == 0 {
        return 4;
    }
    let raw = (channel_sats as f64 / preferred_bin_size_sats as f64).round() as usize;
    raw.max(4).min(50)
}

/// Get the bin index for a given balance ratio.
fn get_bin(our_ratio: f64, num_bins: usize) -> f64 {
    our_ratio * num_bins as f64
}

/// Main entry point: compute the fee multiplier for a channel.
///
/// `our_ratio` is outbound_capacity / channel_value (0.0 to 1.0).
/// Returns a multiplier to apply to the base fee/ppm.
pub fn get_ratio_binned(
    our_ratio: f64,
    channel_sats: u64,
    preferred_bin_size_sats: u64,
) -> f64 {
    let num_bins = get_num_bins(channel_sats, preferred_bin_size_sats);
    let actual_bin = get_bin(our_ratio.clamp(0.0, 1.0), num_bins);
    let bin = (actual_bin.floor() as usize).min(num_bins - 1);
    get_ratio_by_bin(bin, num_bins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ratio_at_50_percent() {
        let ratio = get_ratio(0.5);
        assert!((ratio - 1.0).abs() < 0.001, "At 50% should be ~1.0, got {}", ratio);
    }

    #[test]
    fn test_ratio_at_100_percent() {
        let ratio = get_ratio(1.0);
        // exp(log(50) * -0.5) = 1/sqrt(50) ~= 0.1414
        assert!(ratio < 0.2, "At 100% should be cheap (~0.14), got {}", ratio);
        assert!(ratio > 0.1, "At 100% should be ~0.14, got {}", ratio);
    }

    #[test]
    fn test_ratio_at_0_percent() {
        let ratio = get_ratio(0.0);
        // exp(log(50) * 0.5) = sqrt(50) ~= 7.07
        assert!(ratio > 6.0, "At 0% should be expensive (~7.07), got {}", ratio);
        assert!(ratio < 8.0, "At 0% should be ~7.07, got {}", ratio);
    }

    #[test]
    fn test_ratio_monotonic() {
        // As our_percentage increases (more outbound), ratio should decrease (cheaper)
        let r0 = get_ratio(0.0);
        let r25 = get_ratio(0.25);
        let r50 = get_ratio(0.5);
        let r75 = get_ratio(0.75);
        let r100 = get_ratio(1.0);
        assert!(r0 > r25);
        assert!(r25 > r50);
        assert!(r50 > r75);
        assert!(r75 > r100);
    }

    #[test]
    fn test_num_bins() {
        assert_eq!(get_num_bins(100_000, 200_000), 4); // min
        assert_eq!(get_num_bins(1_000_000, 200_000), 5);
        assert_eq!(get_num_bins(10_000_000, 200_000), 50); // max
        assert_eq!(get_num_bins(20_000_000, 200_000), 50); // clamped
    }

    #[test]
    fn test_binned_ratio() {
        // A 1M sat channel at 50% balance
        let ratio = get_ratio_binned(0.5, 1_000_000, 200_000);
        // Should be close to 1.0 (center bin)
        assert!((ratio - 1.0).abs() < 0.5, "Got {}", ratio);
    }
}
