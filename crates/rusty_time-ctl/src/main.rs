//! rtimec — control CLI for rtimed.
//!
//! M2 surface: `doctor` (platform clock capability report) and `version`. The
//! full op surface (tracking, sources, makestep, …) arrives with the daemon's
//! control socket at M4; rtimec grows exactly as fast as the ops do.

use rusty_time_clock::{ClockRead, SystemClock};

#[global_allocator]
static ALLOC: rusty_time_alloc::HouseAllocator = rusty_time_alloc::house_allocator();

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("doctor") => doctor(),
        Some("version") | Some("--version") => {
            println!("rtimec {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            eprintln!("usage: rtimec <doctor|version>");
            eprintln!();
            eprintln!("  doctor   probe this machine's clock capabilities");
            eprintln!("  version  print version");
            2
        }
    };
    std::process::exit(code);
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
    // because probing it would perturb the clock. Report what would be required.
    #[cfg(windows)]
    println!("  discipline       : requires SeSystemtimePrivilege (run elevated / as service)");
    #[cfg(target_os = "linux")]
    println!("  discipline       : requires CAP_SYS_TIME (root or file capability)");
    #[cfg(target_os = "macos")]
    println!("  discipline       : requires root");

    0
}
