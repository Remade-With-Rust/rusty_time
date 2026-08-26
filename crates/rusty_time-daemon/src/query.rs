//! One-shot SNTP measurement: N exchanges, filtered, reported.

use rusty_time_api::{QueryReport, SampleReport};
use rusty_time_clock::{ClockRead, SystemClock};
use rusty_time_core::ntp::{
    self, HEADER_LEN, LeapIndicator, Mode, NtpPacket, NtpTimestamp, UNIX_EPOCH_OFFSET,
};
use rusty_time_core::{Sample, SampleRegister};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

pub struct Options {
    pub server: String,
    pub count: u32,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub port: u16,
    pub json: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Options, String> {
        let mut it = args.iter();
        let server = it
            .next()
            .filter(|s| !s.starts_with("--"))
            .ok_or("a server name is required")?
            .clone();
        let mut opts = Options {
            server,
            count: 4,
            interval_ms: 1000,
            timeout_ms: 2000,
            port: 123,
            json: false,
        };
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--json" => opts.json = true,
                "--count" => opts.count = num(flag, it.next())?,
                "--interval-ms" => opts.interval_ms = num(flag, it.next())?,
                "--timeout-ms" => opts.timeout_ms = num(flag, it.next())?,
                "--port" => opts.port = num(flag, it.next())?,
                other => return Err(format!("unknown flag '{other}'")),
            }
        }
        if opts.count == 0 || opts.count > 64 {
            return Err("--count must be 1..=64".into());
        }
        Ok(opts)
    }
}

fn num<T: std::str::FromStr>(flag: &str, value: Option<&String>) -> Result<T, String> {
    value
        .ok_or(format!("{flag} needs a value"))?
        .parse()
        .map_err(|_| format!("{flag}: cannot parse value"))
}

/// A 64-bit nonce from the OS-seeded SipHash state: unpredictable to an
/// off-path spoofer, which is the property RFC 5905 wants from the client
/// transmit timestamp.
fn nonce(counter: u64) -> u64 {
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

/// Resolve an NTP timestamp into Unix seconds near a pivot (era disambiguation).
fn ntp_to_unix_near(ts: NtpTimestamp, pivot_unix_s: f64) -> f64 {
    let era_span = 4_294_967_296.0_f64;
    let base = ts.seconds() as f64 - UNIX_EPOCH_OFFSET as f64 + ts.fraction() as f64 / era_span;
    let mut best = base;
    let mut best_dist = (base - pivot_unix_s).abs();
    for k in [-1.0f64, 1.0] {
        let cand = base + k * era_span;
        let d = (cand - pivot_unix_s).abs();
        if d < best_dist {
            best = cand;
            best_dist = d;
        }
    }
    best
}

fn leap_str(leap: LeapIndicator) -> &'static str {
    match leap {
        LeapIndicator::NoWarning => "no-warning",
        LeapIndicator::LastMinute61 => "last-minute-61",
        LeapIndicator::LastMinute59 => "last-minute-59",
        LeapIndicator::Unsynchronized => "unsynchronized",
    }
}

fn refid_str(stratum: u8, id: [u8; 4]) -> String {
    if stratum == 1 {
        let text: String = id
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
            .collect();
        if !text.is_empty() {
            return text;
        }
    }
    format!("{:02x}{:02x}{:02x}{:02x}", id[0], id[1], id[2], id[3])
}

pub fn run(opts: &Options) -> i32 {
    match measure(opts) {
        Ok(report) => {
            if opts.json {
                match serde_json::to_string_pretty(&report) {
                    Ok(s) => println!("{s}"),
                    Err(e) => {
                        eprintln!("rtimed query: serializing report: {e}");
                        return 1;
                    }
                }
            } else {
                print_human(&report);
            }
            if report.received == 0 { 1 } else { 0 }
        }
        Err(msg) => {
            eprintln!("rtimed query: {msg}");
            1
        }
    }
}

fn measure(opts: &Options) -> Result<QueryReport, String> {
    let clock = SystemClock;
    let addr: SocketAddr = (opts.server.as_str(), opts.port)
        .to_socket_addrs()
        .map_err(|e| format!("resolving {}: {e}", opts.server))?
        .next()
        .ok_or_else(|| format!("{} resolved to no addresses", opts.server))?;

    let bind_addr = if addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let socket = UdpSocket::bind(bind_addr).map_err(|e| format!("binding socket: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(opts.timeout_ms.max(1))))
        .map_err(|e| format!("setting timeout: {e}"))?;
    socket
        .connect(addr)
        .map_err(|e| format!("connecting {addr}: {e}"))?;

    let mut register = SampleRegister::new(64);
    let mut report = QueryReport {
        server: opts.server.clone(),
        address: addr.to_string(),
        sent: 0,
        received: 0,
        samples: Vec::new(),
        best_offset_s: None,
        best_delay_s: None,
        regress_offset_s: None,
        regress_freq_ppm: None,
        regress_sd_s: None,
        reference_id: String::new(),
        leap: String::new(),
    };

    let mut buf = [0u8; 1024];
    for i in 0..opts.count {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(opts.interval_ms));
        }
        let tx_nonce = NtpTimestamp(nonce(i as u64));
        let request = NtpPacket::client_request(4, tx_nonce).to_bytes();

        let t1_wall = clock.wall_ns().map_err(|e| e.to_string())? as f64 * 1e-9;
        let t1_mono = clock.mono_s().map_err(|e| e.to_string())?;
        if socket.send(&request).is_err() {
            continue;
        }
        report.sent += 1;

        // Read until our answer or timeout: a stray datagram must not abort the run.
        let response = loop {
            match socket.recv(&mut buf) {
                Ok(n) if n >= HEADER_LEN => {
                    match NtpPacket::parse(&buf[..n]) {
                        Ok(p) if p.origin_ts == tx_nonce && p.mode == Mode::Server => {
                            break Some(p);
                        }
                        Ok(_) => continue, // not ours / not a server answer
                        Err(_) => continue,
                    }
                }
                Ok(_) => continue,
                Err(_) => break None, // timeout
            }
        };
        let Some(packet) = response else { continue };
        let t4_wall = clock.wall_ns().map_err(|e| e.to_string())? as f64 * 1e-9;
        let t4_mono = clock.mono_s().map_err(|e| e.to_string())?;

        // Sanity gates before the sample is allowed to influence anything.
        if packet.stratum == 0 || packet.stratum > 15 {
            continue; // Kiss-o'-Death or unsynchronized
        }
        if packet.leap == LeapIndicator::Unsynchronized {
            continue;
        }
        if packet.transmit_ts.is_zero() || packet.receive_ts.is_zero() {
            continue;
        }

        let t2 = ntp_to_unix_near(packet.receive_ts, t1_wall);
        let t3 = ntp_to_unix_near(packet.transmit_ts, t4_wall);
        let (offset, delay) = ntp::offset_delay(t1_wall, t2, t3, t4_wall);
        if delay < 0.0 {
            continue; // non-causal: asymmetric era resolution or broken server
        }

        report.received += 1;
        report.reference_id = refid_str(packet.stratum, packet.reference_id);
        report.leap = leap_str(packet.leap).to_string();
        report.samples.push(SampleReport {
            offset_s: offset,
            delay_s: delay,
            stratum: packet.stratum,
            root_delay_s: packet.root_delay.to_seconds(),
            root_dispersion_s: packet.root_dispersion.to_seconds(),
        });
        register.push(Sample {
            t: (t1_mono + t4_mono) / 2.0,
            offset,
            delay,
            dispersion: packet.root_dispersion.to_seconds(),
        });
    }

    if let Some(best) = register.best() {
        report.best_offset_s = Some(best.offset);
        report.best_delay_s = Some(best.delay);
    }
    let now_mono = clock.mono_s().map_err(|e| e.to_string())?;
    if let Some(est) = register.regress(now_mono) {
        report.regress_offset_s = Some(est.offset);
        report.regress_freq_ppm = est.freq_ppm;
        report.regress_sd_s = Some(est.offset_sd);
    }
    Ok(report)
}

fn print_human(r: &QueryReport) {
    println!("server    : {} ({})", r.server, r.address);
    println!("exchanges : {}/{} answered", r.received, r.sent);
    if r.received > 0 {
        println!(
            "stratum   : {}  refid {}  leap {}",
            r.samples[0].stratum, r.reference_id, r.leap
        );
        println!();
        println!("  {:>4} {:>14} {:>12}", "#", "offset", "delay");
        for (i, s) in r.samples.iter().enumerate() {
            println!("  {:>4} {:>+14.6} {:>12.6}", i + 1, s.offset_s, s.delay_s);
        }
        println!();
    }
    match (r.best_offset_s, r.best_delay_s) {
        (Some(o), Some(d)) => {
            println!(
                "best      : offset {:+.6} s  (delay {:.6} s, min-delay sample)",
                o, d
            )
        }
        _ => println!("best      : no valid samples"),
    }
    if let Some(o) = r.regress_offset_s {
        let sd = r.regress_sd_s.unwrap_or(0.0);
        print!("regression: offset {o:+.6} s  sd {sd:.6} s");
        if let Some(f) = r.regress_freq_ppm {
            print!("  freq {f:+.3} ppm");
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn era_pivot_resolves_current_times() {
        let now_unix = 1_756_224_000.0; // 2026-08-26
        let ts = NtpTimestamp::from_unix(1_756_224_000, 250_000_000);
        let back = ntp_to_unix_near(ts, now_unix);
        assert!((back - 1_756_224_000.25).abs() < 1e-6, "{back}");
    }

    #[test]
    fn era_pivot_resolves_post_2036_times() {
        // NTP era 0 ends Feb 2036; a timestamp 100 s into era 1 must resolve
        // near a post-2036 pivot, not 136 years in the past.
        let era1_start_unix = 4_294_967_296.0 - UNIX_EPOCH_OFFSET as f64;
        let pivot = era1_start_unix + 50.0;
        let ts = NtpTimestamp::from_parts(100, 0); // era-1 seconds wrap to 100
        let back = ntp_to_unix_near(ts, pivot);
        assert!((back - (era1_start_unix + 100.0)).abs() < 1e-6, "{back}");
    }

    #[test]
    fn options_parse_and_validate() {
        let opts = Options::parse(&[
            "pool.ntp.org".into(),
            "--count".into(),
            "8".into(),
            "--json".into(),
        ])
        .expect("parse");
        assert_eq!(opts.count, 8);
        assert!(opts.json);
        assert!(
            Options::parse(&["--json".into()]).is_err(),
            "server required"
        );
        assert!(
            Options::parse(&["h".into(), "--count".into(), "0".into()]).is_err(),
            "count 0 rejected"
        );
    }
}
