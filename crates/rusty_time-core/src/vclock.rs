//! The virtual clock: a disciplined *view* of time for hosts where the OS clock
//! cannot be adjusted (wasm, unprivileged processes) — and the holdover model
//! everywhere else.
//!
//! It never steps its output backwards: corrections that would reverse reported
//! time are absorbed by freezing until real time catches up.

#[derive(Clone, Copy, Debug)]
pub struct VirtualClock {
    /// Seconds to add to the raw wall clock.
    offset: f64,
    /// Raw clock's frequency error, ppm (positive = raw clock runs fast).
    skew_ppm: f64,
    /// Monotonic time of the last update.
    last_update_mono: Option<f64>,
    /// Error bound at the last update, seconds.
    err_at_update: f64,
    /// How fast the error bound grows with no updates, ppm.
    err_growth_ppm: f64,
    /// Highest corrected time handed out (monotonicity guard).
    high_water: f64,
}

impl VirtualClock {
    pub fn new() -> Self {
        VirtualClock {
            offset: 0.0,
            skew_ppm: 0.0,
            last_update_mono: None,
            err_at_update: f64::INFINITY,
            err_growth_ppm: 15.0, // an undisciplined crystal's typical wander budget
            high_water: f64::NEG_INFINITY,
        }
    }

    /// Feed a fresh measurement.
    ///
    /// * `mono` — monotonic seconds now.
    /// * `offset` — seconds to ADD to the raw wall clock, measured now.
    /// * `skew_ppm` — raw clock frequency error if known.
    /// * `error_bound` — measurement error, seconds.
    pub fn update(&mut self, mono: f64, offset: f64, skew_ppm: Option<f64>, error_bound: f64) {
        self.offset = offset;
        if let Some(s) = skew_ppm {
            self.skew_ppm = s;
        }
        self.last_update_mono = Some(mono);
        self.err_at_update = error_bound.max(0.0);
    }

    /// Corrected wall time. `raw_wall` is the platform clock reading at the same
    /// instant `mono` was read.
    pub fn now(&mut self, mono: f64, raw_wall: f64) -> f64 {
        let corrected = match self.last_update_mono {
            Some(t0) => {
                let dt = (mono - t0).max(0.0);
                // The raw clock has been drifting since the measurement.
                raw_wall + self.offset - self.skew_ppm * 1e-6 * dt
            }
            None => raw_wall,
        };
        // Monotonicity guard: never hand out a time earlier than we already did.
        if corrected < self.high_water {
            self.high_water
        } else {
            self.high_water = corrected;
            corrected
        }
    }

    /// Current error bound, seconds — grows while no updates arrive. `INFINITY`
    /// until the first update: callers gate decisions on this, so it must not
    /// pretend precision it does not have.
    pub fn confidence(&self, mono: f64) -> f64 {
        match self.last_update_mono {
            Some(t0) => {
                let dt = (mono - t0).max(0.0);
                self.err_at_update + self.err_growth_ppm * 1e-6 * dt
            }
            None => f64::INFINITY,
        }
    }

    pub fn is_synchronized(&self) -> bool {
        self.last_update_mono.is_some()
    }
}

impl Default for VirtualClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsynchronized_reports_infinite_error() {
        let vc = VirtualClock::new();
        assert!(vc.confidence(100.0).is_infinite());
    }

    #[test]
    fn correction_and_skew_are_applied() {
        let mut vc = VirtualClock::new();
        // Raw clock 2 ms behind, running 10 ppm fast.
        vc.update(1000.0, 0.002, Some(10.0), 1e-4);
        let t = vc.now(1000.0, 5000.0);
        assert!((t - 5000.002).abs() < 1e-9);
        // 100 s later the raw clock gained another 1 ms; the model removes it.
        let t = vc.now(1100.0, 5100.0);
        assert!((t - 5100.001).abs() < 1e-9, "{t}");
    }

    #[test]
    fn output_never_steps_backwards() {
        let mut vc = VirtualClock::new();
        vc.update(0.0, 0.0, None, 1e-4);
        let t1 = vc.now(10.0, 100.0);
        // A later measurement says we were 50 ms fast: raw correction would
        // report an earlier time.
        vc.update(10.0, -0.050, None, 1e-4);
        let t2 = vc.now(10.001, 100.001);
        assert!(t2 >= t1, "stepped backwards: {t1} -> {t2}");
    }

    #[test]
    fn error_bound_grows_over_time() {
        let mut vc = VirtualClock::new();
        vc.update(0.0, 0.0, None, 1e-4);
        let e0 = vc.confidence(0.0);
        let e1 = vc.confidence(3600.0);
        assert!(e1 > e0);
        // 15 ppm for an hour ≈ 54 ms.
        assert!((e1 - (1e-4 + 0.054)).abs() < 1e-3, "{e1}");
    }
}
