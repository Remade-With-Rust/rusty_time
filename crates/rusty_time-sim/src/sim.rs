//! The simulation engine: a plant (drifting local clock + jittery network +
//! truthful server) driven by the real production discipline stack —
//! `SampleRegister` → `Discipline` → clock commands — exactly as the Linux
//! driver executes them (drain folded into commanded frequency, re-planned each
//! poll; see `rusty_time-clock/src/linux.rs`).
//!
//! Everything is deterministic in (scenario, seed).

use crate::rng::Pcg32;
use crate::scenarios::Scenario;
use rusty_time_core::ntp::offset_delay;
use rusty_time_core::{ClockCommand, Discipline, DisciplineConfig, Sample, SampleRegister};

/// Integration substep for the plant, seconds.
const SUBSTEP_S: f64 = 1.0;
/// A convergence threshold must hold this long to count.
const HOLD_S: f64 = 300.0;
/// Steady-state statistics window: the last quarter of the run.
const STEADY_FRACTION: f64 = 0.25;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct RunMetrics {
    /// First time |err| enters and holds below the threshold, seconds from start.
    pub conv_10ms_s: Option<f64>,
    pub conv_1ms_s: Option<f64>,
    pub conv_100us_s: Option<f64>,
    /// Exchanges completed when 1 ms convergence was reached (network cost).
    pub packets_to_1ms: Option<u32>,
    pub packets_total: u32,
    /// |local − true| over the steady window.
    pub steady_p50_s: f64,
    pub steady_p95_s: f64,
    pub steady_max_s: f64,
    /// Residual true frequency error at end of run, ppm.
    pub freq_resid_ppm: f64,
}

pub fn run(scenario: &Scenario, seed: u64) -> RunMetrics {
    let mut rng = Pcg32::new(seed, 0xC0FFEE);
    let mut t = 0.0_f64; // true time
    let mut err = scenario.initial_offset_s; // local − true
    let mut freq_true_ppm = scenario.base_freq_ppm;
    let mut applied_ppm = 0.0_f64; // driver-commanded total (freq + drain fold)

    let mut register = SampleRegister::new(64);
    let mut discipline = Discipline::new(DisciplineConfig::default());
    // Discipline bookkeeping mirrored by the daemon loop at M3: the pure
    // frequency command and the temporary drain rate are tracked separately,
    // because their register transforms differ (permanent dfreq vs consumed
    // offset). Folding the drain into dfreq lets the slope measurement absorb
    // it and the loop settles into a constant-offset limit cycle — S1 sat
    // pinned at 2 ms until this split existed.
    let mut freq_cmd_ppm = 0.0_f64;
    let mut drain_ppm = 0.0_f64;
    let mut last_plan_mono: Option<f64> = None;

    let mut trajectory: Vec<(f64, f64)> = Vec::with_capacity(scenario.duration_s as usize + 8);
    let mut packets: u32 = 0;
    let mut packets_at: Vec<(f64, u32)> = Vec::new();

    let mut next_poll_at = 0.0_f64;

    while t < scenario.duration_s {
        // ---- poll boundary ----
        if t >= next_poll_at {
            let lost = scenario.loss > 0.0 && rng.uniform() < scenario.loss;
            if !lost {
                packets += 1;
                let mono_now = t + err;

                // The drain that ran since the last plan is a *consumed offset
                // correction*: remove it from stored history as an offset, so
                // the regression slope keeps measuring frequency only.
                if let Some(last) = last_plan_mono {
                    let drained = drain_ppm * 1e-6 * (mono_now - last);
                    if drained != 0.0 {
                        register.slew_samples(mono_now, 0.0, drained);
                    }
                }

                // One exchange. Server clocks are truth; local timestamps carry err.
                let out_delay = scenario.rtt_s / 2.0
                    + scenario.asym_extra_s
                    + rng.normal().abs() * scenario.jitter_sd_s;
                let back_delay = scenario.rtt_s / 2.0 + rng.normal().abs() * scenario.jitter_sd_s;
                let t1 = t + err;
                let t2 = t + out_delay;
                let t3 = t2 + 10e-6; // server processing
                let t4_true = t3 + back_delay;
                let t4 = t4_true + err;
                let (offset, delay) = offset_delay(t1, t2, t3, t4);
                register.push(Sample {
                    t: (t1 + t4) / 2.0,
                    offset,
                    delay,
                    dispersion: 0.0,
                });
                packets_at.push((t, packets));

                // Estimate: regression once warm, best single sample before that.
                let (est_offset, est_freq, est_sd) = match register.regress(mono_now) {
                    Some(e) => (e.offset, e.freq_ppm, e.offset_sd.max(1e-7)),
                    None => match register.best() {
                        Some(b) => (b.offset, None, (b.delay / 2.0).max(1e-7)),
                        None => (0.0, None, 1e-3),
                    },
                };
                let plan = discipline.on_estimate(est_offset, est_freq, est_sd);
                let freq_cmd_new = discipline.freq_ppm();
                match plan.command {
                    ClockCommand::Step { add_seconds } => {
                        err += add_seconds;
                        register.slew_samples(mono_now, freq_cmd_new - freq_cmd_ppm, add_seconds);
                        drain_ppm = 0.0;
                    }
                    ClockCommand::Slew {
                        drain_offset,
                        drain_rate_ppm,
                        ..
                    } => {
                        // Permanent frequency change transforms history as dfreq;
                        // the new drain is accounted when it has actually run
                        // (top of the next plan).
                        register.slew_samples(mono_now, freq_cmd_new - freq_cmd_ppm, 0.0);
                        drain_ppm = drain_rate_ppm.copysign(drain_offset);
                    }
                }
                freq_cmd_ppm = freq_cmd_new;
                applied_ppm = freq_cmd_ppm + drain_ppm;
                last_plan_mono = Some(mono_now);
                next_poll_at = t + plan.next_poll_s;
            } else {
                next_poll_at = t + 2f64.powi(discipline.poll_log2() as i32);
            }
        }

        // ---- plant integration to min(next event, next substep) ----
        let dt = SUBSTEP_S.min(next_poll_at - t).max(1e-3);
        freq_true_ppm += rng.normal() * scenario.wander_ppm_sqrt_s * dt.sqrt();
        // Positive applied_ppm speeds the local clock up, so the disciplined
        // clock's error integrates at (true drift + our correction).
        err += (freq_true_ppm + applied_ppm) * 1e-6 * dt;
        t += dt;
        trajectory.push((t, err));
    }

    finish(
        scenario,
        trajectory,
        packets,
        packets_at,
        freq_true_ppm,
        applied_ppm,
    )
}

fn finish(
    scenario: &Scenario,
    trajectory: Vec<(f64, f64)>,
    packets_total: u32,
    packets_at: Vec<(f64, u32)>,
    freq_true_end_ppm: f64,
    applied_end_ppm: f64,
) -> RunMetrics {
    let mut m = RunMetrics {
        packets_total,
        // Residual: what the disciplined clock still drifts at, ppm.
        freq_resid_ppm: freq_true_end_ppm + applied_end_ppm,
        ..RunMetrics::default()
    };

    m.conv_10ms_s = conv_time(&trajectory, 10e-3);
    m.conv_1ms_s = conv_time(&trajectory, 1e-3);
    m.conv_100us_s = conv_time(&trajectory, 100e-6);
    if let Some(ct) = m.conv_1ms_s {
        m.packets_to_1ms = packets_at
            .iter()
            .find(|(t, _)| *t >= ct)
            .map(|(_, p)| *p)
            .or(Some(packets_total));
    }

    let steady_from = scenario.duration_s * (1.0 - STEADY_FRACTION);
    let mut steady: Vec<f64> = trajectory
        .iter()
        .filter(|(t, _)| *t >= steady_from)
        .map(|(_, e)| e.abs())
        .collect();
    if !steady.is_empty() {
        steady.sort_by(f64::total_cmp);
        let p95_idx = ((steady.len() as f64 * 0.95) as usize).min(steady.len() - 1);
        m.steady_p50_s = steady[steady.len() / 2];
        m.steady_p95_s = steady[p95_idx];
        m.steady_max_s = steady[steady.len() - 1];
    }
    m
}

/// First time |err| goes below `threshold` and stays there for HOLD_S.
fn conv_time(trajectory: &[(f64, f64)], threshold: f64) -> Option<f64> {
    let mut candidate: Option<f64> = None;
    for &(t, e) in trajectory {
        if e.abs() < threshold {
            match candidate {
                None => candidate = Some(t),
                Some(start) => {
                    if t - start >= HOLD_S {
                        return Some(start);
                    }
                }
            }
        } else {
            candidate = None;
        }
    }
    // Held to the end of the run without the full window: count it if it held
    // for at least half the window.
    match (candidate, trajectory.last()) {
        (Some(start), Some(&(t_end, _))) if t_end - start >= HOLD_S / 2.0 => Some(start),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenarios::by_name;

    #[test]
    fn s1_converges_and_is_deterministic() {
        let s1 = by_name("S1").expect("S1");
        let a = run(s1, 7);
        let b = run(s1, 7);
        assert_eq!(
            serde_json::to_string(&a).expect("json"),
            serde_json::to_string(&b).expect("json"),
            "same seed must give identical metrics"
        );
        assert!(a.conv_1ms_s.is_some(), "S1 must reach 1 ms: {a:?}");
        assert!(a.steady_p95_s < 1e-3, "S1 steady p95 {}", a.steady_p95_s);
    }

    #[test]
    fn different_seeds_differ() {
        let s1 = by_name("S1").expect("S1");
        let a = run(s1, 1);
        let b = run(s1, 2);
        assert_ne!(
            serde_json::to_string(&a).expect("json"),
            serde_json::to_string(&b).expect("json")
        );
    }

    #[test]
    fn s8_learns_the_frequency() {
        let s8 = by_name("S8").expect("S8");
        let m = run(s8, 3);
        assert!(
            m.freq_resid_ppm.abs() < 5.0,
            "100 ppm oscillator not tracked: residual {} ppm",
            m.freq_resid_ppm
        );
    }
}
