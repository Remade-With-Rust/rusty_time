//! The client discipline path, as a deterministic instruction-count workload.
//!
//! `hot_path` covers the server. This covers the other half — the loop that
//! runs on every node: sample register, weighted regression with its two trim
//! passes, the discipline's rate and poll decisions, and the drain bookkeeping.
//! On a mesh where most machines are clients and few are servers, this is the
//! code that actually runs everywhere.
//!
//! The workload is shaped like corpus **S6**: a 500 ms cold start on a LAN
//! path, so it exercises the expensive states rather than only the quiet one —
//! the acquisition burst, the makestep decision, the extended burst, the drain
//! retirement, and then long steady-state operation with a full register. A
//! benchmark that only ever measures a converged loop measures the cheapest
//! thing the loop does.
//!
//! Deterministic by construction: sample offsets, delays and times come from a
//! fixed LCG and a simulated monotonic clock, never from the system. Two runs
//! of the same binary produce the same Ir count to the instruction.
//!
//! The correctness gate is the CHECKSUM, taken over every plan the controller
//! emits — command discriminant, frequency, drain rate, poll interval, and the
//! reported estimate. This path is floating point, so the gate is bit-identity
//! of the f64 payloads rather than a tolerance: an instruction-count
//! optimisation must not change arithmetic at all. If a change is meant to
//! alter the numbers, it belongs in the corpus harness with a paired test, not
//! here.
//!
//! Run:
//!   cargo build --release --bench client_path
//!   valgrind --tool=callgrind --cache-sim=no --branch-sim=no ./client_path

use rusty_time_core::client::SyncController;
use rusty_time_core::filter::Sample;
use rusty_time_core::{ClockCommand, DisciplineConfig};

/// Independent sync sessions. Each starts cold, so the acquisition path is
/// measured many times rather than once.
const SESSIONS: usize = 40;
/// Exchanges per session. Long enough to fill the register and spend most of
/// the run in steady state, which is where a real client lives.
const EXCHANGES: usize = 400;

/// Deterministic jitter source. An LCG, not an RNG: reproducibility is the
/// whole point, and anything seeded from the OS would make the Ir count drift.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() & 0xff_ffff) as f64 / 16_777_216.0
    }

    /// Exponential-ish positive jitter, matching the corpus delay model
    /// (`min + scale * exponential`) closely enough to exercise the same
    /// branches in the weighting and trim passes.
    fn jitter(&mut self) -> f64 {
        let u = self.unit().max(1.0 / 16_777_216.0);
        -u.ln()
    }
}

/// Fold an f64 into a checksum by its exact bits. Bit-identity, not tolerance:
/// this harness gates instruction-count work, which must not move the
/// arithmetic even slightly.
fn fold(sum: &mut u64, v: f64) {
    *sum = sum
        .rotate_left(7)
        .wrapping_mul(0x100_0000_01b3)
        .wrapping_add(v.to_bits());
}

fn main() {
    let mut checksum: u64 = 0xcbf2_9ce4_8422_2325;
    let mut plans: u64 = 0;
    let mut steps: u64 = 0;

    for session in 0..SESSIONS {
        let mut lcg = Lcg(0x5eed_0000 + session as u64);
        let mut controller = SyncController::new(DisciplineConfig::default());

        // S6's shape: half a second out, on a 200 us LAN path with 10 us of
        // jitter each way, and a +20 ppm oscillator.
        let mut true_offset = 0.5;
        let freq_ppm = 20.0;
        let mut applied_ppm = 0.0;
        let mut t = 0.0f64;
        let mut next_poll = 2.0f64;

        for _ in 0..EXCHANGES {
            // Advance the simulated clock to the next exchange, letting the
            // uncorrected drift and whatever correction is running both act.
            t += next_poll;
            true_offset -= (freq_ppm - applied_ppm) * 1e-6 * next_poll;

            let d1 = 100e-6 + 10e-6 * lcg.jitter();
            let d2 = 100e-6 + 10e-6 * lcg.jitter();
            let sample = Sample {
                t,
                // What NTP would measure: the true offset plus the asymmetry
                // it cannot see.
                offset: true_offset + (d1 - d2) / 2.0,
                delay: d1 + d2,
                dispersion: 1e-6,
            };

            // Retire a drain first, exactly as the daemon's loop does, so the
            // bookkeeping path is measured too and not just the sample path.
            // No simulated time passes between retirement and the sample, so
            // the retired rate does not act on the model here — only the plan
            // it produced is folded into the gate.
            if let Some(ClockCommand::Slew { freq_ppm: f, .. }) = controller.poll_drain(t) {
                fold(&mut checksum, f);
                plans += 1;
            }

            let step = controller.on_sample(t, sample);
            steps += 1;
            applied_ppm = step.applied_ppm;

            fold(&mut checksum, step.applied_ppm);
            fold(&mut checksum, step.estimate_offset_s);
            fold(&mut checksum, step.plan.next_poll_s);
            checksum = checksum
                .rotate_left(11)
                .wrapping_add(step.samples_used as u64);
            match step.plan.command {
                ClockCommand::Step { add_seconds } => {
                    // A step moves the clock at once; the model must follow it
                    // or every later sample measures a world the loop already
                    // corrected.
                    true_offset -= add_seconds;
                    checksum = checksum.rotate_left(3).wrapping_add(1);
                    fold(&mut checksum, add_seconds);
                }
                ClockCommand::Slew {
                    freq_ppm: f,
                    drain_offset,
                    drain_rate_ppm,
                } => {
                    checksum = checksum.rotate_left(3).wrapping_add(2);
                    fold(&mut checksum, f);
                    fold(&mut checksum, drain_offset);
                    fold(&mut checksum, drain_rate_ppm);
                }
            }
            plans += 1;
            next_poll = step.plan.next_poll_s;
        }
    }

    println!("sessions     {SESSIONS}");
    println!("steps        {steps}");
    println!("plans        {plans}");
    println!("CHECKSUM {checksum:016x}");
}
