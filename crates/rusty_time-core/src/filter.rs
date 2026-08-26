//! Per-source sample history and regression estimation.
//!
//! chrony's central insight (implemented here from its published description, not
//! its GPL source): instead of ntpd's clock filter that keeps one sample in eight,
//! keep a register of recent (time, offset, delay) samples and fit a weighted
//! linear regression through them. The slope is a direct frequency-error
//! measurement; the extrapolated intercept is the current offset. That is what buys
//! chrony-class convergence, and it is the estimator the corpus grades.

/// One completed exchange with a source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Local monotonic time of the exchange midpoint, seconds.
    pub t: f64,
    /// Seconds to ADD to the local clock (RFC 5905 θ).
    pub offset: f64,
    /// Round-trip delay, seconds.
    pub delay: f64,
    /// Accumulated dispersion, seconds.
    pub dispersion: f64,
}

/// Regression output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegressEstimate {
    /// Offset extrapolated to the query time (seconds to add).
    pub offset: f64,
    /// Residual frequency error of the local clock vs the source, ppm.
    /// Positive = local offset is growing = local clock runs slow.
    /// `None` until enough well-spread samples exist to trust the slope.
    pub freq_ppm: Option<f64>,
    /// Standard deviation of the residuals, seconds.
    pub offset_sd: f64,
    /// Samples that survived outlier trimming.
    pub n_used: usize,
    /// Time span covered by the used samples, seconds.
    pub span: f64,
}

/// Fixed-capacity register of recent samples for one source.
#[derive(Clone, Debug)]
pub struct SampleRegister {
    samples: Vec<Sample>,
    capacity: usize,
}

/// Minimum span (seconds) and count before the regression slope is reported.
const FREQ_MIN_SPAN_S: f64 = 8.0;
const FREQ_MIN_SAMPLES: usize = 4;

impl SampleRegister {
    pub fn new(capacity: usize) -> Self {
        SampleRegister {
            samples: Vec::with_capacity(capacity.max(3)),
            capacity: capacity.max(3),
        }
    }

    pub fn push(&mut self, s: Sample) {
        if self.samples.len() == self.capacity {
            self.samples.remove(0); // capacity is ≤64; shift cost is noise here
        }
        self.samples.push(s);
    }

    /// Drop history (after a clock step every stored offset is stale).
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Re-express stored samples after the clock is adjusted, so history stays a
    /// valid predictor in the adjusted clock's terms (chrony's published
    /// slew-samples behavior, derived independently here).
    ///
    /// At time `t_change` the discipline added `doffset` seconds to the clock
    /// and changed its applied frequency by `dfreq_ppm`. Every stored offset
    /// ("seconds still to add") becomes:
    ///
    /// `offset -= doffset + dfreq_ppm·1e-6·(t_i − t_change)`
    ///
    /// which transforms the fitted line exactly as the clock was transformed.
    /// Without this, a regression window spanning corrections sees kinked data:
    /// the slope is a mixture of old and new regimes, and the loop mis-learns
    /// frequency (observed as TIMECORP S1 diverging before this existed).
    pub fn slew_samples(&mut self, t_change: f64, dfreq_ppm: f64, doffset: f64) {
        for s in &mut self.samples {
            s.offset -= doffset + dfreq_ppm * 1e-6 * (s.t - t_change);
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Best single sample (minimum delay among the most recent eight): the
    /// low-noise fallback while the regression is still warming up.
    pub fn best(&self) -> Option<Sample> {
        let tail_start = self.samples.len().saturating_sub(8);
        self.samples[tail_start..]
            .iter()
            .copied()
            .min_by(|a, b| a.delay.total_cmp(&b.delay))
    }

    /// Weighted linear regression of offset against time, with one outlier-trim
    /// pass, extrapolated to `now`.
    pub fn regress(&self, now: f64) -> Option<RegressEstimate> {
        let n = self.samples.len();
        if n < 3 {
            return None;
        }
        let min_delay = self
            .samples
            .iter()
            .map(|s| s.delay)
            .fold(f64::INFINITY, f64::min);
        // Weight favors low-delay samples: queueing noise scales with excess delay.
        let floor = (min_delay * 0.125).max(1e-9);
        let weight = |s: &Sample| {
            let excess = (s.delay - min_delay).max(0.0) + floor;
            let w = (floor / excess) * (floor / excess);
            w.max(1e-6)
        };

        let mut used: Vec<&Sample> = self.samples.iter().collect();
        let mut fit = wls_fit(&used, weight)?;

        // Pass 1 — distrust HISTORY, never the present: when the residuals are
        // autocorrelated (few sign runs) AND the window's two halves disagree
        // (a real regime change, not one spike), drop the oldest quarter and
        // refit. Trimming by residual size alone can reject fresh truthful
        // samples against a stale self-consistent history and lock the loop into
        // a constant error — TIMECORP S1 found exactly that failure before this
        // pass existed.
        for _ in 0..4 {
            if used.len() < 8 {
                break;
            }
            let runs = residual_sign_runs(&used, &fit);
            if runs * 3 >= used.len() {
                break; // residuals look well mixed: one regime
            }
            let (half_gap, mad) = residual_half_gap_and_mad(&used, &fit);
            if half_gap <= 3.0 * (1.4826 * mad).max(1e-9) {
                break; // a lone spike, not a regime change — pass 2's job
            }
            let drop = (used.len() / 4).max(2);
            used.drain(..drop);
            match wls_fit(&used, weight) {
                Some(refit) => fit = refit,
                None => break,
            }
        }

        // Pass 2 — interior spike trim (a delayed packet), thresholded on the
        // robust MAD scale (an rms threshold is inflated by the spike itself and
        // masks it), protecting the newest samples so the present can never be
        // voted away.
        if used.len() > 4 {
            let (_, mad) = residual_half_gap_and_mad(&used, &fit);
            let threshold = (3.0 * 1.4826 * mad).max(1e-9);
            let protect_from = used.len() - 3;
            let kept: Vec<&Sample> = used
                .iter()
                .copied()
                .enumerate()
                .filter(|(i, s)| {
                    *i >= protect_from
                        || (s.offset - (fit.a + fit.b * (s.t - fit.t0))).abs() <= threshold
                })
                .map(|(_, s)| s)
                .collect();
            if kept.len() >= 3
                && kept.len() < used.len()
                && let Some(refit) = wls_fit(&kept, weight)
            {
                used = kept;
                fit = refit;
            }
        }

        let span = used
            .iter()
            .map(|s| s.t)
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), t| {
                (lo.min(t), hi.max(t))
            });
        let span = (span.1 - span.0).max(0.0);

        let freq_ppm = if used.len() >= FREQ_MIN_SAMPLES && span >= FREQ_MIN_SPAN_S {
            Some(fit.b * 1e6)
        } else {
            None
        };

        Some(RegressEstimate {
            offset: fit.a + fit.b * (now - fit.t0),
            freq_ppm,
            offset_sd: fit.sd,
            n_used: used.len(),
            span,
        })
    }
}

/// (|mean residual of older half − mean residual of newer half|, median absolute
/// residual). The gap detects regime changes; the MAD is a spike-robust scale.
fn residual_half_gap_and_mad(samples: &[&Sample], fit: &Fit) -> (f64, f64) {
    let resid = |s: &Sample| s.offset - (fit.a + fit.b * (s.t - fit.t0));
    let n = samples.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let half = n / 2;
    let mean = |part: &[&Sample]| -> f64 {
        if part.is_empty() {
            0.0
        } else {
            part.iter().map(|s| resid(s)).sum::<f64>() / part.len() as f64
        }
    };
    let gap = (mean(&samples[..half]) - mean(&samples[half..])).abs();
    let mut abs: Vec<f64> = samples.iter().map(|s| resid(s).abs()).collect();
    abs.sort_by(f64::total_cmp);
    (gap, abs[abs.len() / 2])
}

/// Number of runs of same-signed residuals, in time order. A well-mixed fit has
/// about n/2; a kinked window (two regimes) has very few.
fn residual_sign_runs(samples: &[&Sample], fit: &Fit) -> usize {
    let mut runs = 0usize;
    let mut last_sign = 0i8;
    for s in samples {
        let r = s.offset - (fit.a + fit.b * (s.t - fit.t0));
        let sign = if r > 0.0 { 1 } else { -1 };
        if sign != last_sign {
            runs += 1;
            last_sign = sign;
        }
    }
    runs
}

struct Fit {
    /// Offset at t0.
    a: f64,
    /// Slope, s/s.
    b: f64,
    /// Weighted residual standard deviation.
    sd: f64,
    /// Time origin (mean of used sample times).
    t0: f64,
}

fn wls_fit(samples: &[&Sample], weight: impl Fn(&Sample) -> f64) -> Option<Fit> {
    let n = samples.len();
    if n < 2 {
        return None;
    }
    let mut sw = 0.0;
    let mut swt = 0.0;
    for s in samples {
        let w = weight(s);
        sw += w;
        swt += w * s.t;
    }
    if sw <= 0.0 {
        return None;
    }
    let t0 = swt / sw;

    let mut sxx = 0.0;
    let mut sxy = 0.0;
    let mut sy = 0.0;
    for s in samples {
        let w = weight(s);
        let x = s.t - t0;
        sxx += w * x * x;
        sxy += w * x * s.offset;
        sy += w * s.offset;
    }
    let b = if sxx > 1e-12 { sxy / sxx } else { 0.0 };
    let a = sy / sw; // intercept at t0 (x is centered)

    let mut sr = 0.0;
    for s in samples {
        let w = weight(s);
        let r = s.offset - (a + b * (s.t - t0));
        sr += w * r * r;
    }
    let dof = (n as f64 - 2.0).max(1.0);
    let sd = (sr / sw * n as f64 / dof).sqrt();

    Some(Fit { a, b, sd, t0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg_with(points: &[(f64, f64, f64)]) -> SampleRegister {
        let mut r = SampleRegister::new(64);
        for &(t, offset, delay) in points {
            r.push(Sample {
                t,
                offset,
                delay,
                dispersion: 0.0,
            });
        }
        r
    }

    #[test]
    fn exact_line_is_recovered() {
        // offset = 1 ms + 10 ppm * t, no noise.
        let pts: Vec<(f64, f64, f64)> = (0..10)
            .map(|i| {
                let t = i as f64 * 16.0;
                (t, 1e-3 + 10e-6 * t, 0.0002)
            })
            .collect();
        let r = reg_with(&pts);
        let est = r.regress(144.0).expect("estimate");
        let freq = est.freq_ppm.expect("freq");
        assert!((freq - 10.0).abs() < 1e-6, "freq {freq}");
        let expect = 1e-3 + 10e-6 * 144.0;
        assert!((est.offset - expect).abs() < 1e-9, "offset {}", est.offset);
        assert!(est.offset_sd < 1e-9);
    }

    #[test]
    fn outlier_is_trimmed() {
        let mut pts: Vec<(f64, f64, f64)> = (0..12)
            .map(|i| {
                let t = i as f64 * 16.0;
                (t, 5e-4, 0.0002)
            })
            .collect();
        pts[6].1 = 0.050; // 50 ms spike (delayed packet), same delay
        let r = reg_with(&pts);
        let est = r.regress(200.0).expect("estimate");
        assert_eq!(est.n_used, 11, "outlier not trimmed");
        assert!(
            (est.offset - 5e-4).abs() < 1e-4,
            "offset {} polluted by spike",
            est.offset
        );
    }

    #[test]
    fn high_delay_samples_are_downweighted() {
        // Clean samples say 1 ms; congested samples (10x delay) say 8 ms.
        let mut pts: Vec<(f64, f64, f64)> =
            (0..8).map(|i| (i as f64 * 16.0, 1e-3, 0.0002)).collect();
        for i in 8..12 {
            pts.push((i as f64 * 16.0, 8e-3, 0.0025));
        }
        let r = reg_with(&pts);
        let est = r.regress(200.0).expect("estimate");
        assert!(
            (est.offset - 1e-3).abs() < 2e-3,
            "offset {} dragged by congested tail",
            est.offset
        );
    }

    #[test]
    fn no_freq_before_enough_span() {
        let pts: Vec<(f64, f64, f64)> = (0..3).map(|i| (i as f64 * 2.0, 1e-3, 0.0002)).collect();
        let r = reg_with(&pts);
        let est = r.regress(6.0).expect("estimate");
        assert!(est.freq_ppm.is_none());
    }

    #[test]
    fn slew_samples_keeps_the_fit_consistent() {
        // Clock runs +50 ppm undisciplined: θ decreases at 50 ppm from 10 ms.
        let mut r = SampleRegister::new(64);
        for i in 0..6 {
            let t = i as f64 * 16.0;
            r.push(Sample {
                t,
                offset: 0.010 - 50e-6 * t,
                delay: 0.0002,
                dispersion: 0.0,
            });
        }
        // At t=80 the discipline applies -50 ppm and adds 6 ms.
        r.slew_samples(80.0, -50.0, 0.006);
        // New world: remaining offset at t=80 is 10ms - 4ms - 6ms = 0, flat.
        let est = r.regress(80.0).expect("estimate");
        assert!(est.offset.abs() < 1e-9, "offset {}", est.offset);
        let f = est.freq_ppm.expect("freq");
        assert!(f.abs() < 1e-6, "slope {f} should be flat after correction");
    }

    #[test]
    fn best_prefers_min_delay() {
        let r = reg_with(&[(0.0, 1e-3, 0.010), (1.0, 2e-3, 0.001), (2.0, 3e-3, 0.020)]);
        let b = r.best().expect("best");
        assert!((b.offset - 2e-3).abs() < 1e-12);
    }
}
