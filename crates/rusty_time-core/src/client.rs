//! The client-side synchronisation controller.
//!
//! This is the thing that turns exchanges into clock commands: sample register,
//! discipline loop, and the register bookkeeping that keeps the two consistent
//! when the clock is adjusted underneath them.
//!
//! **It lives here so the daemon and the simulator run the same code.** They
//! did not, once: the bookkeeping below was written inside the TIMECORP
//! simulator, and the daemon had no client loop at all. A performance
//! comparison against another implementation is only worth anything if the
//! thing measured is the thing that ships, so the logic was moved here and
//! both call it. The simulator's recorded S1/S6/S8 numbers are the regression
//! test on that move — they must not change.

use crate::discipline::{ChangeVerdict, ClockCommand, Discipline, DisciplineConfig, Plan};
use crate::filter::{Sample, SampleRegister};

/// How many samples one source keeps.
pub const REGISTER_CAPACITY: usize = 64;

/// Drives one source's samples into clock commands.
pub struct SyncController {
    register: SampleRegister,
    discipline: Discipline,
    /// The *permanent* frequency correction currently commanded.
    freq_cmd_ppm: f64,
    /// The *temporary* offset-drain rate currently running.
    ///
    /// Tracked apart from the frequency because the two transform stored
    /// samples differently: a frequency change is permanent and tilts the
    /// whole history, while a drain consumes offset and must be subtracted as
    /// one. Folding the drain into the frequency term lets the regression
    /// slope absorb it, and the loop settles into a constant-offset limit
    /// cycle — TIMECORP S1 sat pinned at 2 ms until these were separated.
    drain_ppm: f64,
    /// Seconds of correction the running drain still owes.
    ///
    /// This is the field that makes a drain a **budget** rather than a
    /// frequency. `ClockCommand::Slew` has always carried `drain_offset`, and
    /// no driver honoured it: every platform folded the drain into a constant
    /// frequency that ran until the next plan. So a correction could only ever
    /// be sized to the poll interval — a faster rate would not stop when the
    /// offset was gone, it would sail past it. That is why a 500 ms cold start
    /// spent 16 s on its last 9.8 ms: the remainder was handed the poll's
    /// deadline instead of its own.
    ///
    /// With the budget tracked, a drain ends when it is spent. The rate can
    /// then be chosen for how fast it is safe to move the clock, which is what
    /// chrony does, and what its `drain_offset` field promised all along.
    drain_remaining_s: f64,
    /// Monotonic time of the last plan, for working out how much drain ran.
    last_plan_mono_s: Option<f64>,
    /// What the last plan changed, so it can be undone if the driver refused it.
    unapplied: Option<Unapplied>,
}

/// The bookkeeping one plan performed, kept only until the caller says whether
/// the clock actually accepted it.
#[derive(Clone, Copy, Debug)]
struct Unapplied {
    mono_s: f64,
    dfreq_ppm: f64,
    step_s: f64,
    freq_cmd_before: f64,
    drain_ppm_before: f64,
    drain_remaining_before: f64,
}

/// What the controller decided, plus what the caller must do about it.
pub struct ControllerStep {
    pub plan: Plan,
    /// Total frequency the driver should command: the permanent correction
    /// plus the drain currently running.
    pub applied_ppm: f64,
    /// The estimate that produced the plan, for reporting.
    pub estimate_offset_s: f64,
    pub estimate_freq_ppm: Option<f64>,
    /// Dispersion of the estimate itself, seconds.
    ///
    /// Selection compares intervals of `offset ± root_distance`, and a root
    /// distance built only from path delay describes how well the NETWORK is
    /// known while saying nothing about how well this source's own offset is
    /// known. During acquisition those differ by orders of magnitude: the path
    /// is a hundred microseconds and the estimate is milliseconds.
    ///
    /// Omitting it makes every interval far too narrow to overlap, so a set of
    /// perfectly healthy servers forms no majority and selection returns
    /// nothing at all — measured on the three-server rig, 74 polls out of 89.
    pub estimate_sd_s: f64,
    pub samples_used: usize,
    /// What the maximum-change guard made of this correction. Mirrored out of
    /// the plan so a caller can act on it without matching on the command.
    pub verdict: ChangeVerdict,
}

impl SyncController {
    pub fn new(config: DisciplineConfig) -> Self {
        SyncController {
            register: {
                let mut r = SampleRegister::new(REGISTER_CAPACITY);
                r.set_weight_floor_ratio(config.weight_floor_ratio);
                r.set_offset_weight_floor_ratio(config.offset_weight_floor_ratio);
                r.set_offset_age_halflife_s(config.offset_age_halflife_s);
                r.set_offset_weight_dispersion_k(config.offset_weight_dispersion_k);
                r.set_slope_density_weighting(config.slope_density_weighting);
                r.set_adaptive_window(config.adaptive_window);
                r
            },
            discipline: Discipline::new(config),
            freq_cmd_ppm: 0.0,
            drain_ppm: 0.0,
            drain_remaining_s: 0.0,
            last_plan_mono_s: None,
            unapplied: None,
        }
    }

    /// Undo the bookkeeping of the last plan, because the driver refused it.
    ///
    /// **The loop's arithmetic has to describe what the clock actually did.**
    /// A plan books its own effects the moment it is produced: the frequency
    /// change tilts every stored sample, a step shifts them, and the drain
    /// budget starts counting down. The caller then hands the command to the
    /// platform — which can refuse it. `clock_adjtime` returns `EPERM` the
    /// moment `CAP_SYS_TIME` goes away, and a seccomp policy or a container
    /// with a read-only clock refuses it too.
    ///
    /// Without this, a refusal is silent and cumulative. The register carries
    /// corrections that never happened, the regression reads that history as
    /// truth, and the daemon reports itself synchronised while the clock free
    /// runs — the worst failure a time daemon has, because nothing looks wrong.
    ///
    /// Returns whether there was a plan to revert.
    pub fn revert_last_plan(&mut self) -> bool {
        let Some(u) = self.unapplied.take() else {
            return false;
        };
        // Exact inverse of what `on_sample` applied, in the opposite order.
        self.register
            .slew_samples(u.mono_s, -u.dfreq_ppm, -u.step_s);
        self.freq_cmd_ppm = u.freq_cmd_before;
        self.drain_ppm = u.drain_ppm_before;
        self.drain_remaining_s = u.drain_remaining_before;
        true
    }

    /// Confirm the last plan reached the clock, so it can no longer be undone.
    pub fn confirm_last_plan(&mut self) {
        self.unapplied = None;
    }

    pub fn freq_ppm(&self) -> f64 {
        self.freq_cmd_ppm
    }

    pub fn drain_ppm(&self) -> f64 {
        self.drain_ppm
    }

    pub fn applied_ppm(&self) -> f64 {
        self.freq_cmd_ppm + self.drain_ppm
    }

    pub fn poll_log2(&self) -> i8 {
        self.discipline.poll_log2()
    }

    pub fn samples(&self) -> usize {
        self.register.len()
    }

    /// Seed the frequency estimate from persisted drift, so a restart does not
    /// re-learn what was already known.
    pub fn preload_frequency(&mut self, freq_ppm: f64) {
        self.freq_cmd_ppm = freq_ppm;
    }

    /// The interval to use when an exchange is lost — no plan is produced, but
    /// the caller still needs to know when to try again.
    pub fn retry_interval_s(&self) -> f64 {
        self.discipline.retry_interval_s()
    }

    /// When the running drain will have spent its budget, if one is running.
    pub fn drain_completes_at(&self) -> Option<f64> {
        let last = self.last_plan_mono_s?;
        if self.drain_ppm == 0.0 || self.drain_remaining_s <= 0.0 {
            return None;
        }
        Some(last + self.drain_remaining_s / (self.drain_ppm.abs() * 1e-6))
    }

    /// End the drain if its budget is spent, returning the command that leaves
    /// the clock running at the frequency term alone.
    ///
    /// Callers must invoke this as they advance time — the daemon by waking for
    /// it, the simulator at each substep — or the drain runs on past its budget
    /// and overshoots, which is the behaviour this exists to end.
    pub fn poll_drain(&mut self, mono_now_s: f64) -> Option<ClockCommand> {
        let completes_at = self.drain_completes_at()?;
        if mono_now_s < completes_at {
            return None;
        }
        let last = self.last_plan_mono_s?;

        // Book what the clock ACTUALLY received, not the budget.
        //
        // The budget says when the drain *should* stop; the driver stops when
        // it is told to, which is when the caller next looks. A caller that
        // wakes late has already had the extra correction applied, and booking
        // only the budget silently loses the difference — the register keeps a
        // correction the clock really got but the loop never recorded, and the
        // regression reads it as drift.
        //
        // It is not a rounding error. Measured on S6, waking 11 ms after a
        // 19577 ppm drain expired delivered 215 us that went unbooked, and
        // that single unbooked correction left a permanent ~180 us offset:
        // 137 us steady against chrony's 2.5 us, from one late wake-up in a
        // fifteen-minute run. This is the same failure as a driver silently
        // clamping a slew — the loop's arithmetic must describe what the clock
        // did, not what it was asked to do.
        let consumed = self.drain_ppm * 1e-6 * (mono_now_s - last).max(0.0);
        if consumed != 0.0 {
            self.register.slew_samples(mono_now_s, 0.0, consumed);
        }
        self.drain_ppm = 0.0;
        self.drain_remaining_s = 0.0;
        self.last_plan_mono_s = Some(mono_now_s);
        Some(ClockCommand::Slew {
            freq_ppm: self.freq_cmd_ppm,
            drain_offset: 0.0,
            drain_rate_ppm: 0.0,
        })
    }

    /// Feed one completed exchange and get the resulting plan.
    ///
    /// `mono_now_s` is the local monotonic clock; `sample.t` should be the
    /// exchange midpoint on that same timescale.
    pub fn on_sample(&mut self, mono_now_s: f64, sample: Sample) -> ControllerStep {
        self.on_sample_with_leap(mono_now_s, sample, false)
    }

    /// As [`SyncController::on_sample`], told whether the source has announced
    /// a leap second for the current UTC day.
    pub fn on_sample_with_leap(
        &mut self,
        mono_now_s: f64,
        sample: Sample,
        leap_pending: bool,
    ) -> ControllerStep {
        // Account for the drain that actually ran since the last plan. It is a
        // *consumed offset correction*, so it leaves the stored history as an
        // offset and the regression slope keeps measuring frequency alone.
        // What the drain actually delivered since the last plan.
        //
        // Deliberately NOT capped by the remaining budget. If the drain ran out
        // it was already retired by `poll_drain`, which booked it exactly and
        // zeroed the rate, so this reads zero. If it did not run out, it was
        // slewing for the whole interval and delivered every bit of it.
        // Capping here books less correction than the clock actually received,
        // and the regression reads the difference as drift: it settled the
        // frequency estimate about 1 ppm off true, which at a 32 s poll is a
        // permanent ~200 us offset. S6 measured 136 us against chrony's 2.5 us
        // until this cap came out.
        let drained = match self.last_plan_mono_s {
            Some(last) if self.drain_ppm != 0.0 => {
                self.drain_ppm * 1e-6 * (mono_now_s - last).max(0.0)
            }
            _ => 0.0,
        };
        if drained != 0.0 {
            self.register.slew_samples(mono_now_s, 0.0, drained);
            self.drain_remaining_s = (self.drain_remaining_s - drained.abs()).max(0.0);
        }

        self.register.push(sample);

        // Regression once it has enough spread; before that the lowest-delay
        // single sample, which is the least contaminated reading available.
        let (offset, freq, sd) = match self.register.regress(mono_now_s) {
            Some(estimate) => (
                estimate.offset,
                estimate.freq_ppm,
                estimate.offset_sd.max(1e-7),
            ),
            None => match self.register.best() {
                Some(best) => (best.offset, None, (best.delay / 2.0).max(1e-7)),
                None => (0.0, None, 1e-3),
            },
        };

        let plan = self
            .discipline
            .on_estimate_with_leap(offset, freq, sd, leap_pending);
        let freq_cmd_new = self.discipline.freq_ppm();
        // Captured before the books move, so `revert_last_plan` can put them
        // back exactly if the clock command is refused.
        let freq_cmd_before = self.freq_cmd_ppm;
        let drain_ppm_before = self.drain_ppm;
        let drain_remaining_before = self.drain_remaining_s;

        match plan.command {
            ClockCommand::Step { add_seconds } => {
                // A step moves the clock at once; history is re-expressed in
                // the new clock's terms rather than discarded.
                self.register.slew_samples(
                    mono_now_s,
                    freq_cmd_new - self.freq_cmd_ppm,
                    add_seconds,
                );
                self.drain_ppm = 0.0;
                self.drain_remaining_s = 0.0;
            }
            ClockCommand::Slew {
                drain_offset,
                drain_rate_ppm,
                ..
            } => {
                // The permanent frequency change tilts history now; the new
                // drain is accounted when it has actually run, at the top of
                // the next call.
                self.register
                    .slew_samples(mono_now_s, freq_cmd_new - self.freq_cmd_ppm, 0.0);
                self.drain_ppm = drain_rate_ppm.copysign(drain_offset);
                // The budget: this drain stops once it has moved the clock by
                // this much, whatever the poll interval says.
                self.drain_remaining_s = drain_offset.abs();
            }
        }

        self.freq_cmd_ppm = freq_cmd_new;
        self.last_plan_mono_s = Some(mono_now_s);
        // Remember enough to undo all of the above if the driver refuses it.
        self.unapplied = Some(Unapplied {
            mono_s: mono_now_s,
            dfreq_ppm: freq_cmd_new - freq_cmd_before,
            step_s: match plan.command {
                ClockCommand::Step { add_seconds } => add_seconds,
                ClockCommand::Slew { .. } => 0.0,
            },
            freq_cmd_before,
            drain_ppm_before,
            drain_remaining_before,
        });

        ControllerStep {
            plan,
            applied_ppm: self.freq_cmd_ppm + self.drain_ppm,
            estimate_offset_s: offset,
            estimate_freq_ppm: freq,
            samples_used: self.register.len(),
            estimate_sd_s: sd,
            verdict: plan.verdict,
        }
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::*;
    use crate::filter::Sample;

    fn feed(c: &mut SyncController, n: usize, base: f64) {
        for i in 0..n {
            let t = 16.0 * (i as f64 + 1.0);
            c.on_sample(
                t,
                Sample {
                    t,
                    offset: base - 20e-6 * t,
                    delay: 200e-6,
                    dispersion: 1e-6,
                },
            );
        }
    }

    /// A refused clock command must leave the controller exactly as it was.
    ///
    /// The regression that matters: without this, a daemon that has lost
    /// CAP_SYS_TIME keeps planning corrections, keeps booking them against its
    /// own history, and keeps reporting itself synchronised, while the clock it
    /// believes it is steering runs free.
    #[test]
    fn a_refused_command_leaves_no_trace() {
        let cfg = DisciplineConfig::default();
        let mut applied = SyncController::new(cfg);
        let mut refused = SyncController::new(cfg);

        feed(&mut applied, 12, 0.010);
        feed(&mut refused, 12, 0.010);

        // One more sample on each. The first controller's command reaches the
        // clock; the second's is refused and reverted.
        let t = 16.0 * 13.0;
        let sample = Sample {
            t,
            offset: 0.010 - 20e-6 * t,
            delay: 200e-6,
            dispersion: 1e-6,
        };
        let before_freq = refused.freq_ppm();
        let before_drain = refused.drain_ppm();

        applied.on_sample(t, sample);
        applied.confirm_last_plan();

        refused.on_sample(t, sample);
        assert!(refused.revert_last_plan(), "there was a plan to revert");

        assert_eq!(
            refused.freq_ppm(),
            before_freq,
            "the frequency command survived a refusal"
        );
        assert_eq!(
            refused.drain_ppm(),
            before_drain,
            "the drain survived a refusal"
        );

        // And the stored history must be back where it was: feeding both the
        // same next sample, the one that reverted must NOT agree with the one
        // that applied, because their clocks genuinely differ now.
        let t2 = 16.0 * 14.0;
        let next = Sample {
            t: t2,
            offset: 0.010 - 20e-6 * t2,
            delay: 200e-6,
            dispersion: 1e-6,
        };
        let a = applied.on_sample(t2, next);
        let r = refused.on_sample(t2, next);
        assert_ne!(
            a.applied_ppm, r.applied_ppm,
            "a reverted controller behaved identically to one that applied its              command, so the revert did not actually restore the books"
        );
    }

    /// Reverting twice, or with nothing outstanding, must be harmless.
    #[test]
    fn reverting_nothing_is_a_no_op() {
        let mut c = SyncController::new(DisciplineConfig::default());
        assert!(!c.revert_last_plan(), "nothing has been planned yet");
        feed(&mut c, 6, 0.001);
        let freq = c.freq_ppm();
        assert!(c.revert_last_plan());
        assert!(!c.revert_last_plan(), "a second revert must do nothing");
        assert_ne!(freq, f64::NAN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A toy plant, so the controller is exercised end to end without a
    /// network: the local clock drifts at `base_freq_ppm` and starts `err` off.
    fn closed_loop(base_freq_ppm: f64, initial_err_s: f64, polls: usize) -> f64 {
        let mut controller = SyncController::new(DisciplineConfig {
            iburst: false,
            makestep_threshold: None,
            ..DisciplineConfig::default()
        });
        let mut err = initial_err_s;
        let mut t = 0.0f64;

        for _ in 0..polls {
            let mono = t + err;
            // A perfect exchange: the measured offset is exactly -err.
            let step = controller.on_sample(
                mono,
                Sample {
                    t: mono,
                    offset: -err,
                    delay: 0.0002,
                    dispersion: 0.0,
                },
            );
            let dt = step.plan.next_poll_s;
            // Integrate the plant: true drift plus whatever we commanded.
            err += (base_freq_ppm + step.applied_ppm) * 1e-6 * dt;
            t += dt;
        }
        err
    }

    #[test]
    fn the_controller_converges_on_a_drifting_clock() {
        let residual = closed_loop(40.0, 0.030, 80);
        assert!(
            residual.abs() < 1e-4,
            "did not converge: {residual} s remaining"
        );
    }

    #[test]
    fn it_converges_from_either_direction() {
        for (drift, start) in [
            (40.0, 0.030),
            (-40.0, -0.030),
            (100.0, -0.050),
            (-15.0, 0.010),
        ] {
            let residual = closed_loop(drift, start, 120);
            assert!(
                residual.abs() < 1e-3,
                "drift {drift} ppm from {start} s left {residual} s"
            );
        }
    }

    #[test]
    fn frequency_and_drain_stay_separate() {
        // The distinction this type exists to preserve: after a plan, the
        // permanent frequency and the temporary drain must be individually
        // recoverable, not merged.
        let mut controller = SyncController::new(DisciplineConfig {
            iburst: false,
            makestep_threshold: None,
            ..DisciplineConfig::default()
        });
        let step = controller.on_sample(
            0.0,
            Sample {
                t: 0.0,
                offset: 0.001,
                delay: 0.0002,
                dispersion: 0.0,
            },
        );
        assert!(
            controller.drain_ppm() != 0.0,
            "a non-zero offset should start a drain"
        );
        assert_eq!(
            step.applied_ppm,
            controller.freq_ppm() + controller.drain_ppm(),
            "the applied total must be exactly the two parts"
        );
    }

    #[test]
    fn a_failed_exchange_retries_at_the_burst_spacing_not_the_poll_interval() {
        // A server that has just started answers unsynchronised, and the client
        // must refuse those. If the refusal costs a full poll interval, a cold
        // start waits 16 s for its first usable sample — which is exactly what
        // the first benchmark against chrony caught.
        let controller = SyncController::new(DisciplineConfig {
            min_poll: 4, // 16 s
            iburst: true,
            ..DisciplineConfig::default()
        });
        let retry = controller.retry_interval_s();
        assert!(
            retry <= 4.0,
            "a cold-start retry waited {retry} s; the burst spacing is the point"
        );
    }

    #[test]
    fn once_the_burst_is_spent_retries_use_the_poll_interval() {
        // The fast retry is for acquisition only — a synchronised client that
        // loses a packet must not hammer the server.
        let mut controller = SyncController::new(DisciplineConfig {
            min_poll: 4,
            iburst: true,
            makestep_threshold: None,
            ..DisciplineConfig::default()
        });
        for i in 0..8 {
            controller.on_sample(
                i as f64 * 2.0,
                Sample {
                    t: i as f64 * 2.0,
                    offset: 1e-6,
                    delay: 0.0002,
                    dispersion: 0.0,
                },
            );
        }
        assert!(
            controller.retry_interval_s() >= 16.0,
            "after the burst, retries must back off to the poll interval"
        );
    }

    #[test]
    fn a_drain_ends_when_its_budget_is_spent() {
        // `ClockCommand::Slew` has always carried `drain_offset` -- the size of
        // the correction. Until drains were budgeted nothing honoured it: the
        // drain was a frequency that ran until the next plan, so its rate could
        // only ever be "the offset divided by the poll interval".
        let mut controller = SyncController::new(DisciplineConfig {
            iburst: false,
            makestep_threshold: None,
            ..DisciplineConfig::default()
        });
        controller.on_sample(
            0.0,
            Sample {
                t: 0.0,
                offset: 0.010,
                delay: 0.0002,
                dispersion: 0.0,
            },
        );
        let ends = controller
            .drain_completes_at()
            .expect("a drain should be running");
        assert!(ends > 0.0, "drain has no completion time");
        // Nothing before then...
        assert!(controller.poll_drain(ends - 1e-6).is_none());
        assert!(controller.applied_ppm() != 0.0);
        // ...and it retires exactly once at the end.
        assert!(controller.poll_drain(ends).is_some());
        assert!(controller.poll_drain(ends + 1.0).is_none());
        assert_eq!(
            controller.drain_ppm(),
            0.0,
            "a spent drain must stop slewing the clock"
        );
    }

    #[test]
    fn a_late_retirement_books_what_the_clock_actually_received() {
        // The budget says when the drain *should* stop; the driver stops when
        // it is told to. A caller that wakes late has already had the extra
        // correction applied, and booking only the budget loses the difference
        // -- the regression then reads it as drift. Measured on S6, one late
        // wake-up left a permanent ~180 us offset: 136 us steady against
        // chrony's 2.5 us.
        let mut controller = SyncController::new(DisciplineConfig {
            iburst: false,
            makestep_threshold: None,
            ..DisciplineConfig::default()
        });
        controller.on_sample(
            0.0,
            Sample {
                t: 0.0,
                offset: 0.010,
                delay: 0.0002,
                dispersion: 0.0,
            },
        );
        let rate = controller.drain_ppm();
        let ends = controller.drain_completes_at().expect("a drain");
        let late = 0.5;

        // Retire it half a second late, then feed a sample reporting the clock
        // as correct. If the extra correction were not booked, the loop would
        // believe an offset it had already removed.
        controller.poll_drain(ends + late);
        let step = controller.on_sample(
            ends + late,
            Sample {
                t: ends + late,
                offset: 0.0,
                delay: 0.0002,
                dispersion: 0.0,
            },
        );
        let overrun = rate.abs() * 1e-6 * late;
        assert!(
            overrun > 1e-6,
            "test is vacuous unless the overrun is meaningful"
        );
        assert!(
            step.estimate_offset_s.abs() < 0.010,
            "late retirement lost correction the clock had already received:              estimate {} s",
            step.estimate_offset_s
        );
    }

    #[test]
    fn a_preloaded_frequency_is_the_starting_point() {
        let mut controller = SyncController::new(DisciplineConfig::default());
        controller.preload_frequency(-12.5);
        assert!((controller.freq_ppm() + 12.5).abs() < 1e-12);
    }
}
