//! rtimec — control CLI for rtimed (the chronyc analog).
//!
//! Every verb here is a thin consumer of a typed op in `rusty_time-api`
//! (mission plan §5). The CLI is consumer #1; a test and an agent are #2 and
//! #3 with a different transport and identical semantics.

use rusty_time_api::{ControlRequest, ControlResponse};
use rusty_time_clock::{ClockRead, SystemClock};

#[global_allocator]
static ALLOC: rusty_time_alloc::HouseAllocator = rusty_time_alloc::house_allocator();

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut rest: Vec<String> = Vec::new();
    let mut socket = default_control_path();
    let mut json = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" | "-s" => match it.next() {
                Some(v) => socket = v.clone(),
                None => {
                    eprintln!("rtimec: --socket needs a value");
                    std::process::exit(2);
                }
            },
            "--json" => json = true,
            other => rest.push(other.to_string()),
        }
    }

    let code = match rest.first().map(String::as_str) {
        Some("doctor") => doctor(),
        Some("ping") => op(&socket, ControlRequest::Ping, json),
        Some("serverstats") => op(&socket, ControlRequest::ServerStats, json),
        Some("ntsdata") => op(&socket, ControlRequest::NtsData, json),
        Some("clients") => {
            let limit = rest.get(1).and_then(|v| v.parse().ok()).unwrap_or(16usize);
            op(&socket, ControlRequest::Clients { limit }, json)
        }
        Some("version") | Some("--version") => {
            println!("rtimec {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            usage();
            2
        }
    };
    std::process::exit(code);
}

fn usage() {
    eprintln!("usage: rtimec [--socket PATH] [--json] <command>");
    eprintln!();
    eprintln!("  serverstats     request/response counters, drops, interleaved count");
    eprintln!("  clients [N]     most recently seen clients (default 16)");
    eprintln!("  ntsdata         NTS master key ids currently held");
    eprintln!("  ping            check the daemon is answering");
    eprintln!("  doctor          probe this machine's clock capabilities (no daemon needed)");
    eprintln!("  version         print version");
}

fn default_control_path() -> String {
    #[cfg(windows)]
    {
        r"\\.\pipe\rusty_time".to_string()
    }
    #[cfg(unix)]
    {
        match std::env::var("XDG_RUNTIME_DIR") {
            Ok(dir) if !dir.is_empty() => format!("{dir}/rusty_time.sock"),
            _ => "/tmp/rusty_time.sock".to_string(),
        }
    }
}

fn op(socket: &str, request: ControlRequest, json: bool) -> i32 {
    let response = match rusty_time_ctl::request(socket, &request) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("rtimec: {e}");
            eprintln!("       (is rtimed running? try --socket PATH)");
            return 1;
        }
    };
    if json {
        match serde_json::to_string_pretty(&response) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("rtimec: serializing response: {e}");
                return 1;
            }
        }
        return 0;
    }
    print_human(&response)
}

fn print_human(response: &ControlResponse) -> i32 {
    match response {
        ControlResponse::Pong { version } => {
            println!("rtimed {version} is answering");
            0
        }
        ControlResponse::ServerStats(s) => {
            println!("stratum            : {}", s.stratum);
            println!("uptime             : {} s", s.uptime_s);
            println!("ntp requests       : {}", s.ntp_requests);
            println!("ntp responses      : {}", s.ntp_responses);
            println!("dropped (ratelimit): {}", s.dropped_rate_limit);
            println!("kiss-o-death sent  : {}", s.kiss_of_death);
            println!("interleaved replies: {}", s.interleaved_responses);
            println!("refused            : {}", s.refused);
            println!("clients tracked    : {}", s.clients_tracked);
            println!("clients evicted    : {}", s.clients_evicted);
            0
        }
        ControlResponse::Clients { rows } => {
            if rows.is_empty() {
                println!("no clients seen yet");
                return 0;
            }
            println!(
                "{:<24} {:>10} {:>9} {:>9} {:>8} {:>6}",
                "address", "last seen", "requests", "responses", "dropped", "xleave"
            );
            for r in rows {
                println!(
                    "{:<24} {:>9.1}s {:>9} {:>9} {:>8} {:>6}",
                    r.address,
                    r.last_seen_ago_s,
                    r.requests,
                    r.responses,
                    r.dropped,
                    if r.interleaved { "yes" } else { "no" }
                );
            }
            0
        }
        ControlResponse::NtsData { master_key_ids } => {
            if master_key_ids.is_empty() {
                println!("no NTS master keys (server not running NTS?)");
            } else {
                println!("nts master keys: {}", master_key_ids.len());
                for id in master_key_ids {
                    println!("  id {id:#010x}");
                }
            }
            0
        }
        ControlResponse::Error { message } => {
            eprintln!("rtimed: {message}");
            1
        }
    }
}

fn doctor() -> i32 {
    let clock = SystemClock;
    println!("rtimec doctor — platform clock report");
    println!("  os               : {}", std::env::consts::OS);
    println!("  arch             : {}", std::env::consts::ARCH);

    match clock.wall_ns() {
        Ok(ns) => {
            let secs = ns / 1_000_000_000;
            println!("  wall clock       : ok ({secs} s since epoch)");
        }
        Err(e) => {
            println!("  wall clock       : FAILED — {e}");
            return 1;
        }
    }

    match clock.mono_s() {
        Ok(_) => {
            // Estimate read granularity: smallest nonzero delta over a burst.
            let mut min_delta = f64::INFINITY;
            let mut last = match clock.mono_s() {
                Ok(v) => v,
                Err(e) => {
                    println!("  monotonic clock  : FAILED — {e}");
                    return 1;
                }
            };
            for _ in 0..10_000 {
                if let Ok(now) = clock.mono_s() {
                    let d = now - last;
                    if d > 0.0 && d < min_delta {
                        min_delta = d;
                    }
                    last = now;
                }
            }
            if min_delta.is_finite() {
                println!(
                    "  monotonic clock  : ok (read granularity ≈ {:.0} ns)",
                    min_delta * 1e9
                );
            } else {
                println!("  monotonic clock  : ok");
            }
        }
        Err(e) => {
            println!("  monotonic clock  : FAILED — {e}");
            return 1;
        }
    }

    // Honesty over optimism: we do not probe clock-SETTING capability here,
    // because probing it would perturb the clock. Report what would be
    // required instead.
    #[cfg(windows)]
    println!("  discipline       : requires SeSystemtimePrivilege (run elevated / as service)");
    #[cfg(target_os = "linux")]
    println!("  discipline       : requires CAP_SYS_TIME (root or file capability)");
    #[cfg(target_os = "macos")]
    println!("  discipline       : requires root");

    #[cfg(target_os = "linux")]
    println!("  batch receive    : recvmmsg (up to 32 datagrams per syscall)");
    #[cfg(not(target_os = "linux"))]
    println!("  batch receive    : one datagram per syscall (recvmmsg is Linux-only)");

    0
}
