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
    /// Gain of the integral trim on the frequency estimate. 0 disables it,
    /// leaving a purely proportional loop. See `FREQ_INTEGRAL_GAIN`.
    pub freq_integral_gain: f64,
    /// Step the poll interval back DOWN when `|offset| > this * noise`.
    ///
    /// This is the packet budget, and the packet budget is most of the
    /// accuracy: offset error falls as 1/sqrt(N). Measured on the seeded rig,
    /// clknetsim's own packet counters, same poll bounds for both arms:
    ///
    /// ```text
    ///            mean poll   median |e| S1   per packet spent
    /// chrony        33.9 s        1.52 us    (baseline)
    /// rusty_time    40.1 s        1.47 us    x1.05
    /// ```
    ///
    /// So the estimator was never the deficit — at equal cost it is at parity
    /// on S1 and slightly ahead on S8 (x0.96). We were simply buying fewer
    /// samples. See `POLL_DOWN_NOISE_RATIO`.
    pub poll_down_noise_ratio: f64,
    /// Consecutive stable samples required before the poll interval doubles.
    ///
    /// This, not the dead band, is what sets the packet budget. Sweeping
    /// `poll_down_noise_ratio` from 10 down to 3 moved the mean poll by 0.3 s
    /// and the accuracy not at all, because the step-DOWN branch only runs
    /// when `|offset| >= 2 * noise` and a converged loop is almost never
    /// there. It is always "stable", so it always climbs, and it pins at
    /// maxpoll. The climb rate is the only term with any authority.
    pub poll_up_streak: u32,
    /// Width of the regression's weight floor, as a fraction of the minimum
    /// observed delay. See `rusty_time_core::filter::WEIGHT_FLOOR_RATIO`.
    ///
    /// This is the one knob that can improve accuracy WITHOUT spending more
    /// packets, which is why it is worth a sweep: buying accuracy with poll
    /// rate leaves per-packet efficiency exactly where it was.
    pub weight_floor_ratio: f64,
    /// Weight-floor width for the OFFSET alone; the slope keeps
    /// `weight_floor_ratio`. Equal values reproduce the single-weight fit.
    pub offset_weight_floor_ratio: f64,
    /// Half-life, seconds, of the age decay on the OFFSET weights. Infinite
    /// disables it, weighting by delay alone.
    pub offset_age_halflife_s: f64,
    /// If > 0, take the offset weight floor from measured delay dispersion
    /// rather than a fraction of the minimum delay.
    pub offset_weight_dispersion_k: f64,
    /// Weight the slope fit by the time each sample represents, so an `iburst`
    /// cluster cannot act as a high-leverage anchor on the frequency estimate.
    pub slope_density_weighting: bool,
    /// Absolute steady-state correction time, seconds. 0 keeps the default
    /// behaviour of `CORR_TIME_RATIO * poll_interval`.
    ///
    /// The drain rate is `offset / correction_time`, and tying that time to the
    /// POLL makes the loop's aggressiveness a function of how often it looks.
    /// Polling twice as fast then does not average twice as much — it halves
    /// the time constant and writes twice as much sample noise into the clock,
    /// which is why every attempt to buy accuracy with packets has failed here:
    /// the packets were spent on twitchiness, not precision.
    ///
    /// With an absolute time constant, a faster poll delivers what it should —
    /// more samples inside the same correction window.
    ///
    /// **Off by default: measured, and it does not deliver.** The diagnosis is
    /// sound — an absolute time constant plus chrony's packet rate is the only
    /// pairing that could turn per-packet parity into raw-accuracy advantage,
    /// and neither half can show it alone. Paired against chrony, forty seeded
    /// worlds each:
    ///
    /// ```text
    ///                S1      S2      S4      S6      S8     poll
    /// base        -0.63   +0.63   -1.90   -2.21   -1.26    ~40 s
    /// t=200       -0.95   +0.63   -0.63   -3.48   -1.26    ~38 s
    /// t=120,k8    +0.32   +1.90   -0.32   -2.21   -1.90    ~32 s
    /// t=200,k8    +0.32   +0.63   -0.32   -2.53   -2.85    ~31 s
    /// ```
    ///
    /// Nothing resolves ahead anywhere, and S6 stays resolved behind in every
    /// arm. The absolute constant also destabilises the poll adaptation — S2
    /// fell to a 21 s poll, spending a third more packets for no gain — because
    /// the stability test that raises the interval is calibrated against a
    /// correction time that now no longer moves with it.
    pub corr_time_s: f64,
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
            freq_integral_gain: FREQ_INTEGRAL_GAIN,
            poll_down_noise_ratio: POLL_DOWN_NOISE_RATIO,
            poll_up_streak: POLL_UP_STREAK,
            weight_floor_ratio: crate::filter::WEIGHT_FLOOR_RATIO,
            offset_weight_floor_ratio: crate::filter::OFFSET_WEIGHT_FLOOR_RATIO,
            offset_age_halflife_s: f64::INFINITY,
            offset_weight_dispersion_k: 0.0,
            slope_density_weighting: false,
            corr_time_s: 0.0,
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
/// Correction time, in poll intervals, for an offset that is plainly real.
/// One means "finish before the next sample arrives".
const ACQUIRE_CORR_RATIO: f64 = 1.0;
/// How far outside the noise an offset must sit to be treated as real.
const ACQUIRE_NOISE_MULTIPLE: f64 = 10.0;
/// How many updates count as acquisition.
///
/// Acquisition is a *phase*, not a magnitude. Gating the fast correction on
/// "the offset is much larger than the noise" looked equivalent and is not:
/// a loop that is confidently wrong reports a small `offset_sd` beside a large
/// error, so the test fires in steady state exactly when it should not. The
/// in-house S6 scenario is such a case — a deliberately noisy path where the
/// estimator's own confidence outruns its accuracy — and gating on magnitude
/// alone took its steady error from 2.54 ms to 10.83 ms while the low-noise
/// clknetsim rig showed only the improvement. Counting updates cannot be
/// fooled that way: after this many the loop is no longer starting up,
/// whatever it believes about itself.
const ACQUIRE_UPDATES: u32 = 8;
/// The most of the slew budget the fast correction may ask for.
///
/// Leaving headroom is the point. A correction that consumes the whole budget
/// pins the clock at maximum rate for the entire interval, and the frequency
/// estimator then has to infer a drift from samples taken while the clock was
/// being hauled — which it does badly enough to leave a permanently worse
/// steady state. A quarter keeps the fast path for offsets it can absorb and
/// hands genuinely large cold starts back to the gentle drain.
const ACQUIRE_SLEW_SHARE: f64 = 0.25;
/// How many times the noise an offset must exceed before the clock may be
/// hauled at the full slew ceiling.
const ACQUIRE_FULL_SPEED_CONFIDENCE: f64 = 10_000.0;
/// Above this share of the slew ceiling, the clock is being *hauled*, and a
/// frequency measured across that haul is not a measurement of the
/// oscillator.
///
/// The regression fits a slope through stored samples, and `slew_samples`
/// re-expresses that history for corrections already applied. That accounting
/// is exact for a gentle drain. It is not robust to a correction running at
/// most of the slew ceiling: any small mismatch between the rate commanded and
/// the rate delivered is multiplied by the poll interval and lands in the
/// slope, and the loop then carries a frequency error it never measured. The
/// offset drain has feedback and recovers; the frequency term accumulates and
/// does not.
///
/// So during a haul the offset is still corrected at full speed — the clock is
/// visibly wrong and the fix is not in doubt — but the frequency estimate is
/// left alone until the samples describe a clock that is merely running.
const FREQ_TRUST_SLEW_SHARE: f64 = 0.25;
/// Most polls the acquisition burst may take before it must slow down.
///
/// The burst normally ends after `IBURST_COUNT` samples, and the poll then
/// jumps straight to `min_poll`. That is the whole S6 gap: the offset drain is
/// sized to finish within one poll interval, so ending the burst with a large
/// correction still outstanding hands the remainder a 16 s deadline instead of
/// a 2 s one. Measured against chrony, chrony had a 500 ms cold start gone in
/// about 7 s at close to its slew ceiling, while this loop cleared 500 ms down
/// to 89 ms in the burst and then spent a further 16 s on what was left.
///
/// So the burst ends when the offset is small, not when a counter runs out.
/// The cap is what stops that becoming an unbounded fast poll against someone
/// else's server: a client that cannot converge is a client that must back off
/// anyway, not one that should keep asking every two seconds.
const MAX_ACQUIRE_BURST: u32 = 16;
/// How much of an implied frequency error to absorb per update.
///
/// **Why there is an integral term at all.** The offset drain is proportional:
/// each plan removes `offset / (CORR_TIME_RATIO * poll)` per second. Against a
/// constant unmodelled drift `F`, that settles at an equilibrium rather than at
/// zero -- removal balances accumulation when
///
/// ```text
///     offset  =  CORR_TIME_RATIO * poll * F
/// ```
///
/// which is a *standing error the loop maintains on purpose*. Measured on the
/// in-house corpus it is the whole story: S1 sat at 200 us on a 0.039 ppm
/// residual and a 1024 s poll, and 3 * 1024 * 0.039e-6 is 120 us. A
/// proportional controller cannot remove it; only integral action can.
///
/// The frequency term comes from the regression slope, which is a
/// *measurement*. If that measurement carries any bias, the equilibrium above
/// stands forever and no amount of averaging removes it. So the loop reads the
/// standing offset as evidence in its own right: invert the relation, and a
/// persistent offset **is** a frequency error, expressed in seconds.
///
/// **Measured and rejected. The default is 0 — the trim is OFF.**
///
/// The reasoning above is sound and the result still went the other way. On a
/// SEEDED rig, twenty worlds per arm, paired seed by seed:
///
/// ```text
/// S8  gain=0.0   median |e| 4.78 us    8/20 wins vs chrony   z=-0.89  not resolved
/// S8  gain=0.1   median |e| 6.20 us    5/20 wins vs chrony   z=-2.24  RESOLVED, chrony ahead
/// S1  gain=0.0   median |e| 1.47 us
/// S1  gain=0.1   median |e| 1.98 us
/// ```
///
/// Turning the trim on is the only *resolved* accuracy result in that sweep,
/// and it is a regression. An earlier single unpaired run had read it as an
/// improvement on both scenarios; it was the draw, not the code.
///
/// Why it fails, as best the data supports: the standing offset is not a
/// frequency error here. It is sampling error in the delay draws — it changes
/// SIGN with the seed. Integrating it feeds noise into the frequency estimate,
/// and on S8, whose oscillator already wanders, that is the last thing the
/// loop needs.
///
/// Kept as a field rather than deleted so re-testing costs one flag if the
/// estimator's own bias ever shrinks below this effect.
const FREQ_INTEGRAL_GAIN: f64 = 0.0;

/// How far outside the noise an offset must sit before the poll interval is
/// stepped back down — the default for `DisciplineConfig::poll_down_noise_ratio`.
///
/// An offset below `2 * noise` counts as stable and, after three such samples,
/// doubles the interval. Between that and this ratio the loop does neither, so
/// this number IS the width of the dead band, and a wide dead band pins the
/// client at maxpoll: at 10x it effectively never came back down.
///
/// The value is measured, not chosen — see the sweep in `DisciplineConfig`.
const POLL_DOWN_NOISE_RATIO: f64 = 10.0;

/// Consecutive stable samples before the poll interval doubles — the default
/// for `DisciplineConfig::poll_up_streak`.
const POLL_UP_STREAK: u32 = 3;
/// Weight of the newest offset in the persistence average. Low, because the
/// signal being extracted is the part that does *not* change.
const OFFSET_EWMA_ALPHA: f64 = 0.25;

/// The offset at which acquisition is finished and the burst may end.
///
/// Tied to what "converged" means rather than to a multiple of the noise. The
/// noise-multiple test was tried first and fails at exactly the wrong moment:
/// on S6 the burst had hauled 500 ms down to 9.8 ms, `10 x noise` came out at
/// about 10 ms, the test went false, the poll jumped 2 s -> 16 s, and the last
/// 9.8 ms was handed a 16 s deadline. Those 16 s were the whole difference
/// against chrony. Floored at twice the noise so a genuinely noisy path is not
/// polled fast in pursuit of an offset it cannot resolve.
const ACQUIRE_DONE_S: f64 = 1e-3;

#[derive(Clone, Debug)]
pub struct Discipline {
    cfg: DisciplineConfig,
    freq_ppm: f64,
    updates: u32,
    poll: i8,
    stable_streak: u32,
    iburst_left: u32,
    /// Drain rate the previous plan commanded, as a share of the ceiling.
    /// Samples taken since then were taken while the clock moved at that rate.
    last_drain_share: f64,
    /// Burst polls used, including any the acquisition extension granted.
    burst_used: u32,
    /// Slow average of recent offset estimates.
    ///
    /// A *persistent* offset is the signature of a frequency error the
    /// regression has not measured, and it is the thing that decides
    /// steady-state accuracy. See `integral_trim`.
    offset_ewma: f64,
    /// Whether `offset_ewma` has been seeded.
    ewma_seeded: bool,
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
            last_drain_share: 0.0,
            burst_used: 0,
            offset_ewma: 0.0,
            ewma_seeded: false,
        }
    }

    /// How much of the slew budget an acquisition correction may use, given
    /// how well the offset is known.
    ///
    /// How fast the clock may be hauled should depend on how sure we are where
    /// it is going. A 500 ms offset on a path with microseconds of jitter is
    /// known to five decimal places and can be cleared at the ceiling; the same
    /// 500 ms on a path with a millisecond of jitter is a much rougher number,
    /// and committing to it at full speed writes the roughness into the clock.
    ///
    /// Both rigs demanded this. On clknetsim, restricting the share left S6 at
    /// 18 s against chrony's 12 s; on the in-house corpus, whose S6 models a
    /// 0.74 ms-jitter path, allowing the full share took its steady error from
    /// 1.5 ms to 5.9 ms. Neither constant satisfies both, because the two rigs
    /// differ by two orders of magnitude in exactly the quantity that should
    /// decide it.
    fn acquire_share(&self, offset: f64, noise: f64) -> f64 {
        let confidence = offset.abs() / noise.max(1e-9);
        if confidence >= ACQUIRE_FULL_SPEED_CONFIDENCE {
            1.0
        } else {
            ACQUIRE_SLEW_SHARE
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
        // frequency error of the *disciplined* clock, so accumulate it fully --
        // unless these samples were taken while the clock was being hauled, in
        // which case the slope is mostly the haul.
        let hauling = self.last_drain_share > FREQ_TRUST_SLEW_SHARE;
        if let Some(fm) = freq_ppm_meas
            && !hauling
        {
            self.freq_ppm =
                (self.freq_ppm + fm).clamp(-self.cfg.max_freq_ppm, self.cfg.max_freq_ppm);
        }

        // Poll adaptation first: lengthen when quiet, shorten when the offset is
        // loud relative to the noise floor. Runs before the drain computation so
        // the drain rate is sized for the interval the plan will actually use.
        let noise = offset_sd.max(1e-7);

        // Integral trim: read a standing offset as the frequency error it
        // implies, and absorb a fraction of it. Only once acquisition is over
        // -- during acquisition the offset is large for reasons that have
        // nothing to do with drift, and feeding that in would be nonsense.
        if self.ewma_seeded {
            self.offset_ewma =
                (1.0 - OFFSET_EWMA_ALPHA) * self.offset_ewma + OFFSET_EWMA_ALPHA * offset;
        } else {
            self.offset_ewma = offset;
            self.ewma_seeded = true;
        }
        // ...and only when the standing offset is larger than the noise that
        // could have produced it. Below that line the average is a sample of
        // jitter, and feeding jitter into the frequency term writes it into the
        // clock permanently -- the offset drain can recover from a bad estimate,
        // the frequency term accumulates it. Measured: without this gate S1
        // went from 199.7 us to 231.5 us while its frequency residual did not
        // move at all, which is exactly what integrating noise looks like.
        if self.cfg.freq_integral_gain != 0.0
            && self.updates > ACQUIRE_UPDATES
            && self.offset_ewma.abs() > noise
        {
            let poll_now = self.peek_poll_interval();
            let implied_freq_ppm = (self.offset_ewma / (CORR_TIME_RATIO * poll_now)) * 1e6;
            self.freq_ppm = (self.freq_ppm + self.cfg.freq_integral_gain * implied_freq_ppm)
                .clamp(-self.cfg.max_freq_ppm, self.cfg.max_freq_ppm);
        }
        if offset.abs() < 2.0 * noise {
            self.stable_streak += 1;
            if self.stable_streak >= self.cfg.poll_up_streak && self.poll < self.cfg.max_poll {
                self.poll += 1;
                self.stable_streak = 0;
            }
        } else {
            self.stable_streak = 0;
            if offset.abs() > self.cfg.poll_down_noise_ratio * noise
                && self.poll > self.cfg.min_poll
            {
                self.poll -= 1;
            }
        }

        // Offset: drain over ~CORR_TIME_RATIO poll intervals, capped by maxslewrate.
        //
        // ...except while the offset is unambiguous. The loop re-plans on every
        // sample, so a drain sized to finish in three poll intervals only ever
        // runs for one of them before being replaced: the offset decays by a
        // third per poll, giving a time constant three times longer than the
        // ratio suggests. In steady state that is exactly the wanted
        // behaviour — it is what stops sample noise being written into the
        // clock. During acquisition it is not: a 10 ms startup offset is a
        // hundred times the noise floor, it is not in dispute, and decaying it
        // by a third per 16 s poll leaves the clock wrong for a minute.
        // Measured against chrony under clknetsim, chrony had removed the same
        // offset within about two seconds while this loop was still 40 s away.
        //
        // The test is the one the poll adaptation already uses: an offset far
        // outside the noise is a real error, not a noisy reading, so correct
        // it within the interval. Once it is comparable to the noise the
        // gentle ratio takes over again, and steady-state accuracy — which is
        // at parity with chrony — is untouched.
        // Keep the acquisition burst going while a correction is still
        // outstanding. The drain is sized to finish within one poll interval,
        // so ending the burst early does not merely delay the next
        // measurement — it stretches the correction itself from two seconds to
        // sixteen. On S6 that single step was the entire gap against chrony:
        // the burst hauled 500 ms down to 9.8 ms by t=10.5 s, then handed what
        // was left a 16 s deadline and finished at t=26 s where chrony
        // finished at t=12 s.
        if self.iburst_left == 0
            && self.cfg.iburst
            && self.burst_used < MAX_ACQUIRE_BURST
            && offset.abs() > ACQUIRE_DONE_S.max(2.0 * noise)
        {
            self.iburst_left = 1;
        }

        let poll_s = self.peek_poll_interval();
        //
        // The fast path's premise is "finish this correction before the next
        // sample". If the rate that would take is above the slew ceiling, the
        // correction cannot finish within the interval, the premise is false,
        // and asking for it anyway just pins the clock at maximum slew for the
        // whole interval — which is how a 500 ms cold start went from a 2.54 ms
        // steady error to 10.83 ms on the noisy in-house rig while the
        // low-noise one showed only the improvement. So the fast ratio applies
        // only when it is actually achievable, and a correction too large to
        // finish is drained gently, as before.
        let acquiring = self.updates <= ACQUIRE_UPDATES;
        // Rate: gentle by default, faster while acquiring.
        //
        // The rate stays tied to the poll interval even though drains are now
        // budgeted and stop when spent. Untying it was tried — "clear any
        // acquisition offset in ACQUIRE_TARGET_S seconds" — and it is worse:
        // with a 16 s poll it corrects the whole of each noisy estimate in two
        // seconds and then coasts for fourteen, which chases noise instead of
        // averaging it. Scaling with the poll is what makes the correction
        // proportional to how often the loop actually gets to look.
        //
        // What the budget buys is not a faster rate here. It is that the rate
        // is now free to be chosen at all: an over-fast drain no longer sails
        // past the offset, it stops at it. Measured on the same binary, the
        // same discipline with budgets unenforced settles at 579 us on S6 and
        // with them enforced at 130 us.
        let wanted_rate_ppm = if acquiring && offset.abs() > ACQUIRE_NOISE_MULTIPLE * noise {
            // Move at the fastest rate allowed and stop when the offset is
            // gone. This is only expressible because the drain carries a
            // budget: without one, a rate this high would not stop at the
            // offset, it would sail past it, so the rate had to be "the offset
            // divided by the poll interval" and a cold start's remainder was
            // handed the poll's deadline. That is what put S6 at 26 s against
            // chrony's 12 s.
            // Scales with the poll, so a long interval gets a gentle rate and
            // the loop averages noise instead of chasing it. A fixed clearing
            // time was tried and is wrong for exactly that reason: at a 64 s
            // poll, "clear it in 2 s" is thirty times more aggressive than the
            // interval warrants, and the in-house S6 steady error went from
            // 1.5 ms to 9 ms.
            //
            // The ceiling is the whole slew budget rather than a quarter of
            // it. That is safe only because the drain stops when spent: an
            // over-fast rate now runs out at the offset instead of sailing
            // past it, and what it delivered is booked even if the caller wakes
            // late. Without those two properties this cap had to stay low.
            // Poll-scaled, with a ceiling that depends on how well the offset
            // is known.
            //
            // Untying the rate from the poll entirely -- "clear it in
            // ACQUIRE_TARGET_S" -- was tried twice and measured worse both
            // times: 16 s on S6 against 14 s here, and on the noisy rig a fixed
            // clearing time at a 64 s poll is thirty times more aggressive than
            // the interval warrants, which chases jitter instead of averaging
            // it. Scaling with the poll is what keeps the correction
            // proportional to how often the loop gets to look.
            ((offset.abs() / (ACQUIRE_CORR_RATIO * poll_s)) * 1e6)
                .min(self.cfg.max_slew_ppm * self.acquire_share(offset, noise))
        } else {
            // Correction time: poll-scaled by default, absolute when asked.
            let corr_time = if self.cfg.corr_time_s > 0.0 {
                self.cfg.corr_time_s
            } else {
                CORR_TIME_RATIO * poll_s
            };
            (offset.abs() / corr_time) * 1e6
        };
        let drain_rate_ppm = wanted_rate_ppm.min(self.cfg.max_slew_ppm);
        self.last_drain_share = if self.cfg.max_slew_ppm > 0.0 {
            drain_rate_ppm / self.cfg.max_slew_ppm
        } else {
            0.0
        };

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

    /// How long to wait before trying again when an exchange yields nothing —
    /// lost, or rejected because the server was not yet usable.
    ///
    /// This is the iburst spacing while the burst budget lasts, *not* the poll
    /// interval. A server that has only just started answers its first requests
    /// with the unsynchronised leap indicator, which a client must refuse; if
    /// that refusal then costs a full poll interval, a cold start is delayed by
    /// 16 seconds before the first usable sample. Measured against chrony under
    /// clknetsim, that single wait was most of an 8x convergence gap.
    ///
    /// Nothing is consumed here: a failed exchange must not spend burst budget,
    /// or a few early losses would silently end the burst.
    pub fn retry_interval_s(&self) -> f64 {
        self.peek_poll_interval()
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
            self.burst_used += 1;
            IBURST_SPACING_S
        } else {
            2f64.powi(self.poll as i32)
        }
    }
}

#[cfg(test)]
mod acquisition_tests {
    use super::*;

    fn acquiring() -> Discipline {
        Discipline::new(DisciplineConfig {
            makestep_threshold: None,
            min_poll: 4, // 16 s
            iburst: true,
            ..DisciplineConfig::default()
        })
    }

    #[test]
    fn the_burst_continues_while_a_correction_is_outstanding() {
        // The drain is sized to finish within one poll interval, so ending the
        // burst with an offset still outstanding does not just delay the next
        // measurement — it stretches the correction from 2 s to 16 s. Against
        // chrony on S6 that one step was the whole gap: 500 ms was hauled down
        // to 9.8 ms by the burst, and the remainder then took another 16 s.
        let mut d = acquiring();
        let mut plan = None;
        for _ in 0..IBURST_COUNT + 3 {
            // A 10 ms offset, far above both the 1 ms target and the noise.
            plan = Some(d.on_estimate(0.010, None, 1e-6));
        }
        let next = plan.expect("a plan").next_poll_s;
        assert!(
            next <= IBURST_SPACING_S,
            "burst ended with 10 ms still outstanding: next poll {next} s"
        );
    }

    #[test]
    fn the_burst_ends_once_the_offset_is_small() {
        // ...and it must end, or a converged client polls a stranger's server
        // every two seconds forever.
        let mut d = acquiring();
        let mut plan = None;
        for _ in 0..IBURST_COUNT + 3 {
            plan = Some(d.on_estimate(1e-6, None, 1e-6));
        }
        let next = plan.expect("a plan").next_poll_s;
        assert!(
            next > IBURST_SPACING_S,
            "burst kept running on a converged clock: next poll {next} s"
        );
    }

    #[test]
    fn the_extended_burst_is_bounded() {
        // A client that never converges must back off rather than keep asking.
        let mut d = acquiring();
        let mut plan = None;
        for _ in 0..MAX_ACQUIRE_BURST * 3 {
            plan = Some(d.on_estimate(0.010, None, 1e-6));
        }
        let next = plan.expect("a plan").next_poll_s;
        assert!(
            next > IBURST_SPACING_S,
            "burst never backed off despite never converging: next poll {next} s"
        );
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
