//! The clock-discipline loop: turn estimates into clock commands.
//!
//! The platform driver (or the simulator) executes [`ClockCommand`]s; this module
//! only decides. Frequency corrections come straight from the regression slope —
//! the estimator measures frequency directly, so no PLL time constant is needed
//! (this is the chrony approach, and the reason for its fast convergence).

/// Configuration mirroring the chrony.conf directives we honor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisciplineConfig {
    /// Step (rather than slew) when |offset| exceeds this, during the first
    /// `makestep_limit` updates. `None` = never step.
    pub makestep_threshold: Option<f64>,
    pub makestep_limit: u32,
    /// Cap on offset-correction slew rate, ppm.
    pub max_slew_ppm: f64,
    /// Cap on the absolute frequency correction we will command, ppm.
    pub max_freq_ppm: f64,
    /// log2 seconds.
    pub min_poll: i8,
    pub max_poll: i8,
    /// Send an initial burst of quick polls to converge fast (chrony `iburst`).
    pub iburst: bool,
}

impl Default for DisciplineConfig {
    fn default() -> Self {
        DisciplineConfig {
            makestep_threshold: Some(1.0),
            makestep_limit: 3,
            max_slew_ppm: 83_333.0,
            max_freq_ppm: 500.0,
            min_poll: 6,
            max_poll: 10,
            iburst: true,
        }
    }
}

/// What the platform driver should do right now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockCommand {
    /// Add this many seconds to the clock immediately.
    Step { add_seconds: f64 },
    /// Run at `freq_ppm` (absolute correction vs the undisciplined clock) and
    /// additionally drain `drain_offset` seconds at up to `drain_rate_ppm`.
    Slew {
        freq_ppm: f64,
        drain_offset: f64,
        drain_rate_ppm: f64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    pub command: ClockCommand,
    /// Seconds until the next poll.
    pub next_poll_s: f64,
    /// The sample register is invalid after a step; caller must shift or clear it.
    pub reset_register: bool,
}

/// Number of quick polls in an iburst, and their spacing.
const IBURST_COUNT: u32 = 4;
const IBURST_SPACING_S: f64 = 2.0;
/// Drain a measured offset over roughly this many poll intervals.
const CORR_TIME_RATIO: f64 = 3.0;

#[derive(Clone, Debug)]
pub struct Discipline {
    cfg: DisciplineConfig,
    freq_ppm: f64,
    updates: u32,
    poll: i8,
    stable_streak: u32,
    iburst_left: u32,
}

impl Discipline {
    pub fn new(cfg: DisciplineConfig) -> Self {
        let iburst_left = if cfg.iburst { IBURST_COUNT } else { 0 };
        Discipline {
            cfg,
            freq_ppm: 0.0,
            updates: 0,
            poll: cfg.min_poll,
            stable_streak: 0,
            iburst_left,
        }
    }

    /// Current commanded frequency correction, ppm.
    pub fn freq_ppm(&self) -> f64 {
        self.freq_ppm
    }

    pub fn poll_log2(&self) -> i8 {
        self.poll
    }

    /// Feed the latest combined estimate.
    ///
    /// * `offset` — seconds to add to the local clock, now.
    /// * `freq_ppm_meas` — residual frequency error from the regression (ppm,
    ///   positive = local slow), if trusted.
    /// * `offset_sd` — residual noise of the estimate.
    pub fn on_estimate(&mut self, offset: f64, freq_ppm_meas: Option<f64>, offset_sd: f64) -> Plan {
        self.updates += 1;

        // Step epoch: large offsets early on are stepped away, chrony `makestep`.
        if let Some(threshold) = self.cfg.makestep_threshold
            && offset.abs() > threshold
            && self.updates <= self.cfg.makestep_limit
        {
            self.stable_streak = 0;
            return Plan {
                command: ClockCommand::Step {
                    add_seconds: offset,
                },
                next_poll_s: self.take_poll_interval(),
                reset_register: true,
            };
        }

        // Frequency: the regression slope is a direct measurement of the residual
        // frequency error of the *disciplined* clock, so accumulate it fully.
        if let Some(fm) = freq_ppm_meas {
            self.freq_ppm =
                (self.freq_ppm + fm).clamp(-self.cfg.max_freq_ppm, self.cfg.max_freq_ppm);
        }

        // Poll adaptation first: lengthen when quiet, shorten when the offset is
        // loud relative to the noise floor. Runs before the drain computation so
        // the drain rate is sized for the interval the plan will actually use.
        let noise = offset_sd.max(1e-7);
        if offset.abs() < 2.0 * noise {
            self.stable_streak += 1;
            if self.stable_streak >= 3 && self.poll < self.cfg.max_poll {
                self.poll += 1;
                self.stable_streak = 0;
            }
        } else {
            self.stable_streak = 0;
            if offset.abs() > 10.0 * noise && self.poll > self.cfg.min_poll {
                self.poll -= 1;
            }
        }

        // Offset: drain over ~CORR_TIME_RATIO poll intervals, capped by maxslewrate.
        let poll_s = self.peek_poll_interval();
        let wanted_rate_ppm = (offset.abs() / (CORR_TIME_RATIO * poll_s)) * 1e6;
        let drain_rate_ppm = wanted_rate_ppm.min(self.cfg.max_slew_ppm);

        Plan {
            command: ClockCommand::Slew {
                freq_ppm: self.freq_ppm,
                drain_offset: offset,
                drain_rate_ppm,
            },
            next_poll_s: self.take_poll_interval(),
            reset_register: false,
        }
    }

    /// The interval the *next* plan will use, without consuming iburst budget.
    fn peek_poll_interval(&self) -> f64 {
        if self.iburst_left > 0 {
            IBURST_SPACING_S
        } else {
            2f64.powi(self.poll as i32)
        }
    }

    /// Consume one poll slot — called exactly once per emitted Plan.
    fn take_poll_interval(&mut self) -> f64 {
        if self.iburst_left > 0 {
            self.iburst_left -= 1;
            IBURST_SPACING_S
        } else {
            2f64.powi(self.poll as i32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_initial_offset_is_stepped() {
        let mut d = Discipline::new(DisciplineConfig::default());
        let plan = d.on_estimate(120.0, None, 1e-4);
        assert!(matches!(
            plan.command,
            ClockCommand::Step { add_seconds } if (add_seconds - 120.0).abs() < 1e-9
        ));
        assert!(plan.reset_register);
    }

    #[test]
    fn step_window_closes() {
        let mut d = Discipline::new(DisciplineConfig::default());
        for _ in 0..3 {
            let _ = d.on_estimate(0.0001, None, 1e-4);
        }
        // Fourth update: even a huge offset must slew, not step.
        let plan = d.on_estimate(5.0, None, 1e-4);
        assert!(matches!(plan.command, ClockCommand::Slew { .. }));
    }

    #[test]
    fn freq_accumulates_and_clamps() {
        let mut d = Discipline::new(DisciplineConfig::default());
        let _ = d.on_estimate(1e-4, Some(100.0), 1e-4);
        assert!((d.freq_ppm() - 100.0).abs() < 1e-9);
        let _ = d.on_estimate(1e-4, Some(1000.0), 1e-4);
        assert!((d.freq_ppm() - 500.0).abs() < 1e-9, "clamped at max_freq");
    }

    #[test]
    fn iburst_then_normal_cadence() {
        let mut d = Discipline::new(DisciplineConfig::default());
        let mut intervals = Vec::new();
        for _ in 0..6 {
            let plan = d.on_estimate(1e-5, None, 1e-4);
            intervals.push(plan.next_poll_s);
        }
        assert!(intervals[..4].iter().all(|&i| i == 2.0), "{intervals:?}");
        assert!(intervals[4] >= 64.0, "{intervals:?}");
    }

    #[test]
    fn closed_loop_converges() {
        // A toy plant: local clock 40 ppm fast, 30 ms ahead. The discipline reads
        // perfect estimates each poll; assert the loop pulls both to ~zero.
        let mut d = Discipline::new(DisciplineConfig {
            iburst: false,
            makestep_threshold: None,
            ..DisciplineConfig::default()
        });
        let mut clock_err_s = 0.030_f64; // local - true
        let base_freq_ppm = 40.0;
        let mut t = 0.0;
        for _ in 0..60 {
            // The measured offset is what we should ADD: -(clock_err).
            let offset = -clock_err_s;
            // Perfect freq measurement: the regression slope is dθ/dt, and
            // θ = -err, so the slope is -(base + applied).
            let slope_ppm = -(base_freq_ppm + d.freq_ppm());
            let plan = d.on_estimate(offset, Some(slope_ppm), 1e-5);
            let dt = plan.next_poll_s;
            if let ClockCommand::Slew {
                freq_ppm,
                drain_offset,
                drain_rate_ppm,
            } = plan.command
            {
                // Plant integration over dt: positive applied freq speeds the
                // local clock (raises err); the drain adds θ toward zero err.
                let drift = (base_freq_ppm + freq_ppm) * 1e-6 * dt;
                let max_drain = drain_rate_ppm * 1e-6 * dt;
                let drain = drain_offset.abs().min(max_drain) * drain_offset.signum();
                clock_err_s += drift + drain;
            }
            t += dt;
        }
        assert!(
            clock_err_s.abs() < 1e-4,
            "did not converge: err {clock_err_s} at t {t}"
        );
        assert!(
            (d.freq_ppm() + 40.0).abs() < 2.0,
            "freq not learned (want ~-40): {}",
            d.freq_ppm()
        );
    }
}
