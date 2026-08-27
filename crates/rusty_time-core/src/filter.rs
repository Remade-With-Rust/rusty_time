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
    /// Base weight of each sample, parallel to `samples` (dead prefix and all).
    ///
    /// A sample's weight depends on its own delay and on the window's minimum
    /// delay, and nothing else — so it changes when a sample arrives, and
    /// otherwise only on the rare estimate where the minimum moves. It was
    /// being recomputed for every sample on every estimate: a subtract, a
    /// clamp, an add, a divide, a multiply and a clamp, times the window, times
    /// forever, to arrive at the number it arrived at last time.
    weights: Vec<f64>,
    /// The `(min_delay, floor_ratio)` the cached weights were computed for.
    /// Any change to either invalidates all of them.
    weights_for: (f64, f64),
    /// Index of the oldest live sample within `samples`.
    ///
    /// The window is `samples[head..]`. Evicting the oldest is then `head += 1`
    /// rather than a memmove of the whole window, and the storage is compacted
    /// once every `capacity` pushes instead of on every one — the same total
    /// movement spread over sixty-four times fewer operations.
    ///
    /// A `VecDeque` does this too and was measured 1.9M Ir WORSE: its iterator
    /// has to handle a wrap, and that cost lands on every row of every
    /// estimate. A head offset keeps the window a plain contiguous slice, so
    /// the frequent path stays exactly as fast as it was.
    head: usize,
    /// Contiguous, deliberately.
    ///
    /// This is a sliding window, so once full every push evicts the oldest —
    /// `remove(0)`, which memmoves the whole window. A `VecDeque` makes that
    /// eviction O(1) and was measured **1.9M Ir WORSE**: its iterator has to
    /// handle a wrap, and `regress` walks every sample on every estimate to
    /// build its rows. The eviction happens once per sample; the walk happens
    /// n times per sample. Paying a small penalty on the frequent path to
    /// remove a large one from the rare path is a losing trade, and a
    /// sequential memmove is close to free on modern hardware anyway.
    samples: Vec<Sample>,
    capacity: usize,
    /// Reusable row buffers for `regress`.
    ///
    /// The regression needs two scratch windows per estimate — the working set
    /// and the spike-trimmed candidate — and it used to allocate and free both
    /// on every sample, forever, at a fixed size it already knew. They live
    /// here now because `Row` is a plain value: it borrows nothing from the
    /// register, so the buffers can outlive a call without making this type
    /// self-referential. That is what the flattening bought beyond locality.
    rows: Vec<Row>,
    rows_alt: Vec<Row>,
    /// Smallest delay currently in the register, maintained by `push`.
    ///
    /// The regression needs it on every estimate and it only ever changes when
    /// the contents do, so it is tracked at the one place that can change them
    /// rather than re-derived by scanning the whole window each time.
    min_delay: f64,
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

/// One sample as the regression passes want it: the three numbers they read,
/// plus the weight, laid out flat.
///
/// The passes used to walk `(&Sample, f64)` pairs, so every read of `t` or
/// `offset` chased a pointer into the register and pulled in a 32-byte Sample
/// to use 8 bytes of it. Copying the window into flat rows once per estimate
/// costs one pass and makes every later pass — and there are up to a dozen —
/// a linear walk over contiguous memory with no indirection.
#[derive(Clone, Copy, Debug)]
struct Row {
    t: f64,
    offset: f64,
    delay: f64,
    w: f64,
}

/// Minimum span (seconds) and count before the regression slope is reported.
const FREQ_MIN_SPAN_S: f64 = 8.0;
const FREQ_MIN_SAMPLES: usize = 4;

impl SampleRegister {
    pub fn new(capacity: usize) -> Self {
        SampleRegister {
            samples: Vec::with_capacity(2 * capacity.max(3)),
            weights: Vec::with_capacity(2 * capacity.max(3)),
            weights_for: (f64::NAN, f64::NAN),
            head: 0,
            capacity: capacity.max(3),
            rows: Vec::with_capacity(capacity.max(3)),
            rows_alt: Vec::with_capacity(capacity.max(3)),
            min_delay: f64::INFINITY,
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
        // The register is a time series and several readers depend on it —
        // `regress` takes the window span from the two ends rather than
        // scanning, which is only valid while this holds.
        debug_assert!(
            self.window().last().is_none_or(|last| s.t >= last.t),
            "samples must be pushed in non-decreasing time order"
        );
        self.samples.push(s);
        self.weights.push(0.0); // filled by `sync_weights`
        if self.samples.len() - self.head > self.capacity {
            let evicted = self.samples[self.head];
            self.head += 1;
            // Only a scan can find the new minimum once the old one leaves.
            if evicted.delay <= self.min_delay {
                let mut lowest = f64::INFINITY;
                for kept in &self.samples[self.head..] {
                    lowest = lowest.min(kept.delay);
                }
                self.min_delay = lowest;
            }
        }
        // Reclaim the dead prefix in one move, once it is as long as the
        // window itself.
        if self.head >= self.capacity {
            self.samples.drain(..self.head);
            self.weights.drain(..self.head);
            self.head = 0;
        }
        self.min_delay = self.min_delay.min(s.delay);
    }

    /// The live window: the samples that have not been evicted.
    fn window(&self) -> &[Sample] {
        &self.samples[self.head..]
    }

    /// Bring the cached weights up to date for the current minimum delay.
    ///
    /// Recomputes only when the inputs every weight shares have moved. The new
    /// sample pushed since the last estimate is covered by the same walk, so
    /// there is no separate incremental path to get wrong.
    fn sync_weights(&mut self, min_delay: f64, floor: f64) {
        let key = (min_delay, self.weight_floor_ratio);
        let stale = key != self.weights_for;
        let start = if stale {
            self.head
        } else {
            self.samples.len() - 1
        };
        for i in start..self.samples.len() {
            let excess = (self.samples[i].delay - min_delay).max(0.0) + floor;
            let q = floor / excess;
            self.weights[i] = (q * q).max(1e-6);
        }
        self.weights_for = key;
    }

    /// Drop history (after a clock step every stored offset is stale).
    pub fn clear(&mut self) {
        self.samples.clear();
        self.weights.clear();
        self.weights_for = (f64::NAN, f64::NAN);
        self.head = 0;
        self.min_delay = f64::INFINITY;
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
        // Specialised on which term is live, because at every call site one of
        // them is zero: the drain path passes an offset with no frequency
        // change, the plan path a frequency change with no offset. Written as
        // one expression, each sample paid for a multiply and an add against a
        // zero the caller already knew about.
        let window = &mut self.samples[self.head..];
        if dfreq_ppm == 0.0 {
            if doffset != 0.0 {
                for s in window {
                    s.offset -= doffset;
                }
            }
        } else {
            let rate = dfreq_ppm * 1e-6;
            if doffset == 0.0 {
                for s in window {
                    s.offset -= rate * (s.t - t_change);
                }
            } else {
                for s in window {
                    s.offset -= doffset + rate * (s.t - t_change);
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len() - self.head
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Best single sample (minimum delay among the most recent eight): the
    /// low-noise fallback while the regression is still warming up.
    pub fn best(&self) -> Option<Sample> {
        let win = self.window();
        let tail_start = win.len().saturating_sub(8);
        win[tail_start..]
            .iter()
            .copied()
            .min_by(|a, b| a.delay.total_cmp(&b.delay))
    }

    /// Weighted linear regression of offset against time, with one outlier-trim
    /// pass, extrapolated to `now`.
    pub fn regress(&mut self, now: f64) -> Option<RegressEstimate> {
        let n = self.len();
        if n < 3 {
            return None;
        }
        let min_delay = self.min_delay;
        debug_assert_eq!(
            min_delay,
            self.window()
                .iter()
                .map(|s| s.delay)
                .fold(f64::INFINITY, f64::min),
            "cached min_delay diverged from the register"
        );
        // Weight favors low-delay samples: queueing noise scales with excess delay.
        let floor = (min_delay * self.weight_floor_ratio).max(1e-9);
        self.sync_weights(min_delay, floor);

        // Time each sample stands for: half the gap to either neighbour, so the
        // factors sum to the window span however unevenly it was sampled.
        // Normalised to mean 1 so weight magnitudes stay comparable to the
        // un-weighted case (the `max(1e-6)` floor below is absolute).
        let density: Vec<f64> = if self.slope_density_weighting && n >= 3 {
            let t: Vec<f64> = self.window().iter().map(|s| s.t).collect();
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
        //
        // Built only when there are factors to look up. It used to be
        // unconditional — an allocation and a copy of every timestamp on every
        // estimate, feeding a lookup that the very next line skipped, because
        // density weighting is off by default. Work done for a feature this
        // caller does not use is the cheapest kind of win there is.
        let times: Vec<f64> = if density.is_empty() {
            Vec::new()
        } else {
            self.window().iter().map(|s| s.t).collect()
        };
        let density_factor = |s: &Sample| -> f64 {
            match times.binary_search_by(|probe| probe.total_cmp(&s.t)) {
                Ok(i) => density[i],
                Err(_) => 1.0,
            }
        };

        // Each sample's weight, computed ONCE.
        //
        // It used to be a closure handed to `wls_fit`, which evaluates it in
        // three separate loops — and `regress` calls `wls_fit` up to six times
        // (the initial fit, four trim refits, one spike refit). That is up to
        // eighteen evaluations of the same value per sample per estimate, each
        // one a subtract, two divides, a multiply and two clamps, plus a
        // binary search when density weighting is on.
        //
        // Nothing it depends on changes across the passes: `min_delay` and the
        // floor are fixed for the call, and trimming only ever REMOVES rows. So
        // the weight travels with its sample and every pass reads it.
        // Taken from the register and given back before returning, so the
        // allocation happens once in the process rather than once per estimate.
        let mut used = std::mem::take(&mut self.rows);
        used.clear();
        // Two builds, not one with a test inside. Whether there are density
        // factors at all is decided before the walk and cannot change during
        // it, and there are none by default — so the common path was asking
        // once per sample a question already answered.
        // Caching the OFFSET weight the same way was tried and measured
        // neutral-to-worse (+27k Ir): it must be carried in the row, because
        // trimming leaves a row unable to find its own index again, and the
        // wider row costs as much to copy as the divisions it saves.
        let cached = &self.weights[self.head..];
        if density.is_empty() {
            used.extend(self.window().iter().zip(cached).map(|(s, &w)| Row {
                t: s.t,
                offset: s.offset,
                delay: s.delay,
                w,
            }));
        } else {
            used.extend(self.window().iter().zip(cached).map(|(s, &w)| Row {
                t: s.t,
                offset: s.offset,
                delay: s.delay,
                w: w * density_factor(s),
            }));
        }
        let mut fit = wls_fit(&used)?;

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
            if residuals_well_mixed(&used, &fit) {
                break; // residuals look well mixed: one regime
            }
            let (half_gap, mad, _) = residual_half_gap_and_mad(&used, &fit);
            if half_gap <= 3.0 * (1.4826 * mad).max(1e-9) {
                break; // a lone spike, not a regime change — pass 2's job
            }
            let drop = (used.len() / 4).max(2);
            used.drain(..drop);
            match wls_fit(&used) {
                Some(refit) => fit = refit,
                None => break,
            }
        }

        // Pass 2 — interior spike trim (a delayed packet), thresholded on the
        // robust MAD scale (an rms threshold is inflated by the spike itself and
        // masks it), protecting the newest samples so the present can never be
        // voted away.
        if used.len() > 4 {
            // `None` when nothing droppable exceeds the threshold, which is the
            // large majority of estimates on a converged loop — the threshold is
            // about three sigma. Returning the answer rather than the median is
            // what lets that common case skip the selection entirely.
            if let Some(threshold) = spike_threshold(&used, &fit) {
                let protect_from = used.len() - 3;
                let mut kept = std::mem::take(&mut self.rows_alt);
                kept.clear();
                kept.extend(
                    used.iter()
                        .copied()
                        .enumerate()
                        .filter(|(i, r)| {
                            *i >= protect_from
                                || (r.offset - (fit.a + fit.b * (r.t - fit.t0))).abs() <= threshold
                        })
                        .map(|(_, r)| r),
                );
                if kept.len() >= 3
                    && kept.len() < used.len()
                    && let Some(refit) = wls_fit(&kept)
                {
                    // Swap rather than assign: both buffers stay alive and go back
                    // to the register, so neither allocation is ever repeated.
                    std::mem::swap(&mut used, &mut kept);
                    fit = refit;
                }
                self.rows_alt = kept;
            }
        }

        // Ends, not a scan. The register is a time series: `push` appends and
        // `slew_samples` rewrites offsets only, so `self.samples` is ascending
        // in `t`, and every trimmed set here is a subsequence of it — pass 1
        // drops a prefix, pass 2 filters in order. So the first and last rows
        // ARE the extremes, and folding min/max over the whole window was
        // recomputing something the ordering already guarantees.
        let span = match (used.first(), used.last()) {
            (Some(lo), Some(hi)) => (hi.t - lo.t).max(0.0),
            _ => 0.0,
        };

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
        // Accumulators for the residual dispersion, filled by whichever walk
        // over the final rows happens anyway.
        let mut sd_sw = 0.0f64;
        let mut sd_sr = 0.0f64;
        let mut sd_done = false;
        let decays = self.offset_age_halflife_s.is_finite();
        let offset_now = if self.offset_weight_floor_ratio != self.weight_floor_ratio
            || self.offset_weight_dispersion_k > 0.0
            || decays
        {
            let sharp_floor = if self.offset_weight_dispersion_k > 0.0 {
                // Median excess delay: the path's own noise scale, robust to
                // the long tail that a mean would follow.
                let mut d: Vec<f64> = used.iter().map(|r| r.delay).collect();
                d.sort_by(f64::total_cmp);
                let median = d[d.len() / 2];
                (self.offset_weight_dispersion_k * (median - min_delay))
                    .max(min_delay * 1e-4)
                    .max(1e-9)
            } else {
                (min_delay * self.offset_weight_floor_ratio).max(1e-9)
            };
            let halflife = self.offset_age_halflife_s;
            let base = |r: &Row| {
                let excess = (r.delay - min_delay).max(0.0) + sharp_floor;
                let w = (sharp_floor / excess) * (sharp_floor / excess);
                w.max(1e-6)
            };
            let mut sw = 0.0;
            let mut swt = 0.0;
            let mut swy = 0.0;
            // The age-decay test is hoisted out of the walk. It is a property
            // of the configuration, not of the sample, and it is off by
            // default — so the common path was branching once per row on a
            // question whose answer was fixed before the loop began.
            if decays {
                for r in &used {
                    // Age is measured from `now`, not from the newest sample:
                    // the lever being shortened is to the present moment.
                    let w = base(r) * 0.5f64.powf(((now - r.t).max(0.0)) / halflife);
                    sw += w;
                    swt += w * r.t;
                    swy += w * r.offset;
                }
            } else {
                // The dispersion is accumulated in this same walk. It is a
                // separate pass over the identical rows immediately afterwards,
                // reading the same three fields — the loop is the cost, not the
                // arithmetic, and the arithmetic is unchanged, so the value is
                // bit-identical.
                for r in &used {
                    let w = base(r);
                    sw += w;
                    swt += w * r.t;
                    swy += w * r.offset;

                    sd_sw += r.w;
                    let e = r.offset - (fit.a + fit.b * (r.t - fit.t0));
                    sd_sr += r.w * e * e;
                }
                sd_done = true;
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

        let n_used = used.len();
        // The one dispersion this call reports, taken from the fit that
        // survived — then the buffer goes back to the register.
        let offset_sd = if sd_done {
            finish_sd(sd_sw, sd_sr, used.len())
        } else {
            residual_sd(&used, &fit)
        };
        self.rows = used;

        Some(RegressEstimate {
            offset: offset_now,
            freq_ppm,
            offset_sd,
            n_used,
            span,
        })
    }
}

/// (|mean residual of older half − mean residual of newer half|, median absolute
/// residual). The gap detects regime changes; the MAD is a spike-robust scale.
/// Handing this function a reusable `&mut Vec` to spare the allocation was
/// tried THREE times and measured worse every time: +3.4M Ir with a caller
/// local, +2.4M with the register owning it, and +1.1M again after this
/// function was made `#[inline(always)]` — the retry was fair, because that
/// changed the baseline the first two were measured against. The indirection
/// is paid on every element of every pass; the allocation is paid once per
/// estimate and glibc serves it from a hot bin. Settled, and not to be
/// re-litigated without a fourth reason.
/// Inlined on purpose. It is called from two places in one function, and
/// letting it inline lets the residual buffer live in the caller's frame and
/// its loops merge with the surrounding code — measured 1.7M Ir.
#[inline(always)]
fn residual_half_gap_and_mad(samples: &[Row], fit: &Fit) -> (f64, f64, f64) {
    let resid = |r: &Row| r.offset - (fit.a + fit.b * (r.t - fit.t0));
    let n = samples.len();
    if n == 0 {
        return (0.0, 0.0, 0.0);
    }
    let half = n / 2;
    // The residual of each sample is computed ONCE.
    //
    // It used to be evaluated twice — once walking the halves for their means,
    // once building the absolute values — and this runs up to five times per
    // estimate. Collect the signed residuals with `collect()` (a sized
    // iterator, so one allocation and a tight loop), sum the halves off that,
    // then take absolute values in place. Every loop stays branch-free and
    // vectorisable, which is what a fused loop with an `if i < half` inside it
    // is not: that version was measured 1.8M Ir WORSE than the duplicated
    // arithmetic it removed.
    let mut abs: Vec<f64> = samples.iter().map(resid).collect();
    // Gating this on a `want_gap` flag so pass 2 could skip it was measured
    // 0.9M Ir WORSE: the branch costs more than the two sums it guards, and it
    // breaks the straight-line codegen of the whole function.
    let sum_old: f64 = abs[..half].iter().sum();
    let sum_new: f64 = abs[half..].iter().sum();
    let mean_old = if half == 0 {
        0.0
    } else {
        sum_old / half as f64
    };
    let mean_new = if n == half {
        0.0
    } else {
        sum_new / (n - half) as f64
    };
    let gap = (mean_old - mean_new).abs();
    for v in &mut abs {
        *v = v.abs();
    }
    // Largest residual among the samples pass 2 is allowed to drop — the three
    // newest are protected and can never be trimmed, so they must not decide
    // whether trimming is worth attempting. Free here: the values are already
    // in hand.
    let droppable = n.saturating_sub(3);
    // A plain comparison, not `fold(0.0, f64::max)`. `f64::max` carries IEEE
    // NaN semantics that a `>` test does not, and swapping to it measured
    // 2.9M Ir WORSE.
    let mut worst = 0.0f64;
    for v in &abs[..droppable] {
        if *v > worst {
            worst = *v;
        }
    }
    // Selection, not a sort.
    //
    // Only the middle element is ever read, and `select_nth_unstable_by_key`
    // places exactly that element at that index in O(n) while leaving the rest
    // merely partitioned. Keyed on the raw bit pattern: these are absolute
    // values, so every one is non-negative, and across non-negative floats the
    // IEEE-754 bit pattern is monotonic — ordering by `to_bits()` is
    // order-identical to ordering by value, at one integer compare each.
    let mid = abs.len() / 2;
    abs.select_nth_unstable_by_key(mid, |v| v.to_bits());
    (gap, abs[mid], worst)
}

/// The spike-trim threshold for pass 2 — `Some` only when something actually
/// exceeds it.
///
/// The caller's test is `worst > (3.0 * 1.4826 * mad).max(1e-9)`, and it used
/// to get there by selecting the median. That selection was **17% of the entire
/// client path** — 18,493 partitions for 16,000 estimates — to answer a
/// question that is almost always "no".
///
/// It is answerable exactly without the median. `v -> 3.0 * 1.4826 * v` is
/// monotone non-decreasing over the non-negative reals, so sorting by `v` also
/// sorts by `f(v)`, and therefore `f(mad)` is the `mid`-th smallest `f`-value.
/// So `worst > f(mad)` holds precisely when at least `mid + 1` of the values
/// satisfy `f(v) < worst` — a counting pass, no partition. The `.max(1e-9)` arm
/// separates cleanly: `worst > max(f(mad), 1e-9)` is `worst > f(mad)` **and**
/// `worst > 1e-9`.
///
/// The median is then selected only on the rare path that is going to use it,
/// so the threshold handed back is bit-identical to the old one.
#[inline(always)]
fn spike_threshold(samples: &[Row], fit: &Fit) -> Option<f64> {
    let n = samples.len();
    if n == 0 {
        return None;
    }
    let resid = |r: &Row| r.offset - (fit.a + fit.b * (r.t - fit.t0));
    let mut abs: Vec<f64> = samples.iter().map(|r| resid(r).abs()).collect();

    // The three newest are protected and can never be trimmed, so they must not
    // decide whether trimming is worth attempting.
    let droppable = n.saturating_sub(3);
    let mut worst = 0.0f64;
    for v in &abs[..droppable] {
        if *v > worst {
            worst = *v;
        }
    }
    // `worst` starts at zero and only ever grows through a `>` test, so it
    // cannot be NaN and the plain comparison is exact here.
    if worst <= 1e-9 {
        return None;
    }

    let mid = n / 2;
    let mut below = 0usize;
    for v in &abs {
        if 3.0 * 1.4826 * *v < worst {
            below += 1;
        }
    }
    if below < mid + 1 {
        return None;
    }

    abs.select_nth_unstable_by_key(mid, |v| v.to_bits());
    Some((3.0 * 1.4826 * abs[mid]).max(1e-9))
}

/// Whether the residuals are well mixed — that is, whether the number of runs
/// of same-signed residuals reaches a third of the window. A well-mixed fit has
/// about n/2 runs; a kinked window (two regimes) has very few.
///
/// Returns the ANSWER rather than the count, so it can stop as soon as the
/// answer is known. The caller only ever asked `runs * 3 >= len`, and on a
/// converged loop that becomes true about a third of the way in — every sample
/// visited after that was a residual computed to reach a conclusion already
/// reached.
fn residuals_well_mixed(samples: &[Row], fit: &Fit) -> bool {
    let need = samples.len();
    let mut runs = 0usize;
    let mut last_sign = 0i8;
    for row in samples {
        let r = row.offset - (fit.a + fit.b * (row.t - fit.t0));
        let sign = if r > 0.0 { 1 } else { -1 };
        if sign != last_sign {
            runs += 1;
            last_sign = sign;
            if runs * 3 >= need {
                return true;
            }
        }
    }
    false
}

struct Fit {
    /// Offset at t0.
    a: f64,
    /// Slope, s/s.
    b: f64,
    /// Time origin (mean of used sample times).
    t0: f64,
}

/// Weighted least squares over rows that already carry their weight.
///
/// NOT inlined: forcing it measured 0.9M Ir worse. It has up to six call sites
/// in `regress` and duplicating three loops at each of them costs more than
/// the calls do — the opposite of `residual_half_gap_and_mad`, which has two.
fn wls_fit(samples: &[Row]) -> Option<Fit> {
    let n = samples.len();
    if n < 2 {
        return None;
    }
    let mut sw = 0.0;
    let mut swt = 0.0;
    for r in samples {
        sw += r.w;
        swt += r.w * r.t;
    }
    if sw <= 0.0 {
        return None;
    }
    let t0 = swt / sw;

    let mut sxx = 0.0;
    let mut sxy = 0.0;
    let mut sy = 0.0;
    for r in samples {
        let x = r.t - t0;
        sxx += r.w * x * x;
        sxy += r.w * x * r.offset;
        sy += r.w * r.offset;
    }
    let b = if sxx > 1e-12 { sxy / sxx } else { 0.0 };
    let a = sy / sw; // intercept at t0 (x is centered)

    Some(Fit { a, b, t0 })
}

/// Weighted residual standard deviation of a fit over its window.
///
/// Split out of `wls_fit` because only the FINAL fit's dispersion is ever
/// read. `regress` makes up to six fits — the initial one, four trim refits and
/// a spike refit — and each one used to walk the whole window a third time to
/// compute a number that the next trim immediately discarded. It is computed
/// once now, on whichever fit survives, from the same inputs and in the same
/// order, so the value is bit-identical.
fn residual_sd(samples: &[Row], fit: &Fit) -> f64 {
    let n = samples.len();
    let mut sw = 0.0;
    let mut sr = 0.0;
    for row in samples {
        sw += row.w;
        let r = row.offset - (fit.a + fit.b * (row.t - fit.t0));
        sr += row.w * r * r;
    }
    finish_sd(sw, sr, n)
}

/// The tail of `residual_sd`, shared with the caller that accumulates the same
/// sums inside a walk it was doing anyway.
fn finish_sd(sw: f64, sr: f64, n: usize) -> f64 {
    if sw <= 0.0 {
        return 0.0;
    }
    let dof = (n as f64 - 2.0).max(1.0);
    (sr / sw * n as f64 / dof).sqrt()
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
        let mut r = reg_with(&pts);
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
        let mut r = reg_with(&pts);
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
        let mut r = reg_with(&pts);
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
        let mut r = reg_with(&pts);
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
