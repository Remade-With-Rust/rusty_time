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
    /// Weight-floor width, as a fraction of the minimum observed delay.
    /// See [`SampleRegister::set_weight_floor_ratio`].
    weight_floor_ratio: f64,
    /// Weight-floor width used for the OFFSET only. See
    /// [`SampleRegister::set_offset_weight_floor_ratio`].
    offset_weight_floor_ratio: f64,
    /// Weight the SLOPE by the time each sample represents, not by its
    /// existence. See [`SampleRegister::set_slope_density_weighting`].
    slope_density_weighting: bool,
    /// If > 0, set the offset weight floor from the measured delay DISPERSION
    /// rather than from a fraction of the minimum delay. See
    /// [`SampleRegister::set_offset_weight_dispersion_k`].
    offset_weight_dispersion_k: f64,
    /// Half-life, seconds, of the age decay applied to the OFFSET weights.
    /// Infinite disables it. See
    /// [`SampleRegister::set_offset_age_halflife_s`].
    offset_age_halflife_s: f64,
}

/// Default weight-floor width, as a fraction of the minimum observed delay.
///
/// The regression weights a sample by the inverse square of its excess delay,
/// which is right: for exponential per-direction jitter the offset error of a
/// sample scales with its excess round trip, so `1/excess^2` is inverse-variance
/// weighting. The floor exists because that blows up as the excess goes to zero.
///
/// Tying its width to the PATH LENGTH is the questionable part — a sample's
/// offset error scales with the path's JITTER, and the two are unrelated. On
/// the corpus S1 path (200 us RTT, 10 us of jitter each way) this puts the
/// floor at 25 us against a ~20 us jitter scale, which flattens the weighting.
///
/// **That reasoning is correct, it is measurable, and narrowing the floor was
/// still rejected.** Halving it to 0.0625, paired against this default over
/// forty seeded worlds per scenario:
///
/// ```text
///        wins/40      z        verdict
/// S1       31      +3.48   RESOLVED better
/// S2        5      -4.74   RESOLVED worse
/// S4       15      -1.58   worse (per packet -2.21, RESOLVED worse)
/// S6        9      -3.48   RESOLVED worse
/// S8       16      -1.26   worse
/// ```
///
/// It buys S1 and sells every other path in the corpus. The reason is the same
/// trade the poll-rate sweep found: a narrower floor concentrates the fit onto
/// the few lowest-delay samples, which sharpens the OFFSET and shortens the
/// effective baseline for the SLOPE. S1 has a constant +20 ppm and does not
/// care about the slope; S2, S4, S6 and S8 all do.
///
/// The near-miss is worth recording. The change was confirmed out-of-sample on
/// forty fresh seeds — and only on S1 and S8, the two scenarios it had been
/// tuned on. It cleared |z| > 2 there and looked finished. Varying the SEED is
/// not varying the axis that can flip the answer; adding S2, S4 and S6 turned
/// a confirmed win into a three-way resolved regression.
pub const WEIGHT_FLOOR_RATIO: f64 = 0.125;

/// Weight-floor width for the OFFSET alone. Narrower than the slope's, on
/// purpose — this is the split that `set_offset_weight_floor_ratio` exists for.
///
/// Delay jitter in the corpus is drawn `exponential`, and real queueing is
/// skewed the same way: a packet can be delayed a great deal and cannot be
/// delivered early. Averaging that broadly does not merely add noise, it adds a
/// STANDING BIAS, because the whole tail sits on one side. Weighting the offset
/// by inverse variance rejects the tail; the slope is untouched, since a
/// constant bias does not tilt a line.
///
/// Measured against the previous single-weight fit, paired, fifty fresh seeded
/// worlds per scenario (S8 re-run at a hundred to settle a near-miss):
///
/// ```text
///        median |e|  ->  median |e|    wins      z       verdict
/// S1        1.22 us       1.25 us     28/50   +0.85    better, not resolved
/// S2      7164 us       6759 us       42/50   +4.81    RESOLVED better
/// S4      2575 us       2696 us       26/50   +0.28    neutral
/// S6         2.74 us       1.49 us    41/50   +4.53    RESOLVED better
/// S8         3.71 us       3.64 us   47/100   -0.60    neutral
/// ```
///
/// Two resolved improvements, no resolved regression, and convergence
/// unchanged (S1 5 s, S6 14-16 s, S8 5 s in both arms).
///
/// The S6 result is the one that names the mechanism. Its error was not noisy,
/// it was a standing **+2.74 us bias** where chrony sat at +0.05; the split
/// takes it to +0.95. A DC offset on the scenario with the largest transient is
/// what skewed-delay averaging looks like from the outside.
pub const OFFSET_WEIGHT_FLOOR_RATIO: f64 = 0.03125;

/// Minimum span (seconds) and count before the regression slope is reported.
const FREQ_MIN_SPAN_S: f64 = 8.0;
const FREQ_MIN_SAMPLES: usize = 4;

impl SampleRegister {
    pub fn new(capacity: usize) -> Self {
        SampleRegister {
            samples: Vec::with_capacity(capacity.max(3)),
            capacity: capacity.max(3),
            weight_floor_ratio: WEIGHT_FLOOR_RATIO,
            offset_weight_floor_ratio: OFFSET_WEIGHT_FLOOR_RATIO,
            slope_density_weighting: false,
            offset_weight_dispersion_k: 0.0,
            offset_age_halflife_s: f64::INFINITY,
        }
    }

    /// Set the weight-floor width. See [`WEIGHT_FLOOR_RATIO`].
    pub fn set_weight_floor_ratio(&mut self, ratio: f64) {
        self.weight_floor_ratio = ratio.max(1e-4);
    }

    /// Set the weight-floor width used for the OFFSET alone, leaving the slope
    /// fitted over the broad weights.
    ///
    /// The regression answers two questions with one weight set, and they want
    /// opposite things. WHERE the clock is, is best told by the few samples
    /// that queued least — concentrate. HOW FAST it is running is a slope, and
    /// a slope wants a long baseline — spread out. Every scalar tried on the
    /// corpus bought one by selling the other, always with the same shape:
    /// better on S1, whose oscillator is a constant +20 ppm and whose slope is
    /// therefore free, and resolved worse on every path that has to work for
    /// its frequency.
    ///
    /// Equal to `weight_floor_ratio` reproduces the single-weight behaviour
    /// exactly.
    pub fn set_offset_weight_floor_ratio(&mut self, ratio: f64) {
        self.offset_weight_floor_ratio = ratio.max(1e-4);
    }

    /// Set the half-life of the age decay on the OFFSET weights. Infinite (the
    /// default) weights every surviving sample by delay alone.
    ///
    /// This shortens an ARM, not a memory. The offset handed to the discipline
    /// is `mean_offset + b * (now - t0)`, where `t0` is the weighted mean
    /// sample time — and with delay-only weights that sits near the middle of
    /// the register, hundreds of seconds behind `now`. So the slope is
    /// multiplied by a long lever before it reaches the answer, and a slope
    /// error that is far too small to see becomes a standing offset: 2 ppb over
    /// 500 s is 1 us, which is the exact scale of the bias left on S6 after the
    /// weights were split.
    ///
    /// Decaying by age pulls `t0` toward `now` and shrinks the lever. It costs
    /// effective samples, so it is a trade rather than a free win, and the
    /// slope is deliberately left alone — it wants the long baseline.
    ///
    /// **Off by default, because the corpus will not agree on a value.** Paired
    /// against no decay, forty seeded worlds per scenario:
    ///
    /// ```text
    ///            S1        S2        S4        S6        S8
    /// h=150   +3.16     -4.43     +1.26     -3.16     +4.43
    /// h=300   +3.16     -5.06     +0.63     -3.16     +4.11
    /// h=600   +3.16     -5.06     -0.95     -3.16     +1.90
    /// ```
    ///
    /// Resolved better on S1 and S8, resolved worse on S2 and S6, at every
    /// half-life tried. Set it only if you know your own path drifts.
    ///
    /// The lever-arm reasoning above is also NOT why it helps where it helps:
    /// S6's standing bias went UP with decay (+0.88 -> +1.34 us), which the
    /// shortened lever was supposed to reduce. The mechanism is unexplained;
    /// only the measurement is trustworthy.
    ///
    /// **A gate on this was tried and removed.** The obvious fix for a knob the
    /// corpus disagrees about is to apply it only where it belongs — decay when
    /// the oscillator actually drifts, since that is the one thing that makes an
    /// old sample stale. The register fitted its two halves separately and
    /// compared the slopes against the standard error of their difference.
    /// Measured across the corpus, that statistic does not discriminate:
    ///
    /// ```text
    ///        p50    p75    p90    max    fires at K=1.5
    /// S1    0.49   0.80   1.19   1.98        2.5%
    /// S2    1.35   1.65   1.91   2.40       38.3%     <- steady, decay HURTS
    /// S4    1.03   1.78   2.28   3.55       40.9%
    /// S6    0.78   1.03   1.29   1.98        7.4%
    /// S8    0.79   1.26   2.26   2.77       20.0%     <- drifting, decay HELPS
    /// ```
    ///
    /// It fires nearly twice as often on S2, whose frequency is constant, as on
    /// S8, whose frequency is the random walk the test was built to find — S8's
    /// median separation sits BELOW S2's. No threshold can separate them,
    /// because on a high-jitter path the two half-slopes disagree from
    /// measurement noise long before any oscillator moves. Detecting drift
    /// needs a statistic that separates noise from wander by LAG — an Allan
    /// variance over successive frequency estimates — not a single window
    /// split.
    /// Weight each sample in the SLOPE fit by the span of time it represents,
    /// rather than letting every packet count equally.
    ///
    /// A least-squares slope is dominated by whatever sits furthest from the
    /// centroid, and `iburst` puts a dense cluster exactly there. With the
    /// extended acquisition burst a cold start lands up to twenty samples in
    /// the first forty seconds and about twenty-five more across the next
    /// twenty minutes: **almost half the register inside three percent of its
    /// time span**, all of it at one end of the axis.
    ///
    /// That cluster is a high-leverage anchor. It was taken while the clock was
    /// being slewed hard, so any residual error in it does not average out — it
    /// tilts the line. A tilt is a frequency error, and a frequency error held
    /// against a proportional drain is a STANDING OFFSET: 6 ppb over a 192 s
    /// correction time is 1.2 us, which is the bias S6 actually carries
    /// (+1.23 us, against chrony's +0.05).
    ///
    /// Scaling each weight by the local time spacing makes the fit approximate
    /// the continuous-time regression it was always meant to be, so twenty
    /// samples two seconds apart count for the forty seconds they cover rather
    /// than for twenty times a sixty-four-second poll. It changes nothing on a
    /// path that is sampled evenly, which is the point: this corrects a
    /// pathology rather than trading one scenario against another.
    pub fn set_slope_density_weighting(&mut self, on: bool) {
        self.slope_density_weighting = on;
    }

    /// Take the offset weight floor from the path's measured DISPERSION —
    /// `k * (median delay - min delay)` — instead of a fraction of the minimum
    /// delay. Zero (the default) keeps the min-delay fraction.
    ///
    /// The floor stands for the error scale of a zero-excess sample, and that
    /// scale is set by how much the path's delay VARIES, not by how long the
    /// path is. Tying it to `min_delay` makes the weighting a different shape on
    /// every path, and the corpus shows how far that goes:
    ///
    /// ```text
    ///        min delay   jitter scale   floor at 0.03125*min   weight at typical excess
    /// S1        200 us         ~20 us              6.2 us              ~0.09
    /// S2         40 ms          ~4 ms              1.2 ms              ~0.09
    /// S4         10 ms         ~40 ms              312 us            ~0.0002
    /// ```
    ///
    /// S1 and S2 happen to land in the same place because their jitter is a
    /// similar fraction of their path length. S4's is not — its jitter is four
    /// times its minimum delay — so the same constant produces weights three
    /// orders of magnitude smaller and the fit collapses onto one or two
    /// packets. That is the shape of S4's result: median 2781 us against
    /// chrony's 2022, and a worst case of 15150 against 10907.
    ///
    /// Setting the floor from dispersion makes the weighting the same SHAPE
    /// everywhere, which is what the inverse-variance argument assumed.
    ///
    /// **Off by default: the reasoning is right and the result does not carry.**
    /// Paired against the min-delay floor, forty seeded worlds per scenario:
    ///
    /// ```text
    ///            S1        S2        S4        S6        S8
    /// k=0.15  -0.63     +4.11     +0.95     -0.95     -0.63
    /// k=0.30  -0.63     -2.85     -0.32     -0.32     -2.21
    /// k=0.60  +0.95     -4.74     +0.00     -2.85     -0.32
    /// ```
    ///
    /// `k=0.15` does what the model predicted where the model applies — it is
    /// resolved better on S2 and trends better on S4, the two paths whose
    /// dispersion is least like S1's — and it trends mildly WORSE on the three
    /// paths the old constant already suited. One resolved gain against three
    /// unresolved losses is not a win, and the larger k values are resolved
    /// regressions outright.
    ///
    /// Kept as a knob because the argument survives the measurement: if a path
    /// has jitter far out of proportion to its length, the min-delay fraction
    /// is the wrong shape there and this is the correction. It is simply not a
    /// better DEFAULT for the corpus as it stands.
    pub fn set_offset_weight_dispersion_k(&mut self, k: f64) {
        self.offset_weight_dispersion_k = k.max(0.0);
    }

    pub fn set_offset_age_halflife_s(&mut self, halflife_s: f64) {
        self.offset_age_halflife_s = if halflife_s > 0.0 {
            halflife_s
        } else {
            f64::INFINITY
        };
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
        let floor = (min_delay * self.weight_floor_ratio).max(1e-9);
        let weight = |s: &Sample| {
            let excess = (s.delay - min_delay).max(0.0) + floor;
            let w = (floor / excess) * (floor / excess);
            w.max(1e-6)
        };

        // Time each sample stands for: half the gap to either neighbour, so the
        // factors sum to the window span however unevenly it was sampled.
        // Normalised to mean 1 so weight magnitudes stay comparable to the
        // un-weighted case (the `max(1e-6)` floor below is absolute).
        let density: Vec<f64> = if self.slope_density_weighting && n >= 3 {
            let t: Vec<f64> = self.samples.iter().map(|s| s.t).collect();
            let mut d = Vec::with_capacity(n);
            for i in 0..n {
                let lo = if i == 0 {
                    t[0]
                } else {
                    (t[i] + t[i - 1]) / 2.0
                };
                let hi = if i + 1 == n {
                    t[n - 1]
                } else {
                    (t[i] + t[i + 1]) / 2.0
                };
                d.push((hi - lo).max(0.0));
            }
            let mean = d.iter().sum::<f64>() / n as f64;
            if mean > 0.0 {
                for v in &mut d {
                    *v /= mean;
                }
                d
            } else {
                vec![1.0; n]
            }
        } else {
            Vec::new()
        };
        // `self.samples` is time-ordered and every trimmed set is a subsequence
        // of it, so a sample's factor is found by its time.
        let times: Vec<f64> = self.samples.iter().map(|s| s.t).collect();
        let weight = |s: &Sample| {
            let w = weight(s);
            if density.is_empty() {
                return w;
            }
            match times.binary_search_by(|probe| probe.total_cmp(&s.t)) {
                Ok(i) => w * density[i],
                Err(_) => w,
            }
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

        // Re-seat the OFFSET on sharper weights, holding the slope fixed.
        //
        // With the slope `b` already decided by the broad fit, the intercept
        // that minimises the sharply-weighted residuals is just the weighted
        // mean offset taken about the weighted mean time — the `b * (t - t0)`
        // terms cancel there by construction. So this is one pass and no
        // second solve, and it cannot disturb the frequency estimate.
        //
        // Runs only when the two ratios differ, so the default is bit-identical
        // to the single-weight path rather than merely equivalent in algebra.
        let decays = self.offset_age_halflife_s.is_finite();
        let offset_now = if self.offset_weight_floor_ratio != self.weight_floor_ratio
            || self.offset_weight_dispersion_k > 0.0
            || decays
        {
            let sharp_floor = if self.offset_weight_dispersion_k > 0.0 {
                // Median excess delay: the path's own noise scale, robust to
                // the long tail that a mean would follow.
                let mut d: Vec<f64> = used.iter().map(|s| s.delay).collect();
                d.sort_by(f64::total_cmp);
                let median = d[d.len() / 2];
                (self.offset_weight_dispersion_k * (median - min_delay))
                    .max(min_delay * 1e-4)
                    .max(1e-9)
            } else {
                (min_delay * self.offset_weight_floor_ratio).max(1e-9)
            };
            let halflife = self.offset_age_halflife_s;
            let sharp = |s: &Sample| {
                let excess = (s.delay - min_delay).max(0.0) + sharp_floor;
                let w = (sharp_floor / excess) * (sharp_floor / excess);
                let w = w.max(1e-6);
                if decays {
                    // Age is measured from `now`, not from the newest sample:
                    // the lever being shortened is to the present moment.
                    w * 0.5f64.powf(((now - s.t).max(0.0)) / halflife)
                } else {
                    w
                }
            };
            let mut sw = 0.0;
            let mut swt = 0.0;
            let mut swy = 0.0;
            for s in &used {
                let w = sharp(s);
                sw += w;
                swt += w * s.t;
                swy += w * s.offset;
            }
            if sw > 0.0 {
                let t0s = swt / sw;
                (swy / sw) + fit.b * (now - t0s)
            } else {
                fit.a + fit.b * (now - fit.t0)
            }
        } else {
            fit.a + fit.b * (now - fit.t0)
        };

        Some(RegressEstimate {
            offset: offset_now,
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
