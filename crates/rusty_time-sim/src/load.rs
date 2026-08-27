//! `timecorp load` — a real NTP load generator, for the G4 server arm.
//!
//! G4 asks whether our server answers more requests per second than chrony's
//! on the same rig, at no worse p99. That needs load a real server can feel,
//! from outside the process, over a real socket — the S12 scenarios drive the
//! admission *policy* in-process and deliberately do not touch a socket.
//!
//! Two things are measured, and the distinction matters:
//!
//! * **Replies per second** is the gate's own unit, and it is a wall-clock
//!   number on a shared machine, so it drifts.
//! * **Server CPU microseconds per reply** is what actually changed when the
//!   server got cheaper, and it barely drifts at all — CPU time does not
//!   accrue while the process is descheduled. The harness reads it from
//!   `/proc/<pid>/stat` around the run and reports both.
//!
//! Requests are genuine NTPv4 client packets built by the shipping codec, so a
//! server that answers them is doing exactly the work it does in production.

use rusty_time_core::ntp::{self, HEADER_LEN, LeapIndicator, Mode, NtpPacket, NtpTimestamp};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

pub struct LoadOptions {
    pub target: String,
    pub requests: usize,
    /// Requests allowed outstanding at once. The server is asked for a queue,
    /// not a ping-pong: a depth of one measures round-trip latency, not
    /// throughput.
    pub concurrency: usize,
    pub timeout_ms: u64,
    /// Report percentiles over this many latency samples.
    pub sample_every: usize,
}

impl LoadOptions {
    pub fn parse(args: &[String]) -> Result<LoadOptions, String> {
        let mut opts = LoadOptions {
            target: "127.0.0.1:11123".into(),
            requests: 200_000,
            concurrency: 64,
            timeout_ms: 2_000,
            sample_every: 16,
        };
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let mut value = || -> Result<String, String> {
                it.next().cloned().ok_or(format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--target" => opts.target = value()?,
                "--requests" => {
                    opts.requests = value()?.parse().map_err(|_| "--requests: not a number")?;
                }
                "--concurrency" => {
                    opts.concurrency = value()?
                        .parse()
                        .map_err(|_| "--concurrency: not a number")?;
                }
                "--timeout-ms" => {
                    opts.timeout_ms = value()?.parse().map_err(|_| "--timeout-ms: not a number")?;
                }
                other => return Err(format!("unknown flag '{other}'")),
            }
        }
        if opts.concurrency == 0 {
            return Err("--concurrency must be at least 1".into());
        }
        Ok(opts)
    }
}

fn request_bytes(nonce: u64) -> [u8; HEADER_LEN] {
    NtpPacket {
        leap: LeapIndicator::NoWarning,
        version: 4,
        mode: Mode::Client,
        stratum: 0,
        poll: 6,
        precision: -20,
        root_delay: ntp::NtpShort(0),
        root_dispersion: ntp::NtpShort(0),
        reference_id: [0; 4],
        reference_ts: NtpTimestamp::ZERO,
        origin_ts: NtpTimestamp::ZERO,
        receive_ts: NtpTimestamp::ZERO,
        // The server echoes this in the origin field, so it is how a reply is
        // matched to its request.
        transmit_ts: NtpTimestamp(nonce),
    }
    .to_bytes()
}

/// Ticks per second in `/proc/<pid>/stat`.
///
/// Fixed at 100 by Linux's userspace ABI — `USER_HZ` is 100 whatever the
/// kernel's internal `CONFIG_HZ` is, precisely so that `/proc` fields have a
/// stable meaning. Written as a constant rather than a `sysconf` call because
/// the workspace denies `unsafe` outside the platform seam, and this is a fact
/// about the interface rather than something to interrogate at runtime.
#[cfg(target_os = "linux")]
const PROC_TICKS_PER_S: f64 = 100.0;

/// Server CPU time (user + system) in seconds, read from /proc.
#[cfg(target_os = "linux")]
fn process_cpu_s(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field is parenthesised and may itself contain spaces, so split
    // after its closing paren rather than counting fields from the start.
    let tail = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    // Overall fields 14 and 15 (utime, stime); indices 11 and 12 after the split.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) as f64 / PROC_TICKS_PER_S)
}

#[cfg(not(target_os = "linux"))]
fn process_cpu_s(_pid: u32) -> Option<f64> {
    None
}

pub fn run(opts: &LoadOptions) -> i32 {
    let addr: SocketAddr = match opts
        .target
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
    {
        Some(a) => a,
        None => {
            eprintln!("timecorp load: cannot resolve {}", opts.target);
            return 1;
        }
    };
    let socket = match UdpSocket::bind(if addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("timecorp load: bind: {e}");
            return 1;
        }
    };
    if socket.connect(addr).is_err() || socket.set_nonblocking(true).is_err() {
        eprintln!("timecorp load: cannot connect to {addr}");
        return 1;
    }

    // Server CPU before the run, if the target is a local process we can find.
    let server_pid: Option<u32> = std::env::var("SERVER_PID")
        .ok()
        .and_then(|v| v.parse().ok());
    let cpu_before = server_pid.and_then(process_cpu_s);

    let mut sent = 0usize;
    let mut received = 0usize;
    let mut in_flight = 0usize;
    let mut latencies: Vec<f64> = Vec::with_capacity(opts.requests / opts.sample_every + 1);
    // Send timestamps for the sampled requests only, so bookkeeping does not
    // become the thing being measured.
    let mut pending: std::collections::HashMap<u64, Instant> = std::collections::HashMap::new();

    let mut buf = [0u8; 1024];
    let started = Instant::now();
    let deadline = started + Duration::from_millis(opts.timeout_ms) + Duration::from_secs(60);

    while received < opts.requests && Instant::now() < deadline {
        // Top up the window.
        while in_flight < opts.concurrency && sent < opts.requests {
            let nonce = 0x9e37_79b9_7f4a_7c15u64.wrapping_mul(sent as u64 + 1);
            let packet = request_bytes(nonce);
            match socket.send(&packet) {
                Ok(_) => {
                    if sent.is_multiple_of(opts.sample_every) {
                        pending.insert(nonce, Instant::now());
                    }
                    sent += 1;
                    in_flight += 1;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // Drain whatever came back.
        let mut drained = 0;
        loop {
            match socket.recv(&mut buf) {
                Ok(n) if n >= HEADER_LEN => {
                    received += 1;
                    in_flight = in_flight.saturating_sub(1);
                    drained += 1;
                    if let Ok(p) = NtpPacket::parse(&buf[..n])
                        && let Some(at) = pending.remove(&p.origin_ts.0)
                    {
                        latencies.push(at.elapsed().as_secs_f64());
                    }
                }
                Ok(_) => {
                    received += 1;
                    in_flight = in_flight.saturating_sub(1);
                    drained += 1;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        // Nothing moving and nothing left to send: the server has stopped
        // answering. Give outstanding replies a moment, then stop.
        if drained == 0 && sent >= opts.requests {
            std::thread::sleep(Duration::from_millis(2));
            if Instant::now() > started + Duration::from_millis(opts.timeout_ms) && in_flight > 0 {
                break;
            }
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    let cpu_after = server_pid.and_then(process_cpu_s);
    latencies.sort_by(f64::total_cmp);
    let pick = |q: f64| -> f64 {
        if latencies.is_empty() {
            return f64::NAN;
        }
        latencies[((latencies.len() - 1) as f64 * q) as usize]
    };

    println!("sent       {sent}");
    println!("answered   {received}");
    println!("lost       {}", sent.saturating_sub(received));
    println!("seconds    {elapsed:.3}");
    println!("replies_s  {:.0}", received as f64 / elapsed.max(1e-9));
    println!("p50_us     {:.1}", pick(0.50) * 1e6);
    println!("p99_us     {:.1}", pick(0.99) * 1e6);
    if let (Some(a), Some(b)) = (cpu_before, cpu_after) {
        let cpu = (b - a).max(0.0);
        println!("server_cpu_s   {cpu:.3}");
        println!("cpu_us_reply   {:.3}", cpu * 1e6 / (received.max(1) as f64));
    }
    0
}
