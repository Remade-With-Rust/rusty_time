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

/// The kernel's hard ceiling on `ADJ_FREQUENCY`, in ppm.
///
/// Linux clamps `time_freq` to `MAXFREQ` (`kernel/time/ntp.c`), which is
/// 500000 ns/s — that is **500 ppm**, not the ±32768 ppm the width of the
/// scaled field suggests. The field is 2^-16 ppm units and does have room for
/// far more; the kernel simply refuses to use it.
pub const LINUX_MAX_ADJ_FREQ_PPM: f64 = 500.0;

/// How far from nominal the kernel will accept a tick, as a fraction.
///
/// `process_adjtimex_modes` rejects anything outside 900000/USER_HZ ..
/// 1100000/USER_HZ, so ±10%.
pub const LINUX_TICK_RANGE_FRACTION: f64 = 0.1;

/// Split a wanted frequency into a tick value and the residual for
/// `ADJ_FREQUENCY`.
///
/// **Why this exists at all.** 500 ppm cannot drain a 10 ms offset in less
/// than 20 seconds, and cannot drain a 500 ms one in less than 17 minutes, so
/// a driver limited to `ADJ_FREQUENCY` converges far slower than chrony and —
/// worse — silently delivers less correction than the discipline loop was told
/// it would. The controller then subtracts a drain that never happened from
/// its sample history, the regression reads the shortfall as a frequency
/// error, and the loop winds itself up to the frequency clamp and overshoots.
/// That is not a hypothetical: it is what the first cross-implementation run
/// against chrony produced — a 10 ms start overshooting to −8.8 ms.
///
/// The tick — how much the kernel adds per timer interrupt — carries the
/// coarse part, and `ADJ_FREQUENCY` trims what is left. chrony's Linux driver
/// does exactly this, and it is the only way to get a usable slew range.
///
/// Returns `(tick_us, residual_ppm)`, with the residual always inside the
/// kernel's `ADJ_FREQUENCY` limit.
pub fn linux_tick_and_freq(total_ppm: f64, nominal_tick_us: i64) -> (i64, f64) {
    if nominal_tick_us <= 0 {
        return (
            nominal_tick_us,
            total_ppm.clamp(-LINUX_MAX_ADJ_FREQ_PPM, LINUX_MAX_ADJ_FREQ_PPM),
        );
    }
    let ppm_per_us = 1e6 / nominal_tick_us as f64;
    let max_offset_us = (nominal_tick_us as f64 * LINUX_TICK_RANGE_FRACTION).floor() as i64;
    let reachable = max_offset_us as f64 * ppm_per_us + LINUX_MAX_ADJ_FREQ_PPM;
    let wanted = total_ppm.clamp(-reachable, reachable);

    // Leave the tick alone unless the frequency knob genuinely cannot cover
    // the request: changing the tick perturbs jiffies-based timekeeping, so it
    // is a tool for range, not for everyday trimming.
    let mut offset_us = 0i64;
    if wanted.abs() > LINUX_MAX_ADJ_FREQ_PPM {
        offset_us = (wanted / ppm_per_us).round() as i64;
        offset_us = offset_us.clamp(-max_offset_us, max_offset_us);
    }
    let residual = (wanted - offset_us as f64 * ppm_per_us)
        .clamp(-LINUX_MAX_ADJ_FREQ_PPM, LINUX_MAX_ADJ_FREQ_PPM);
    (nominal_tick_us + offset_us, residual)
}

/// The widest frequency this kernel can actually be driven at, given its tick.
pub fn linux_max_slew_ppm(nominal_tick_us: i64) -> f64 {
    if nominal_tick_us <= 0 {
        return LINUX_MAX_ADJ_FREQ_PPM;
    }
    let ppm_per_us = 1e6 / nominal_tick_us as f64;
    (nominal_tick_us as f64 * LINUX_TICK_RANGE_FRACTION).floor() * ppm_per_us
        + LINUX_MAX_ADJ_FREQ_PPM
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

    /// The nominal tick for the usual USER_HZ of 100.
    const TICK_100HZ: i64 = 10_000;

    #[test]
    fn small_corrections_leave_the_tick_alone() {
        // Under the kernel's frequency ceiling there is no reason to disturb
        // the tick, and doing so would perturb jiffies-based timekeeping.
        for ppm in [0.0, 1.0, -50.0, 499.0, -499.0] {
            let (tick, residual) = linux_tick_and_freq(ppm, TICK_100HZ);
            assert_eq!(tick, TICK_100HZ, "tick moved for {ppm} ppm");
            assert!((residual - ppm).abs() < 1e-9);
        }
    }

    #[test]
    fn large_corrections_are_delivered_in_full() {
        // The whole point: what the driver commands must equal what was asked
        // for. This is the assertion whose absence let the daemon believe it
        // was slewing at 1716 ppm while the kernel delivered 500.
        for ppm in [1_716.0, -1_716.0, 5_000.0, -20_000.0, 83_333.0] {
            let (tick, residual) = linux_tick_and_freq(ppm, TICK_100HZ);
            let delivered = (tick - TICK_100HZ) as f64 * (1e6 / TICK_100HZ as f64) + residual;
            assert!(
                (delivered - ppm).abs() < 1e-6,
                "asked {ppm} ppm, driver would deliver {delivered} ppm"
            );
        }
    }

    #[test]
    fn the_residual_always_fits_the_kernels_frequency_limit() {
        // A residual over MAXFREQ is silently clamped by the kernel, which is
        // exactly the lie this module exists to stop telling.
        for tick in [1_000i64, 10_000, 4_000] {
            for ppm in [0.0, 600.0, -600.0, 1e5, -1e5, 1e9, -1e9] {
                let (_, residual) = linux_tick_and_freq(ppm, tick);
                assert!(
                    residual.abs() <= LINUX_MAX_ADJ_FREQ_PPM + 1e-9,
                    "tick {tick}, {ppm} ppm left residual {residual}"
                );
            }
        }
    }

    #[test]
    fn the_tick_stays_inside_what_the_kernel_will_accept() {
        // Outside ±10% adjtimex returns EINVAL and the correction is lost.
        for tick in [1_000i64, 10_000, 4_000] {
            for ppm in [1e9, -1e9, 250_000.0] {
                let (out, _) = linux_tick_and_freq(ppm, tick);
                assert!(
                    out >= 900_000 / (1_000_000 / tick) && out <= 1_100_000 / (1_000_000 / tick),
                    "tick {out} outside the kernel's window for nominal {tick}"
                );
            }
        }
    }

    #[test]
    fn the_advertised_ceiling_is_one_the_driver_can_reach() {
        // capabilities().max_slew_ppm feeds the discipline's own limit, so an
        // over-claim here is what makes the controller command the impossible.
        for tick in [1_000i64, 10_000, 4_000] {
            let ceiling = linux_max_slew_ppm(tick);
            let (out, residual) = linux_tick_and_freq(ceiling, tick);
            let delivered = (out - tick) as f64 * (1e6 / tick as f64) + residual;
            assert!(
                (delivered - ceiling).abs() < 1e-6,
                "advertised {ceiling} ppm but could only deliver {delivered}"
            );
        }
        // And it is a real improvement over the frequency knob alone.
        assert!(linux_max_slew_ppm(TICK_100HZ) > 99_000.0);
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
