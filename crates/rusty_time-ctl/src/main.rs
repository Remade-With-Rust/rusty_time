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
    let mut socket = rusty_time_api::default_control_spec();
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
        Some("doctor") => doctor(json),
        Some("ping") => op(&socket, ControlRequest::Ping, json),
        Some("serverstats") => op(&socket, ControlRequest::ServerStats, json),
        Some("ntsdata") => op(&socket, ControlRequest::NtsData, json),
        Some("tracking") => op(&socket, ControlRequest::Tracking, json),
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
    eprintln!("  tracking        this clock's offset, error bound, and whether it is synchronized");
    eprintln!("  serverstats     request/response counters, drops, interleaved count");
    eprintln!("  clients [N]     most recently seen clients (default 16)");
    eprintln!("  ntsdata         NTS master key ids currently held");
    eprintln!("  ping            check the daemon is answering");
    eprintln!("  doctor          probe this machine's clock capabilities (no daemon needed)");
    eprintln!("  version         print version");
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
        ControlResponse::Tracking(t) => {
            // The bound is the number a caller decides on, so lead with the
            // verdict and print the bound next to the offset it qualifies.
            println!(
                "synchronized       : {}",
                if t.synchronized { "yes" } else { "no" }
            );
            println!("offset             : {:+.9} s", t.offset_s);
            println!("error bound        : {:.9} s", t.error_bound_s);
            println!("frequency          : {:+.3} ppm", t.freq_ppm);
            println!("poll interval      : 2^{} s", t.poll_log2);
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

fn doctor(json: bool) -> i32 {
    let clock = SystemClock;
    let caps = rusty_time_clock::capabilities();

    if json {
        // The smoke rigs parse this; keep the field names stable.
        println!("{{");
        println!("  \"os\": \"{}\",", caps.os);
        println!("  \"arch\": \"{}\",", caps.arch);
        println!("  \"can_read\": {},", caps.can_read);
        println!("  \"can_discipline\": {},", caps.can_discipline);
        println!("  \"batch_receive\": {},", caps.batch_receive);
        println!("  \"max_slew_ppm\": {},", caps.max_slew_ppm);
        match caps.mono_resolution_ns {
            Some(r) => println!("  \"mono_resolution_ns\": {r:.1},"),
            None => println!("  \"mono_resolution_ns\": null,"),
        }
        println!(
            "  \"discipline_requirement\": \"{}\"",
            caps.discipline_requirement.replace('"', "'")
        );
        println!("}}");
        return if caps.can_read { 0 } else { 1 };
    }

    println!("rtimec doctor — platform clock report");
    println!("  os               : {} ({})", caps.os, caps.arch);

    match clock.wall_ns() {
        Ok(ns) => println!(
            "  wall clock       : ok ({} s since epoch)",
            ns / 1_000_000_000
        ),
        Err(e) => {
            println!("  wall clock       : FAILED — {e}");
            return 1;
        }
    }

    match caps.mono_resolution_ns {
        Some(res) => println!("  monotonic clock  : ok (measured granularity ≈ {res:.0} ns)"),
        None if caps.can_read => println!("  monotonic clock  : ok"),
        None => {
            println!("  monotonic clock  : FAILED");
            return 1;
        }
    }

    // Probed, not assumed: this says whether disciplining would actually work
    // *right now*, which is the question an operator is asking.
    if caps.can_discipline {
        println!("  discipline       : available");
    } else {
        println!("  discipline       : NOT available in this process");
        println!("                     needs {}", caps.discipline_requirement);
    }
    match caps.slew_resolution_ppm {
        Some(res) => println!(
            "  slew             : {res:.4} ppm steps, up to ±{:.0} ppm",
            caps.max_slew_ppm
        ),
        None => println!("  slew             : no frequency knob on this platform"),
    }
    println!(
        "  batch receive    : {}",
        if caps.batch_receive {
            "recvmmsg (up to 32 datagrams per syscall)"
        } else {
            "one datagram per syscall (recvmmsg is Linux-only)"
        }
    );

    0
}
