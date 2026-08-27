#![no_main]
//! The discipline loop against adversarial-but-legal sample sequences.
//!
//! The three existing targets fuzz PARSERS. Nothing fuzzed the loop those
//! parsers feed — the sample register, the weighted regression with its two
//! trim passes, and the discipline that turns their output into a clock
//! command. That is the code with the most arithmetic in it and the most to go
//! wrong, and it is the code that was rewritten hardest.
//!
//! **The property under test is that a clock command is never nonsense.** A
//! `SyncController` that emits a NaN frequency does not merely compute a poor
//! answer: `clock_adjtime` receives it through an `as i64` conversion, which in
//! Rust saturates rather than trapping, so a NaN becomes 0 and an infinity
//! becomes `i64::MAX` — the daemon would set the machine's clock from a
//! quantity it never intended to produce. Nothing downstream re-checks it.
//!
//! Samples are constrained to what `rtimed sync` will actually admit, because
//! that is the real contract: `exchange()` already rejects stratum 0 and
//! stratum > 15, an unsynchronised leap indicator, zero timestamps, and a
//! negative round-trip delay. Fuzzing values the daemon refuses to build would
//! test unreachable code and hide real failures among false ones. What is left
//! free is everything an authenticated but hostile — or simply broken — server
//! can still choose: enormous offsets, zero delay, huge delay, samples arriving
//! at any spacing, and long runs of them.

use libfuzzer_sys::fuzz_target;
use rusty_time_core::client::SyncController;
use rusty_time_core::filter::Sample;
use rusty_time_core::{ClockCommand, DisciplineConfig};

/// Pull a f64 in `[lo, hi]` out of the fuzzer's bytes.
fn scaled(bytes: &[u8], at: usize, lo: f64, hi: f64) -> f64 {
    let mut raw = [0u8; 8];
    for (i, slot) in raw.iter_mut().enumerate() {
        *slot = bytes.get(at + i).copied().unwrap_or(0);
    }
    let unit = u64::from_le_bytes(raw) as f64 / u64::MAX as f64;
    lo + unit * (hi - lo)
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 16 {
        return;
    }

    let mut cfg = DisciplineConfig::default();
    // Poll bounds are operator-controlled, so vary them within their legal
    // range rather than pinning the defaults.
    cfg.min_poll = (data[0] % 7) as i8 + 4;
    cfg.max_poll = cfg.min_poll + (data[1] % 7) as i8;
    if data[2] & 1 == 0 {
        cfg.makestep_threshold = None;
    }
    cfg.iburst = data[2] & 2 == 0;

    let mut controller = SyncController::new(cfg);
    let mut t = 0.0f64;

    let body = &data[3..];
    let per = 10usize;
    for chunk in 0..(body.len() / per).min(512) {
        let at = chunk * per;

        // Spacing: anything from a burst to a long silence. Never negative —
        // the register is a time series and the daemon reads a monotonic clock.
        t += scaled(body, at, 0.0, 4096.0);

        // What a server can still choose after validation: an offset from
        // microseconds to a century, and a delay from zero to an hour.
        let offset = scaled(body, at + 2, -3.15e9, 3.15e9);
        let delay = scaled(body, at + 5, 0.0, 3600.0);
        let dispersion = scaled(body, at + 8, 0.0, 16.0);

        // Mirror `exchange()`'s admission rules exactly.
        if !offset.is_finite() || !delay.is_finite() || delay < 0.0 {
            continue;
        }

        // Retiring a spent drain is part of the daemon's loop, so it is part of
        // the sequence under test.
        if let Some(cmd) = controller.poll_drain(t) {
            check(&cmd, &controller);
        }

        let step = controller.on_sample(
            t,
            Sample {
                t,
                offset,
                delay,
                dispersion,
            },
        );
        check(&step.plan.command, &controller);

        assert!(
            step.plan.next_poll_s.is_finite() && step.plan.next_poll_s > 0.0,
            "poll interval {} is not a usable delay",
            step.plan.next_poll_s
        );
        assert!(
            step.applied_ppm.is_finite(),
            "applied frequency {} is not finite",
            step.applied_ppm
        );
        assert!(
            step.estimate_offset_s.is_finite(),
            "reported offset {} is not finite",
            step.estimate_offset_s
        );
    }
});

/// Every clock command must be something a driver can actually execute.
fn check(cmd: &ClockCommand, controller: &SyncController) {
    match *cmd {
        ClockCommand::Step { add_seconds } => {
            assert!(
                add_seconds.is_finite(),
                "step of {add_seconds} would be handed to the system clock"
            );
        }
        ClockCommand::Slew {
            freq_ppm,
            drain_offset,
            drain_rate_ppm,
        } => {
            assert!(freq_ppm.is_finite(), "slew frequency {freq_ppm} is not finite");
            assert!(
                drain_offset.is_finite(),
                "drain offset {drain_offset} is not finite"
            );
            assert!(
                drain_rate_ppm.is_finite(),
                "drain rate {drain_rate_ppm} is not finite"
            );
            assert!(
                drain_rate_ppm >= 0.0,
                "drain rate {drain_rate_ppm} is negative; the sign belongs to the offset"
            );
        }
    }
    assert!(
        controller.freq_ppm().is_finite(),
        "the controller's frequency estimate went non-finite"
    );
}
