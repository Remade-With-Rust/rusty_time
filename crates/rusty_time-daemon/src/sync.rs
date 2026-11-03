//! `rtimed sync` — the resident client daemon.
//!
//! Polls its sources, runs the discipline loop, and applies the result to the
//! system clock. This is the chronyd-equivalent mode, and it is what the
//! TIMECORP cross-implementation arm measures.
//!
//! The discipline itself is `rusty_time_core::client::MultiController`, which
//! is also what the simulator drives. That shared type is the reason a corpus
//! number here means anything: the alternative — a simulator with its own copy
//! of the loop — measures code that never ships.

use crate::nts_session::{NtsSession, NtsSessionError};
use rusty_time_clock::{ClockDrive, ClockRead, SystemClock, net};
use rusty_time_core::client::MultiController;
use rusty_time_core::discipline::ChangeVerdict;
use rusty_time_core::ntp::{self, HEADER_LEN, LeapIndicator, Mode, NtpPacket, NtpTimestamp};
use rusty_time_core::select::select;
use rusty_time_core::{ClockCommand, DisciplineConfig, LeapMode, Sample, SourceEstimate};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct SyncOptions {
    pub servers: Vec<String>,
    pub port: u16,
    /// Measure and report, but never touch the clock. The default is to
    /// discipline, because that is what a time daemon is for; this exists so
    /// the behaviour can be observed without privilege.
    pub dry_run: bool,
    /// Stop after this many seconds. 0 runs forever.
    pub run_seconds: u64,
    pub discipline: DisciplineConfig,
    pub timeout_ms: u64,
    /// Print one line per exchange, for the corpus harness to parse.
    pub verbose: bool,
    /// Where the control socket lives. Same default and same resolution as
    /// `rtimed serve`, so `rtimec` finds either role without being told which
    /// is running.
    pub control_path: String,
    /// Also answer NTP on this address with the clock being steered — a
    /// router serving its LAN. Replies say unsynchronised (leap 3, stratum 16)
    /// until a correction has reached the clock, then carry the selected
    /// source's stratum + 1 and reference id.
    pub serve_bind: Option<String>,
    /// Authenticate every source with NTS (RFC 8915): each server name is an
    /// NTS-KE host, and a reply that does not verify is never used.
    pub nts: bool,
    /// NTS-KE port; only consulted when `nts` is set.
    pub ke_port: u16,
    /// Extra trust anchors (PEM) for NTS-KE: a private CA or a pinned server.
    pub nts_ca_pem: Option<String>,
    /// chrony's `nocerttimecheck`, for a machine with no battery clock: until
    /// this process has put its first correction on the clock, judge NTS-KE
    /// certificate dates at the chain's own time rather than the local clock,
    /// which is known to be wrong. Chain, signatures and name are verified
    /// regardless; after the first correction, dates are checked as usual.
    pub nocerttimecheck: bool,
}

/// Whether this process has put a correction on the clock. Until then a clock
/// with no battery behind it (a router at its image's build date) is no basis
/// for judging certificate dates.
static CLOCK_SET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl SyncOptions {
    pub fn parse(args: &[String]) -> Result<SyncOptions, String> {
        let mut opts = SyncOptions {
            servers: Vec::new(),
            port: 123,
            dry_run: false,
            run_seconds: 0,
            discipline: DisciplineConfig::default(),
            timeout_ms: 2000,
            verbose: false,
            control_path: crate::control::default_path(),
            serve_bind: None,
            nts: false,
            ke_port: rusty_time_nts::KE_PORT,
            nts_ca_pem: None,
            nocerttimecheck: false,
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = || -> Result<String, String> {
                it.next().cloned().ok_or(format!("{arg} needs a value"))
            };
            match arg.as_str() {
                "--control" => opts.control_path = value()?,
                "--serve" => opts.serve_bind = Some(value()?),
                "--nts" => opts.nts = true,
                "--nocerttimecheck" => opts.nocerttimecheck = true,
                "--ke-port" => {
                    opts.ke_port = value()?.parse().map_err(|_| "--ke-port: not a number")?;
                }
                "--nts-ca" => {
                    let path = value()?;
                    opts.nts_ca_pem =
                        Some(std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?);
                }
                "--dry-run" => opts.dry_run = true,
                "--verbose" | "-v" => opts.verbose = true,
                "--port" => {
                    opts.port = value()?.parse().map_err(|_| "--port: not a number")?;
                }
                "--seconds" => {
                    opts.run_seconds = value()?.parse().map_err(|_| "--seconds: not a number")?;
                }
                "--timeout-ms" => {
                    opts.timeout_ms = value()?.parse().map_err(|_| "--timeout-ms: not a number")?;
                }
                "--minpoll" => {
                    opts.discipline.min_poll =
                        value()?.parse().map_err(|_| "--minpoll: not a number")?;
                }
                "--maxpoll" => {
                    opts.discipline.max_poll =
                        value()?.parse().map_err(|_| "--maxpoll: not a number")?;
                }
                "--makestep" => {
                    let threshold: f64 = value()?
                        .parse()
                        .map_err(|_| "--makestep: threshold is not a number")?;
                    let limit: i64 = value()?
                        .parse()
                        .map_err(|_| "--makestep: limit is not a number")?;
                    opts.discipline.makestep_threshold = Some(threshold);
                    opts.discipline.makestep_limit =
                        if limit < 0 { u32::MAX } else { limit as u32 };
                }
                "--freq-integral-gain" => {
                    opts.discipline.freq_integral_gain = value()?
                        .parse()
                        .map_err(|_| "--freq-integral-gain: not a number")?;
                }
                "--poll-down-ratio" => {
                    opts.discipline.poll_down_noise_ratio = value()?
                        .parse()
                        .map_err(|_| "--poll-down-ratio: not a number")?;
                }
                "--poll-up-streak" => {
                    opts.discipline.poll_up_streak = value()?
                        .parse()
                        .map_err(|_| "--poll-up-streak: not a number")?;
                }
                "--weight-floor-ratio" => {
                    opts.discipline.weight_floor_ratio = value()?
                        .parse()
                        .map_err(|_| "--weight-floor-ratio: not a number")?;
                }
                "--offset-weight-floor-ratio" => {
                    opts.discipline.offset_weight_floor_ratio = value()?
                        .parse()
                        .map_err(|_| "--offset-weight-floor-ratio: not a number")?;
                }
                "--offset-age-halflife" => {
                    opts.discipline.offset_age_halflife_s = value()?
                        .parse()
                        .map_err(|_| "--offset-age-halflife: not a number")?;
                }
                "--offset-weight-dispersion-k" => {
                    opts.discipline.offset_weight_dispersion_k = value()?
                        .parse()
                        .map_err(|_| "--offset-weight-dispersion-k: not a number")?;
                }
                "--slope-density" => opts.discipline.slope_density_weighting = true,
                "--corr-ratio" => {
                    opts.discipline.corr_time_ratio =
                        value()?.parse().map_err(|_| "--corr-ratio: not a number")?;
                }
                "--corr-time" => {
                    opts.discipline.corr_time_s =
                        value()?.parse().map_err(|_| "--corr-time: not a number")?;
                }
                "--maxchange" => {
                    // chrony's three-argument form: offset, start, ignore.
                    let offset: f64 = value()?
                        .parse()
                        .map_err(|_| "--maxchange: offset is not a number")?;
                    let start: u32 = value()?
                        .parse()
                        .map_err(|_| "--maxchange: start is not a number")?;
                    let ignore: i32 = value()?
                        .parse()
                        .map_err(|_| "--maxchange: ignore is not a number")?;
                    if !(offset.is_finite() && offset > 0.0) {
                        return Err(
                            "--maxchange: offset must be a positive number of seconds".into()
                        );
                    }
                    opts.discipline.max_change_s = Some(offset);
                    opts.discipline.max_change_start = start;
                    opts.discipline.max_change_ignore = ignore;
                }
                "--corr-time-max" => {
                    opts.discipline.corr_time_max_s = value()?
                        .parse()
                        .map_err(|_| "--corr-time-max: not a number")?;
                }
                "--leapsecmode" => {
                    opts.discipline.leap_mode = match value()?.as_str() {
                        "slew" => LeapMode::Slew,
                        "step" => LeapMode::Step,
                        "ignore" => LeapMode::Ignore,
                        other => {
                            return Err(format!(
                                "--leapsecmode: expected slew, step or ignore, got '{other}'"
                            ));
                        }
                    };
                }
                "--no-makestep" => opts.discipline.makestep_threshold = None,
                "--no-iburst" => opts.discipline.iburst = false,
                other if other.starts_with("--") => {
                    return Err(format!("unknown flag '{other}'"));
                }
                server => opts.servers.push(server.to_string()),
            }
        }
        if opts.servers.is_empty() {
            return Err("at least one server is required".into());
        }
        Ok(opts)
    }
}

/// One configured source.
struct Source {
    name: String,
    socket: UdpSocket,
    /// The NTS session when `--nts` is set: every request spends one of its
    /// cookies, and only a reply that verifies under its keys is used.
    nts: Option<NtsSession>,
    /// Monotonic instant of the next due poll.
    due: Instant,
    /// Latest estimate, for selection across sources.
    last_offset_s: f64,
    last_root_distance_s: f64,
    last_stratum: u8,
    has_estimate: bool,
    exchanges: u64,
    lost: u64,
    /// Losses since the last successful exchange. Cumulative `lost` cannot
    /// answer "is this source answering right now", which is what the quorum
    /// below needs: a source that dropped four packets an hour ago is not
    /// unreachable.
    lost_in_a_row: u32,
    /// Whether this source announced a leap second in its last reply. The
    /// indicator is set for the whole UTC day the leap falls in, so it arrives
    /// long before the step does.
    leap_pending: bool,
}

pub fn run(opts: &SyncOptions) -> i32 {
    let clock = SystemClock;
    let caps = rusty_time_clock::capabilities();

    if !opts.dry_run && !caps.can_discipline {
        eprintln!("rtimed sync: this process cannot discipline the clock.");
        eprintln!("             needs {}", caps.discipline_requirement);
        eprintln!("             (use --dry-run to measure without adjusting)");
        return 1;
    }

    // The discipline loop's estimate, published for the control socket. Until
    // the first accepted correction this reports unsynchronized with an
    // infinite error bound, so a consumer that asks early is told "do not
    // decide anything with me" rather than handed a zero offset.
    let tracking = Arc::new(Mutex::new(crate::control::TrackingState::unsynchronized()));
    {
        let ctl_state = crate::control::ControlState::for_sync(Arc::clone(&tracking));
        let ctl_path = opts.control_path.clone();
        std::thread::spawn(move || {
            // A control socket that cannot bind must never stop the clock from
            // being steered — report and carry on disciplining.
            if let Err(e) = crate::control::serve(&ctl_path, ctl_state) {
                eprintln!("rtimed sync: control plane unavailable: {e}");
            }
        });
    }

    // Never plan a slew the driver cannot deliver. The discipline's bookkeeping
    // assumes the rate it asked for is the rate that ran; if the platform
    // silently clamps, the controller subtracts a correction that never
    // happened and the regression reads the shortfall as a frequency error.
    let mut discipline = opts.discipline;
    discipline.max_slew_ppm = discipline.max_slew_ppm.min(caps.max_slew_ppm);

    // SIGTERM and SIGINT are honoured from here on: by the wait below, and by
    // the discipline loop, which never waits longer than 500 ms between
    // checks. Without a handler, a jailed or containerised rtimed (PID 1 of
    // its namespace) ignores SIGTERM entirely.
    rusty_time_clock::stop::on_stop_signal(rusty_time_clock::stop::StopMode::Flag);

    // A router starts its time daemon before its uplink is up: no name
    // resolves and no route exists, so every source fails to open. Wait for
    // one, as chronyd and BusyBox ntpd do, rather than exit. With `--seconds`
    // the wait counts against the run, so a bounded run still ends.
    let waiting_since = Instant::now();
    let mut retry = Duration::from_secs(2);
    let mut attempts: u32 = 0;
    let mut sources = loop {
        attempts += 1;
        let mut opened = Vec::new();
        let mut failures = Vec::new();
        for name in &opts.servers {
            match open_source(name, &discipline, opts) {
                Ok(source) => opened.push(source),
                Err(e) => failures.push(format!("rtimed sync: {name}: {e}")),
            }
        }
        if !opened.is_empty() {
            for failure in &failures {
                eprintln!("{failure}");
            }
            if attempts > 1 {
                println!("rtimed sync: a source opened after {attempts} attempts");
            }
            break opened;
        }
        if attempts == 1 {
            for failure in &failures {
                eprintln!("{failure}");
            }
            eprintln!("rtimed sync: no usable sources yet; retrying");
        }
        if opts.run_seconds > 0 && waiting_since.elapsed().as_secs() >= opts.run_seconds {
            eprintln!("rtimed sync: no usable sources");
            return 1;
        }
        let resume = Instant::now() + retry;
        while Instant::now() < resume {
            if rusty_time_clock::stop::stop_requested() {
                println!("rtimed sync: stopping");
                return 0;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        retry = (retry * 2).min(Duration::from_secs(64));
    };

    // Measured on the same clock the samples use, so "how long until the first
    // exchange" is answerable from the log rather than inferred.
    let mono_start = clock.mono_s().unwrap_or(0.0);
    println!(
        "rtimed: syncing from {} source(s){} (startup at mono {:.3})",
        sources.len(),
        if opts.dry_run { " (dry run)" } else { "" },
        mono_start
    );

    // The LAN side, when asked for: starts unsynchronised and follows the loop.
    let served = match &opts.serve_bind {
        None => None,
        Some(bind) => match crate::server::serve_following(bind) {
            Ok(state) => {
                println!(
                    "rtimed sync: serving NTP on {bind} (unsynchronised until the clock is steered)"
                );
                Some(state)
            }
            Err(e) => {
                eprintln!("rtimed sync: {e}");
                return 1;
            }
        },
    };
    // When the served view last came from an accepted correction. A loop
    // that has heard nothing for four maximum poll intervals stops vouching
    // for the clock to the LAN, as ntpd and chronyd do.
    let mut served_synced_at: Option<Instant> = None;
    let served_stale_after =
        Duration::from_secs_f64(4.0 * 2f64.powi(opts.discipline.max_poll.into()));

    // Measurement arm, resolved once: RUSTY_TIME_NO_DRAIN_STOP=1 leaves drains
    // running until the next plan replaces them, which is what this daemon did
    // before drains carried a budget.
    let stop_drains = std::env::var_os("RUSTY_TIME_NO_DRAIN_STOP").is_none();

    // Consecutive refusals from the clock driver. A handful can be transient;
    // a run of them means this process cannot steer the clock at all, and
    // continuing would be a daemon that logs errors while quietly reporting
    // success.
    const MAX_REFUSALS: u32 = 10;
    let mut refused_in_a_row: u32 = 0;

    // Whether the clock is being held because the sources disagree. Tracked so
    // the message appears once per episode rather than once per poll.
    let mut holding = false;

    // ONE loop for the clock, one register per source. The frequency, the drain
    // and its budget are properties of the clock, of which there is exactly
    // one; only the sample history belongs to a source. Giving every source its
    // own copy of the loop is what made multi-source selection unreliable.
    let mut ctl = MultiController::new(opts.discipline, sources.len());

    let mut drains_retired: u64 = 0;
    let started = Instant::now();
    let mut driver = SystemClock;
    let mut applied_any = false;

    let mut stopped = false;

    loop {
        if opts.run_seconds > 0 && started.elapsed().as_secs() >= opts.run_seconds {
            break;
        }
        if rusty_time_clock::stop::stop_requested() {
            println!("rtimed sync: stopping");
            stopped = true;
            break;
        }
        if let (Some(served), Some(at)) = (&served, served_synced_at)
            && at.elapsed() > served_stale_after
        {
            if let Ok(mut s) = served.lock() {
                s.stratum = crate::server::STRATUM_UNSYNCHRONIZED;
                s.upstream = crate::server::Upstream::UNSYNCHRONIZED;
            }
            served_synced_at = None;
            eprintln!(
                "rtimed sync: no accepted correction for {served_stale_after:?}; serving as unsynchronised"
            );
        }

        // Retire any drain whose budget is spent.
        //
        // This is what makes `ClockCommand::Slew`'s `drain_offset` mean
        // something. Without it the drain is not a correction of a known size,
        // it is a frequency that runs until the next packet arrives — so its
        // rate could only ever be "the offset divided by the poll interval",
        // because anything faster would sail past the offset instead of
        // stopping at it. Waking for the end of the drain is what lets the
        // rate be chosen for how fast the clock may safely move.
        //
        // There is ONE drain, because there is one clock. This used to be a
        // loop over per-source drains that had to withhold the command from
        // every unselected source — otherwise a falseticker could impose its
        // frequency the moment its own drain happened to expire. With a shared
        // loop that hazard cannot be expressed: no unselected source owns a
        // drain to expire.
        let mono_now = clock.mono_s().unwrap_or(0.0);
        if stop_drains && let Some(command) = ctl.poll_drain(mono_now) {
            drains_retired += 1;
            if opts.verbose {
                println!(
                    "t={:8.3} drain retired (#{drains_retired}) freq={:+.3}",
                    mono_now - mono_start,
                    ctl.freq_ppm()
                );
            }
            if !opts.dry_run
                && let Err(e) = driver.apply(&command)
            {
                // Retiring a drain only ever asks the clock to STOP draining,
                // so a refusal leaves it running faster than the books say.
                // Count it with the rest: the condition it signals is the same.
                refused_in_a_row += 1;
                eprintln!("rtimed sync: ending drain: {e} (refused {refused_in_a_row}x)");
            }
        }

        // Wait until the earliest source is due, or until a drain runs out —
        // whichever comes first.
        let now = Instant::now();
        let next_due = sources.iter().map(|s| s.due).min().unwrap_or(now);
        let until_due = next_due.saturating_duration_since(now);
        let until_drain = ctl
            .drain_completes_at()
            .filter(|_| stop_drains)
            .map(|at| at - mono_now)
            .filter(|remaining| *remaining > 0.0)
            .unwrap_or(f64::INFINITY);
        let wait = if until_drain.is_finite() {
            until_due.min(Duration::from_secs_f64(until_drain))
        } else {
            until_due
        };
        if !wait.is_zero() {
            // The 500 ms ceiling keeps the loop responsive to shutdown; a drain
            // ending sooner than that is woken for exactly.
            std::thread::sleep(wait.min(Duration::from_millis(500)));
            continue;
        }

        for index in 0..sources.len() {
            if sources[index].due > Instant::now() {
                continue;
            }
            match exchange(&mut sources[index], opts, &clock) {
                Some((sample, stratum, root_distance)) => {
                    let mono_now = match clock.mono_s() {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    // `exchange` has just recorded whether this reply carried a
                    // leap announcement, so the discipline can treat the second
                    // as expected rather than as a source misbehaving.
                    let leap_pending = sources[index].leap_pending;

                    // MEASURE. Every source that answers updates its own
                    // history, whether or not it is the one steering. This
                    // touches no clock state, so an unselected source leaves
                    // nothing behind to revert, confirm or adopt — which is the
                    // entire class of bug the shared loop removes.
                    let est = ctl.observe(index, mono_now, sample);
                    {
                        let source = &mut sources[index];
                        source.exchanges += 1;
                        source.lost_in_a_row = 0;
                        source.last_offset_s = est.offset_s;
                        // Root distance describes how well the PATH is known. On
                        // its own it says nothing about how well this source's
                        // offset is known, and during acquisition those differ by
                        // orders of magnitude — a hundred microseconds of path
                        // against milliseconds of estimate. Selection compares
                        // intervals of `offset ± root_distance`, so leaving the
                        // estimate's own dispersion out makes every interval far
                        // too narrow to overlap: on the three-server rig a set of
                        // perfectly healthy servers formed no majority on 74
                        // polls out of 89.
                        source.last_root_distance_s = root_distance + est.sd_s;
                        source.last_stratum = stratum;
                        source.has_estimate = true;
                    }

                    // SELECT on the estimates as they now stand, including the
                    // one that just arrived.
                    let selected = selected_index(&sources);

                    // STEER, from the selected source only. An unselected
                    // source is rescheduled on the shared poll interval and
                    // otherwise does nothing at all.
                    let Some(step) = (if selected == Some(index) {
                        Some(ctl.steer(est, mono_now, leap_pending))
                    } else {
                        sources[index].due =
                            Instant::now() + Duration::from_secs_f64(ctl.poll_interval_s());
                        None
                    }) else {
                        hold_if_no_majority(
                            selected,
                            &mut holding,
                            &mut ctl,
                            &mut driver,
                            opts,
                            sources.len(),
                        );
                        continue;
                    };
                    sources[index].due =
                        Instant::now() + Duration::from_secs_f64(step.plan.next_poll_s);

                    if opts.verbose {
                        println!(
                            "t={:8.3} sample source={} offset={:+.9} freq={:+.3} \
                             samples={} poll={}",
                            mono_now - mono_start,
                            sources[index].name,
                            step.estimate_offset_s,
                            step.applied_ppm,
                            step.samples_used,
                            step.plan.next_poll_s
                        );
                    }

                    // A correction the guard refused must not reach the clock,
                    // and one that exhausts the allowance ends the process. A
                    // daemon that keeps refusing corrections while reporting
                    // success is a machine whose clock is quietly wrong, which
                    // is the state this guard exists to make impossible.
                    match step.verdict {
                        ChangeVerdict::Accepted => {}
                        ChangeVerdict::Refused { offset_s, seen } => {
                            eprintln!(
                                "rtimed sync: {src} asked for a {offset_s:+.3} s correction, beyond the {limit:.3} s limit — refused ({seen}x)",
                                src = sources[index].name,
                                limit = opts.discipline.max_change_s.unwrap_or(0.0),
                            );
                        }
                        ChangeVerdict::GiveUp { offset_s } => {
                            eprintln!(
                                "rtimed sync: {src} still asking for a {offset_s:+.3} s correction beyond the {limit:.3} s limit after {ignore} refusals — giving up rather than running a clock this daemon has decided it cannot steer",
                                src = sources[index].name,
                                limit = opts.discipline.max_change_s.unwrap_or(0.0),
                                ignore = opts.discipline.max_change_ignore,
                            );
                            return 1;
                        }
                    }

                    // The selected source's plan goes to the clock. There is no
                    // "else" any more: an unselected source never reached this
                    // point, because it never produced a plan.
                    if holding {
                        eprintln!("rtimed sync: majority restored — steering again");
                        holding = false;
                    }
                    let mut steered = false;
                    if !opts.dry_run {
                        match driver.apply(&step.plan.command) {
                            Ok(()) => {
                                applied_any = true;
                                CLOCK_SET.store(true, std::sync::atomic::Ordering::Relaxed);
                                refused_in_a_row = 0;
                                steered = true;
                                ctl.confirm_last_plan();
                            }
                            Err(e) => {
                                // The command did not reach the clock, so the
                                // books that assumed it did must be put back.
                                // Left standing, the registers would carry a
                                // correction that never happened and the
                                // regression would read it as truth — the
                                // daemon would report itself synchronised while
                                // the clock ran free.
                                ctl.revert_last_plan();
                                refused_in_a_row += 1;
                                eprintln!(
                                    "rtimed sync: applying clock command: {e}                                      (refused {refused_in_a_row}x; correction reverted)"
                                );
                                if refused_in_a_row >= MAX_REFUSALS {
                                    eprintln!(
                                        "rtimed sync: the clock has refused {MAX_REFUSALS}                                          consecutive corrections — giving up rather than                                          reporting a synchronisation that is not happening"
                                    );
                                    return 1;
                                }
                            }
                        }
                        if let ClockCommand::Step { add_seconds } = step.plan.command {
                            println!("rtimed sync: stepped clock by {add_seconds:+.6} s");
                        }
                    }

                    // Publish for the control socket. `synchronized` is true
                    // only when a correction actually REACHED the clock —
                    // the same rule the refusal path above enforces: never
                    // report a synchronisation that is not happening. So a
                    // dry run reports unsynchronized however good its estimate
                    // is, because nothing is steering this clock.
                    let synchronized = steered && matches!(step.verdict, ChangeVerdict::Accepted);
                    if let Ok(mut t) = tracking.lock() {
                        t.publish(
                            synchronized,
                            step.estimate_offset_s,
                            step.estimate_freq_ppm.unwrap_or(0.0),
                            step.estimate_sd_s,
                            step.plan.next_poll_s,
                        );
                    }
                    // The LAN sees the same rule: only a correction that reached
                    // the clock lets it vouch for the time, one stratum below
                    // the source that steered it.
                    if synchronized
                        && let Some(served) = &served
                        && let Ok(peer) = sources[index].socket.peer_addr()
                        && let Ok(mut s) = served.lock()
                    {
                        s.stratum = sources[index]
                            .last_stratum
                            .saturating_add(1)
                            .min(crate::server::STRATUM_UNSYNCHRONIZED);
                        s.upstream = crate::server::Upstream {
                            leap: LeapIndicator::NoWarning,
                            reference_id: crate::refid::reference_id(peer.ip()),
                            root_delay_s: 0.0,
                            // The selected source's root distance, which already
                            // includes half its root delay and this path's
                            // uncertainty: what a client should add for us.
                            root_dispersion_s: sources[index].last_root_distance_s,
                        };
                        served_synced_at = Some(Instant::now());
                    }
                }
                None => {
                    let source = &mut sources[index];
                    source.lost += 1;
                    source.lost_in_a_row = source.lost_in_a_row.saturating_add(1);
                    let retry = ctl.retry_interval_s();
                    source.due = Instant::now() + Duration::from_secs_f64(retry);
                }
            }
        }
    }

    println!();
    for source in &sources {
        println!(
            "{}: {} exchanges, {} lost, offset {:+.9} s, freq {:+.3} ppm",
            source.name,
            source.exchanges,
            source.lost,
            source.last_offset_s,
            // One clock, one frequency: this is the loop's, not this source's.
            ctl.freq_ppm()
        );
    }
    if opts.dry_run {
        // Worth saying plainly: with the clock untouched the loop never sees
        // its own corrections, so it keeps asking for more and the frequency
        // winds to the limit. That figure is the controller straining against
        // an offset it was not allowed to fix, not a measurement of drift.
        println!(
            "(dry run: the clock was never adjusted, so the loop is open and \
             the frequency figure is not a drift measurement)"
        );
    }
    // Asked to stop before the first correction is a stop, not a failure.
    if !opts.dry_run && !applied_any && !stopped {
        eprintln!("rtimed sync: no clock command was ever applied");
        return 1;
    }
    0
}

/// Hold the clock at its current commanded rate when the sources do not agree.
///
/// With nothing selected the daemon commands nothing, so the last frequency
/// keeps running and the clock walks away from a disagreement it has already
/// detected. Refusing to steer is a decision; coasting is the absence of one.
///
/// The rate held is the loop's own `freq_ppm`, which with a shared loop simply
/// IS what the clock is running at. Under per-source controllers this had to be
/// tracked separately and got it wrong: it read the freshly polled source's
/// frequency, which could be the falseticker saturated at the -500 ppm slew
/// clamp, and a hold then installed exactly the correction selection exists to
/// reject — 53 ms of walk in two minutes on the three-server rig.
///
/// A single source always intersects itself, so this never fires on a
/// one-server deployment.
fn hold_if_no_majority(
    selected: Option<usize>,
    holding: &mut bool,
    ctl: &mut MultiController,
    driver: &mut SystemClock,
    opts: &SyncOptions,
    sources: usize,
) {
    if selected.is_some() {
        if *holding {
            eprintln!("rtimed sync: majority restored — steering again");
            *holding = false;
        }
        return;
    }
    if opts.dry_run {
        return;
    }
    if !*holding {
        eprintln!(
            "rtimed sync: no majority among {sources} sources — holding the clock at {ppm:+.3} ppm rather than coasting",
            ppm = ctl.freq_ppm(),
        );
        *holding = true;
    }
    let hold = ClockCommand::Slew {
        freq_ppm: ctl.freq_ppm(),
        drain_offset: 0.0,
        drain_rate_ppm: 0.0,
    };
    if let Err(e) = driver.apply(&hold) {
        eprintln!("rtimed sync: holding the clock: {e}");
    }
}

/// Consecutive lost exchanges after which a source stops counting towards the
/// quorum below. Without this, one dead server in a configured pair would block
/// synchronisation forever; with it, the daemon waits a few polls and then
/// proceeds on whatever is actually answering.
const UNREACHABLE_AFTER: u32 = 4;

/// Which source should drive the clock, by the same falseticker-rejecting
/// selection the plan specifies.
fn selected_index(sources: &[Source]) -> Option<usize> {
    // QUORUM. Selection is only meaningful against the sources that could
    // disagree, and during acquisition they have not all replied yet — so the
    // first source to produce an estimate is trivially its own majority. With
    // three servers configured, that lets a falseticker which answers first
    // steer the clock with its lie until the honest two arrive.
    //
    // Honest note on this guard: it did NOT change the rig outcome (15 of 16
    // seeded worlds either way), so it is not a fix for anything measured. It
    // is here as a safety property — an unverified single source should not
    // move a clock when the operator configured several precisely so that it
    // could be cross-checked — and it is kept because it costs nothing, not
    // because it bought a number.
    //
    // A single configured source is exempt: it is its own majority by
    // definition, and holding out for a quorum of one would only refuse to ever
    // synchronise.
    let answering = sources
        .iter()
        .filter(|s| s.has_estimate || s.lost_in_a_row < UNREACHABLE_AFTER)
        .count();
    let heard = sources.iter().filter(|s| s.has_estimate).count();
    if answering > 1 && heard * 2 <= answering {
        return None;
    }

    let estimates: Vec<SourceEstimate> = sources
        .iter()
        .enumerate()
        .filter(|(_, s)| s.has_estimate)
        .map(|(i, s)| SourceEstimate {
            id: i,
            offset: s.last_offset_s,
            root_distance: s.last_root_distance_s.max(1e-9),
            stratum: s.last_stratum,
        })
        .collect();
    if estimates.is_empty() {
        return None;
    }
    select(&estimates).truechimers.first().copied()
}

fn open_source(
    name: &str,
    _discipline: &DisciplineConfig,
    opts: &SyncOptions,
) -> Result<Source, String> {
    // With NTS the name is the key-establishment host, and KE says where NTP
    // actually is (RFC 8915 §4.1.7: it may delegate to another host or port).
    let nts = if opts.nts {
        Some(establish_nts(name, opts)?)
    } else {
        None
    };
    let (host, port) = match &nts {
        Some(session) => (session.ntp_server.clone(), session.ntp_port),
        None => (name.to_string(), opts.port),
    };
    let socket = connect_ntp(&host, port, opts)?;

    Ok(Source {
        name: name.to_string(),
        socket,
        nts,
        due: Instant::now(),
        last_offset_s: 0.0,
        last_root_distance_s: 1.0,
        lost_in_a_row: 0,
        last_stratum: 16,
        has_estimate: false,
        exchanges: 0,
        lost: 0,
        leap_pending: false,
    })
}

/// A UDP socket connected to `host:port`, with the read timeout and kernel
/// receive timestamps every exchange relies on.
fn connect_ntp(host: &str, port: u16, opts: &SyncOptions) -> Result<UdpSocket, String> {
    let addr: SocketAddr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolving: {e}"))?
        .next()
        .ok_or("resolved to no addresses")?;
    let bind = if addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let socket = UdpSocket::bind(bind).map_err(|e| format!("binding: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(opts.timeout_ms.max(1))))
        .map_err(|e| format!("timeout: {e}"))?;
    socket
        .connect(addr)
        .map_err(|e| format!("connecting {addr}: {e}"))?;
    // Kernel receive timestamps where the platform has them: the difference
    // between "when it arrived" and "when we were scheduled to read it".
    let _ = net::enable_rx_timestamps(&socket);
    Ok(socket)
}

/// Run NTS-KE with `host`, as `rtimed query --nts` does.
fn establish_nts(host: &str, opts: &SyncOptions) -> Result<NtsSession, String> {
    let extra_roots = match &opts.nts_ca_pem {
        Some(pem) => {
            use rusty_time_nts::tls::pki_types::{CertificateDer, pem::PemObject};
            CertificateDer::pem_slice_iter(pem.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("parsing --nts-ca: {e}"))?
        }
        None => Vec::new(),
    };
    let timeout = Duration::from_millis(opts.timeout_ms.max(1) * 5);
    let unset = !CLOCK_SET.load(std::sync::atomic::Ordering::Relaxed);
    if opts.nocerttimecheck && unset {
        NtsSession::establish_ignoring_cert_dates(host, opts.ke_port, timeout, &extra_roots)
            .map_err(|e| {
                format!(
                    "NTS key establishment (certificate dates not checked: clock not yet set): {e}"
                )
            })
    } else {
        NtsSession::establish(host, opts.ke_port, timeout, &extra_roots)
            .map_err(|e| format!("NTS key establishment: {e}"))
    }
}

/// A fresh NTS session for `source` once its cookies are spent (each request
/// spends one; a reply that fails to verify returns none). Reconnects if key
/// establishment now names a different NTP server.
fn renew_nts(source: &mut Source, opts: &SyncOptions) -> bool {
    let session = match establish_nts(&source.name, opts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rtimed sync: {}: {e}", source.name);
            return false;
        }
    };
    let same_server = source.nts.as_ref().is_some_and(|old| {
        old.ntp_server == session.ntp_server && old.ntp_port == session.ntp_port
    });
    if !same_server {
        match connect_ntp(&session.ntp_server, session.ntp_port, opts) {
            Ok(socket) => source.socket = socket,
            Err(e) => {
                eprintln!("rtimed sync: {}: {e}", source.name);
                return false;
            }
        }
    }
    source.nts = Some(session);
    true
}

/// One NTP exchange. Returns the sample plus what the server said about itself.
fn exchange(
    source: &mut Source,
    opts: &SyncOptions,
    clock: &SystemClock,
) -> Option<(Sample, u8, f64)> {
    let nonce = NtpTimestamp(nonce_value(source.exchanges));
    let header = NtpPacket::client_request(4, nonce).to_bytes();
    // NTS: the header goes out wrapped in a cookie and an authenticator. With
    // every cookie spent, key establishment runs again first.
    let protected = match source.nts.as_mut().map(|s| s.protect(&header)) {
        None => None,
        Some(Ok(packet)) => Some(packet),
        Some(Err(NtsSessionError::CookiesExhausted)) => {
            if !renew_nts(source, opts) {
                return None;
            }
            match source.nts.as_mut()?.protect(&header) {
                Ok(packet) => Some(packet),
                Err(e) => {
                    eprintln!("rtimed sync: {}: {e}", source.name);
                    return None;
                }
            }
        }
        Some(Err(e)) => {
            eprintln!("rtimed sync: {}: {e}", source.name);
            return None;
        }
    };
    let request: &[u8] = protected.as_deref().unwrap_or(&header);

    // Kept as INTEGER nanoseconds, not seconds-as-f64.
    //
    // Unix time is about 1.79e9 seconds now, and an f64 there has a 238 ns
    // gap between representable values — so converting a timestamp to seconds
    // rounds away 238 ns before any arithmetic happens, and a difference of
    // two such values can be off by 477 ns. The wire carries 2^-32 s, which is
    // 0.233 ns: three orders of magnitude finer than what was being kept.
    //
    // That already costs a third of the measured error budget, and it gets
    // worse on a schedule. **In February 2038 Unix time crosses 2^31**, the
    // exponent steps, and the gap doubles to 477 ns — the error in a difference
    // becoming 954 ns, comparable to the entire steady-state error. Nothing
    // would break loudly; the daemon would simply get less accurate, on a date.
    let t1_ns = clock.wall_ns().ok()?;
    let t1_mono = clock.mono_s().ok()?;
    source.socket.send(request).ok()?;

    let deadline = Instant::now() + Duration::from_millis(opts.timeout_ms.max(1));
    // An NTS reply fits: it refills only the cookies spent since the last
    // full store, normally the one this request used.
    let mut bufs = [[0u8; 1024]; 4];
    let mut received = Vec::with_capacity(4);
    let mut scratch = net::BatchScratch::new();
    // `recv_batch` is used rather than `recv` because the socket has receive
    // timestamping enabled, and a timestamp arrives as control data on a
    // `recvmsg` — a plain `recv` supplies no control buffer and silently
    // discards it. (clknetsim asserts on exactly that mismatch, which is how
    // this was caught.)
    let (packet, t4_ns, t4_mono) = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match net::wait_readable(&source.socket, remaining) {
            Ok(true) => {}
            _ => return None,
        }
        let count = match net::recv_batch(&source.socket, &mut bufs, &mut scratch, &mut received) {
            Ok(n) => n,
            Err(_) => return None,
        };
        // Read the local clock once, right after the syscall, for any datagram
        // the kernel did not stamp itself.
        let userspace_wall_ns = clock.wall_ns().ok()?;
        let mono = clock.mono_s().ok()?;
        let mut found = None;
        for (index, message) in received.iter().enumerate().take(count) {
            if message.len < HEADER_LEN {
                continue;
            }
            match NtpPacket::parse(&bufs[index][..message.len]) {
                Ok(p) if p.origin_ts == nonce && p.mode == Mode::Server => {
                    // Under NTS a reply counts only if it verifies against the
                    // request in flight; that also banks its fresh cookies. An
                    // unauthenticated one is dropped, and the exchange waits on
                    // for the real reply.
                    if let Some(session) = source.nts.as_mut()
                        && session.verify(&bufs[index][..message.len]).is_err()
                    {
                        // Except an NTS NAK answering this very request (RFC
                        // 8915 §5.7, kiss code NTSN): the server cannot redeem
                        // our cookies any more, so renew at the next poll
                        // instead of losing one exchange per cookie left, as
                        // chronyd does. Forging one needs the request's origin
                        // timestamp, i.e. an on-path attacker, who could drop
                        // our packets anyway.
                        if p.stratum == 0 && p.reference_id == *b"NTSN" {
                            eprintln!(
                                "rtimed sync: {}: NTS NAK; key establishment at the next poll",
                                source.name
                            );
                            session.forget_cookies();
                            return None;
                        }
                        continue;
                    }
                    // The kernel stamp is taken when the packet reached the
                    // stack; the userspace read is taken after we were
                    // scheduled. Prefer the former — the difference is
                    // scheduling latency, and it lands straight in the offset.
                    let t4_ns: i128 = message
                        .kernel_rx_ns
                        .map(i128::from)
                        .unwrap_or(userspace_wall_ns);
                    // Scheduling latency, in seconds, for the monotonic stamp.
                    let latency = ((userspace_wall_ns - t4_ns) as f64 * 1e-9).max(0.0);
                    found = Some((p, t4_ns, mono - latency));
                    break;
                }
                _ => continue,
            }
        }
        if let Some(hit) = found {
            break hit;
        }
    };

    // Refuse anything that is not a usable time source before its numbers can
    // influence the clock.
    if packet.stratum == 0 || packet.stratum > 15 || packet.leap == LeapIndicator::Unsynchronized {
        return None;
    }
    if packet.transmit_ts.is_zero() || packet.receive_ts.is_zero() {
        return None;
    }

    // All four timestamps RELATIVE to T1, so the magnitudes are milliseconds
    // rather than decades and every bit of the wire's precision survives.
    //
    // `seconds_since` takes the difference in the 32.32 fixed-point domain — an
    // exact integer subtraction — and only then divides. The result is a small
    // number, which f64 represents to well under a nanosecond.
    //
    // It also removes the era guess. Picking an era by proximity to the local
    // clock is only as good as that clock; a difference under ±68 years is
    // unambiguous by RFC 5905's own arithmetic, and every difference here is
    // milliseconds.
    let t1_ntp = NtpTimestamp::from_unix(
        t1_ns.div_euclid(1_000_000_000) as i64,
        t1_ns.rem_euclid(1_000_000_000) as u32,
    );
    let t1 = 0.0;
    let t2 = packet.receive_ts.seconds_since(t1_ntp);
    let t3 = packet.transmit_ts.seconds_since(t1_ntp);
    let t4 = (t4_ns - t1_ns) as f64 * 1e-9;
    let (offset, delay) = ntp::offset_delay(t1, t2, t3, t4);
    if delay < 0.0 {
        return None;
    }

    source.leap_pending = matches!(
        packet.leap,
        LeapIndicator::LastMinute61 | LeapIndicator::LastMinute59
    );

    let dispersion = packet.root_dispersion.to_seconds();
    let root_distance = packet.root_delay.to_seconds() / 2.0 + dispersion + delay / 2.0;
    Some((
        Sample {
            t: (t1_mono + t4_mono) / 2.0,
            offset,
            delay,
            dispersion,
        },
        packet.stratum,
        root_distance,
    ))
}

fn nonce_value(counter: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut h = RandomState::new().build_hasher();
    h.write_u64(counter);
    h.write_u128(
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_require_a_server() {
        assert!(SyncOptions::parse(&[]).is_err());
        assert!(SyncOptions::parse(&["--dry-run".into()]).is_err());
        let opts = SyncOptions::parse(&["pool.ntp.org".into(), "--dry-run".into()]).expect("parse");
        assert_eq!(opts.servers, vec!["pool.ntp.org".to_string()]);
        assert!(opts.dry_run);
    }

    #[test]
    fn discipline_knobs_are_configurable_for_a_fair_comparison() {
        // The corpus must be able to give both implementations the same
        // policy, or the comparison measures configuration, not code.
        let opts = SyncOptions::parse(&[
            "s".into(),
            "--minpoll".into(),
            "2".into(),
            "--maxpoll".into(),
            "6".into(),
            "--makestep".into(),
            "1.0".into(),
            "3".into(),
        ])
        .expect("parse");
        assert_eq!(opts.discipline.min_poll, 2);
        assert_eq!(opts.discipline.max_poll, 6);
        assert_eq!(opts.discipline.makestep_threshold, Some(1.0));
        assert_eq!(opts.discipline.makestep_limit, 3);
    }

    /// A source with a given estimate, for the selection tests. The socket is
    /// bound but never used: selection reads only the last estimate.
    fn stub(offset_s: f64, root_distance_s: f64) -> Source {
        Source {
            name: "stub".into(),
            socket: UdpSocket::bind("127.0.0.1:0").expect("bind"),
            nts: None,
            due: Instant::now(),
            last_offset_s: offset_s,
            last_root_distance_s: root_distance_s,
            last_stratum: 1,
            has_estimate: true,
            exchanges: 1,
            lost: 0,
            lost_in_a_row: 0,
            leap_pending: false,
        }
    }

    #[test]
    fn one_source_is_always_selected() {
        // The hold-when-nothing-is-selected path must never fire on a
        // single-server deployment, which is the common case, and the quorum
        // must not stall it either. A lone source is its own majority however
        // wide or narrow its interval is.
        for rd in [1e-9, 1e-6, 1.0, 1e6] {
            assert_eq!(
                selected_index(&[stub(0.123, rd)]),
                Some(0),
                "a single source with root distance {rd} was not selected"
            );
        }
    }

    #[test]
    fn a_falseticker_is_rejected_and_two_honest_sources_still_agree() {
        // Two good sources 4 ms apart and one liar 5 s out: the shape the
        // three-server rig produces. The honest pair must form the majority.
        let sources = [
            stub(0.011, 0.05),
            stub(0.007, 0.05),
            stub(5.009, 0.05), // the falseticker
        ];
        let picked = selected_index(&sources).expect("a majority of two exists");
        assert!(
            picked < 2,
            "selection chose the falseticker (index {picked})"
        );
    }

    #[test]
    fn intervals_too_narrow_to_overlap_elect_nobody() {
        // The defect the root-distance fix addresses, pinned from the other
        // side: with the estimate's own uncertainty omitted, two HEALTHY
        // sources a few ms apart have intervals ~100 us wide that cannot
        // intersect, and a set of perfectly good servers elects nobody.
        let too_narrow = [stub(0.011, 50e-6), stub(0.007, 50e-6), stub(5.009, 50e-6)];
        assert_eq!(
            selected_index(&too_narrow),
            None,
            "three mutually disjoint intervals must not produce a majority"
        );
        // Widening each interval to cover the real uncertainty in the estimate
        // — which is what adding estimate_sd_s does — recovers the majority
        // from exactly the same offsets.
        let honest = [
            stub(0.011, 50e-6 + 4e-3),
            stub(0.007, 50e-6 + 4e-3),
            stub(5.009, 50e-6 + 4e-3),
        ];
        assert!(matches!(selected_index(&honest), Some(i) if i < 2));
    }

    #[test]
    fn a_lone_early_reply_cannot_steer_a_three_server_clock() {
        // Acquisition: only the falseticker has answered so far. It must not
        // be its own majority, or a five-second lie drives the clock until the
        // honest servers arrive.
        let mut sources = [stub(5.0, 0.05), stub(0.0, 0.05), stub(0.0, 0.05)];
        sources[1].has_estimate = false;
        sources[2].has_estimate = false;
        assert_eq!(
            selected_index(&sources),
            None,
            "one source out of three configured must not reach quorum"
        );
        // A second reply reaches the quorum but not agreement: one liar and one
        // honest server is a 1-1 split, and no majority exists to be had.
        sources[1].has_estimate = true;
        assert_eq!(
            selected_index(&sources),
            None,
            "a liar and an honest server disagree; neither is a majority"
        );
        // The third reply breaks the tie, and the honest pair takes it.
        sources[2].has_estimate = true;
        let picked = selected_index(&sources).expect("two honest servers agree");
        assert!(picked > 0, "selection chose the falseticker");
    }

    #[test]
    fn a_source_that_stopped_answering_stops_blocking_the_quorum() {
        // The other side of the quorum: two configured servers, one of them
        // dead. Once it has missed enough polls it leaves the denominator, so
        // the survivor can discipline the clock instead of the daemon waiting
        // for a quorum that will never form.
        let mut sources = [stub(0.001, 0.05), stub(0.0, 0.05)];
        sources[1].has_estimate = false;
        sources[1].lost_in_a_row = 1;
        assert_eq!(
            selected_index(&sources),
            None,
            "while the second server may still answer, one of two is not a majority"
        );
        sources[1].lost_in_a_row = UNREACHABLE_AFTER;
        assert_eq!(
            selected_index(&sources),
            Some(0),
            "an unreachable source must not block the one that is answering"
        );
    }

    #[test]
    fn a_negative_makestep_limit_means_always() {
        let opts =
            SyncOptions::parse(&["s".into(), "--makestep".into(), "0.1".into(), "-1".into()])
                .expect("parse");
        assert_eq!(opts.discipline.makestep_limit, u32::MAX);
    }
}
