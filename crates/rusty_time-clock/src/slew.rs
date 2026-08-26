//! Platform slew arithmetic, with no platform in it.
//!
//! Each OS exposes a different knob — Linux takes a frequency in 2^-16 ppm,
//! Windows takes a per-interrupt increment, macOS takes a one-shot offset and
//! has no frequency knob at all — so each needs a conversion from the
//! discipline's plan into its own units.
//!
//! That conversion is pure arithmetic, and it lives here rather than beside the
//! syscalls so it can be tested **on every host**. This matters most for macOS:
//! it is the one target this project cannot run locally, so if its arithmetic
//! were only compiled on a Mac it would only ever be checked by CI, and only
//! then if CI had a Mac. Here it is checked by every `cargo test` anywhere.

/// Cap on how far ahead a single macOS `adjtime` correction is projected.
pub const MACOS_MAX_HORIZON_S: f64 = 1024.0;

/// Seconds of correction to hand macOS `adjtime` for one plan.
///
/// macOS has no frequency knob, so a frequency correction must be expressed as
/// an offset that will accumulate over the interval before the daemon re-arms.
/// The horizon is however long the offset drain would itself take, and the
/// frequency term is projected across that same span.
pub fn macos_adjtime_amount(freq_ppm: f64, drain_offset: f64, drain_rate_ppm: f64) -> f64 {
    let horizon_s = if drain_rate_ppm > 0.0 {
        (drain_offset.abs() / (drain_rate_ppm * 1e-6)).min(MACOS_MAX_HORIZON_S)
    } else {
        0.0
    };
    drain_offset + freq_ppm * 1e-6 * horizon_s
}

/// Net frequency a driver should command: the discipline's frequency term plus
/// the offset-drain rate, signed by the direction of the drain, clamped to what
/// the platform will accept.
pub fn total_ppm(freq_ppm: f64, drain_offset: f64, drain_rate_ppm: f64, max_ppm: f64) -> f64 {
    let drain = drain_rate_ppm.copysign(drain_offset);
    (freq_ppm + drain).clamp(-max_ppm, max_ppm)
}

/// Windows: convert a frequency correction into the adjustment value the API
/// wants — the per-interrupt increment scaled by (1 + ppm).
///
/// Clamped at zero because the value is unsigned: a clock can be slowed to a
/// stop but never run backwards, and letting it go negative would wrap into an
/// enormous forward jump.
pub fn windows_adjustment(increment: u64, total_ppm: f64) -> u64 {
    let scaled = increment as f64 * (1.0 + total_ppm * 1e-6);
    if scaled <= 0.0 { 0 } else { scaled as u64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_direction_sets_the_sign() {
        // Draining a positive offset means running faster, and vice versa.
        assert!(total_ppm(0.0, 0.010, 50.0, 1e6) > 0.0);
        assert!(total_ppm(0.0, -0.010, 50.0, 1e6) < 0.0);
        // Frequency and drain terms add.
        assert!((total_ppm(10.0, 0.001, 5.0, 1e6) - 15.0).abs() < 1e-9);
    }

    #[test]
    fn total_ppm_respects_the_platform_ceiling() {
        assert_eq!(total_ppm(1e9, 1.0, 1e9, 500.0), 500.0);
        assert_eq!(total_ppm(-1e9, -1.0, 1e9, 500.0), -500.0);
    }

    #[test]
    fn windows_adjustment_scales_the_increment() {
        let increment = 156_250u64; // a typical 15.625 ms tick in 100 ns units
        assert_eq!(windows_adjustment(increment, 0.0), increment);
        let faster = windows_adjustment(increment, 1_000.0);
        assert!((faster as f64 / increment as f64 - 1.001).abs() < 1e-4);
        let slower = windows_adjustment(increment, -1_000.0);
        assert!(slower < increment);
    }

    #[test]
    fn windows_adjustment_never_wraps_negative() {
        // An absurd negative correction must clamp to zero rather than wrap an
        // unsigned value into a huge forward jump.
        assert_eq!(windows_adjustment(1000, -2_000_000.0), 0);
    }

    #[test]
    fn macos_amount_carries_the_offset_when_there_is_no_frequency_term() {
        let amount = macos_adjtime_amount(0.0, 0.005, 100.0);
        assert!((amount - 0.005).abs() < 1e-12);
    }

    #[test]
    fn macos_amount_projects_frequency_over_the_drain_horizon() {
        // Drain 10 ms at 100 ppm takes 100 s; +20 ppm over 100 s adds 2 ms.
        let amount = macos_adjtime_amount(20.0, 0.010, 100.0);
        assert!(
            (amount - 0.012).abs() < 1e-9,
            "expected 12 ms, got {amount}"
        );
    }

    #[test]
    fn macos_horizon_is_bounded() {
        // A tiny drain rate would otherwise project the frequency term across
        // an unbounded span and ask for an absurd one-shot correction.
        let amount = macos_adjtime_amount(500.0, 1.0, 1e-9);
        let worst = 1.0 + 500.0 * 1e-6 * MACOS_MAX_HORIZON_S;
        assert!(
            amount <= worst + 1e-9,
            "amount {amount} exceeded the bounded horizon {worst}"
        );
    }

    #[test]
    fn macos_amount_is_zero_when_there_is_nothing_to_do() {
        assert_eq!(macos_adjtime_amount(0.0, 0.0, 0.0), 0.0);
        // No drain rate means no horizon, so a pure frequency term has nowhere
        // to be projected — the caller re-arms on the next plan instead.
        assert_eq!(macos_adjtime_amount(50.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn macos_sign_follows_the_offset() {
        assert!(macos_adjtime_amount(0.0, -0.005, 100.0) < 0.0);
        assert!(macos_adjtime_amount(0.0, 0.005, 100.0) > 0.0);
    }
}
