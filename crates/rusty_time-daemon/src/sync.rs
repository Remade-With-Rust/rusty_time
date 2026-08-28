//! `rtimed sync` — the resident client daemon.
//!
//! Polls its sources, runs the discipline loop, and applies the result to the
//! system clock. This is the chronyd-equivalent mode, and it is what the
//! TIMECORP cross-implementation arm measures.
//!
//! The discipline itself is `rusty_time_core::client::SyncController`, which
//! is also what the simulator drives. That shared type is the reason a corpus
//! number here means anything: the alternative — a simulator with its own copy
//! of the loop — measures code that never ships.

use rusty_time_clock::{ClockDrive, ClockRead, SystemClock, net};
use rusty_time_core::client::SyncController;
use rusty_time_core::discipline::ChangeVerdict;
use rusty_time_core::ntp::{self, HEADER_LEN, LeapIndicator, Mode, NtpPacket, NtpTimestamp};
use rusty_time_core::select::select;
use rusty_time_core::{ClockCommand, DisciplineConfig, LeapMode, Sample, SourceEstimate};
use std::net::{ToSocketAddrs, UdpSocket};
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
}

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
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = || -> Result<String, String> {
                it.next().cloned().ok_or(format!("{arg} needs a value"))
            };
            match arg.as_str() {
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
    controller: SyncController,
    /// Monotonic instant of the next due poll.
    due: Instant,
    /// Latest estimate, for selection across sources.
    last_offset_s: f64,
    last_root_distance_s: f64,
    last_stratum: u8,
    has_estimate: bool,
    exchanges: u64,
    lost: u64,
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

    // Never plan a slew the driver cannot deliver. The discipline's bookkeeping
    // assumes the rate it asked for is the rate that ran; if the platform
    // silently clamps, the controller subtracts a correction that never
    // happened and the regression reads the shortfall as a frequency error.
    let mut discipline = opts.discipline;
    discipline.max_slew_ppm = discipline.max_slew_ppm.min(caps.max_slew_ppm);

    let mut sources = Vec::new();
    for name in &opts.servers {
        match open_source(name, &discipline, opts) {
            Ok(source) => sources.push(source),
            Err(e) => eprintln!("rtimed sync: {name}: {e}"),
        }
    }
    if sources.is_empty() {
        eprintln!("rtimed sync: no usable sources");
        return 1;
    }

    // Measured on the same clock the samples use, so "how long until the first
    // exchange" is answerable from the log rather than inferred.
    let mono_start = clock.mono_s().unwrap_or(0.0);
    println!(
        "rtimed: syncing from {} source(s){} (startup at mono {:.3})",
        sources.len(),
        if opts.dry_run { " (dry run)" } else { "" },
        mono_start
    );

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

    let mut drains_retired: u64 = 0;
    let started = Instant::now();
    let mut driver = SystemClock;
    let mut applied_any = false;

    loop {
        if opts.run_seconds > 0 && started.elapsed().as_secs() >= opts.run_seconds {
            break;
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
        // Only the SELECTED source may write to the clock, exactly as on the
        // sample path. Every source runs its own controller and therefore its
        // own drain, so retiring them all and applying each one's command would
        // let a source the selector rejected — a falseticker, or simply the
        // worse of two — impose its frequency on the clock the moment its drain
        // happened to expire. A single-source benchmark cannot see this, which
        // is why it is worth stating rather than assuming.
        //
        // Their drains are still retired, because that is bookkeeping each
        // controller owes itself; only the command is withheld.
        let mono_now = clock.mono_s().unwrap_or(0.0);
        let driving = selected_index(&sources);
        if stop_drains {
            for (index, source) in sources.iter_mut().enumerate() {
                let Some(command) = source.controller.poll_drain(mono_now) else {
                    continue;
                };
                if driving != Some(index) {
                    continue; // retired in its own books; it does not drive the clock
                }
                drains_retired += 1;
                if opts.verbose {
                    println!(
                        "t={:8.3} drain retired (#{drains_retired}) freq={:+.3}",
                        mono_now - mono_start,
                        source.controller.freq_ppm()
                    );
                }
                if !opts.dry_run
                    && let Err(e) = driver.apply(&command)
                {
                    // Retiring a drain only ever asks the clock to STOP
                    // draining, so a refusal leaves it running faster than the
                    // books say. Count it with the rest: the condition it
                    // signals is the same one.
                    refused_in_a_row += 1;
                    eprintln!("rtimed sync: ending drain: {e} (refused {refused_in_a_row}x)");
                }
            }
        }

        // Wait until the earliest source is due, or until a drain runs out —
        // whichever comes first.
        let now = Instant::now();
        let next_due = sources.iter().map(|s| s.due).min().unwrap_or(now);
        let until_due = next_due.saturating_duration_since(now);
        let until_drain = sources
            .iter()
            .filter(|_| stop_drains)
            .filter_map(|s| s.controller.drain_completes_at())
            .map(|at| at - mono_now)
            .filter(|remaining| *remaining > 0.0)
            .fold(f64::INFINITY, f64::min);
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
                    let source = &mut sources[index];
                    // `exchange` has just recorded whether this reply carried a
                    // leap announcement, so the discipline can treat the second
                    // as expected rather than as a source misbehaving.
                    let leap_pending = source.leap_pending;
                    let step =
                        source
                            .controller
                            .on_sample_with_leap(mono_now, sample, leap_pending);
                    source.exchanges += 1;
                    source.last_offset_s = step.estimate_offset_s;
                    source.last_root_distance_s = root_distance;
                    source.last_stratum = stratum;
                    source.has_estimate = true;
                    source.due = Instant::now() + Duration::from_secs_f64(step.plan.next_poll_s);

                    if opts.verbose {
                        println!(
                            "t={:8.3} sample source={} offset={:+.9} freq={:+.3} \
                             samples={} poll={}",
                            mono_now - mono_start,
                            source.name,
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

                    // Only the selected source drives the clock.
                    if selected_index(&sources) == Some(index) && !opts.dry_run {
                        match driver.apply(&step.plan.command) {
                            Ok(()) => {
                                applied_any = true;
                                refused_in_a_row = 0;
                                sources[index].controller.confirm_last_plan();
                            }
                            Err(e) => {
                                // The command did not reach the clock, so the
                                // books that assumed it did must be put back.
                                // Left standing, the register would carry a
                                // correction that never happened and the
                                // regression would read it as truth — the
                                // daemon would report itself synchronised while
                                // the clock ran free.
                                sources[index].controller.revert_last_plan();
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
                    } else {
                        // NOT driving the clock, so this plan never reached it.
                        //
                        // Confirming here is wrong in principle — it cements a
                        // correction that did not happen — and reverting it,
                        // which is what principle says, measured WORSE on the
                        // multi-source rig every time it was tried. The
                        // interaction is not understood, so the behaviour is
                        // left exactly as it shipped rather than changed on a
                        // theory the measurements contradict. See the ledger:
                        // multi-source selection is a known open defect and
                        // `tools/corpus/multisource.sh` reproduces it.
                        sources[index].controller.confirm_last_plan();
                    }
                }
                None => {
                    let source = &mut sources[index];
                    source.lost += 1;
                    let retry = source.controller.retry_interval_s();
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
            source.controller.freq_ppm()
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
    if !opts.dry_run && !applied_any {
        eprintln!("rtimed sync: no clock command was ever applied");
        return 1;
    }
    0
}

/// Which source should drive the clock, by the same falseticker-rejecting
/// selection the plan specifies.
fn selected_index(sources: &[Source]) -> Option<usize> {
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
    discipline: &DisciplineConfig,
    opts: &SyncOptions,
) -> Result<Source, String> {
    let addr = (name, opts.port)
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

    Ok(Source {
        name: name.to_string(),
        socket,
        controller: SyncController::new(*discipline),
        due: Instant::now(),
        last_offset_s: 0.0,
        last_root_distance_s: 1.0,
        last_stratum: 16,
        has_estimate: false,
        exchanges: 0,
        lost: 0,
        leap_pending: false,
    })
}

/// One NTP exchange. Returns the sample plus what the server said about itself.
fn exchange(
    source: &mut Source,
    opts: &SyncOptions,
    clock: &SystemClock,
) -> Option<(Sample, u8, f64)> {
    let nonce = NtpTimestamp(nonce_value(source.exchanges));
    let request = NtpPacket::client_request(4, nonce).to_bytes();

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
    source.socket.send(&request).ok()?;

    let deadline = Instant::now() + Duration::from_millis(opts.timeout_ms.max(1));
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

    #[test]
    fn a_negative_makestep_limit_means_always() {
        let opts =
            SyncOptions::parse(&["s".into(), "--makestep".into(), "0.1".into(), "-1".into()])
                .expect("parse");
        assert_eq!(opts.discipline.makestep_limit, u32::MAX);
    }
}
