//! `rtimed refclock` — read a reference clock and report what it says.
//!
//! An op before it is a daemon feature (mission plan §5): this is how a human,
//! a test and the smoke rig all confirm a GPS, a SOCK producer or a PTP
//! hardware clock is actually reachable, before anyone asks the discipline
//! loop to trust it.

#[cfg(unix)]
use rusty_time_clock::refclock;

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        #[cfg(unix)]
        Some("shm") => {
            let unit = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            shm(unit)
        }
        #[cfg(unix)]
        Some("sock") => {
            let path = args
                .get(1)
                .cloned()
                .unwrap_or_else(|| "/run/rusty_time.sock.refclock".to_string());
            let seconds = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(5u64);
            sock(&path, seconds)
        }
        #[cfg(target_os = "linux")]
        Some("phc") => {
            let index = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            phc(index)
        }
        _ => {
            eprintln!("usage: rtimed refclock <shm [UNIT] | sock [PATH] [SECONDS] | phc [INDEX]>");
            eprintln!();
            eprintln!("  shm   read a gpsd/ntpd shared-memory segment (NTP0 + unit)");
            eprintln!("  sock  bind chrony's SOCK protocol and report samples");
            eprintln!("  phc   read a PTP hardware clock (/dev/ptpN)");
            eprintln!();
            eprintln!("These read only; none of them touches the system clock.");
            2
        }
    }
}

#[cfg(unix)]
fn report(kind: &str, sample: rusty_time_core::refclock::RefclockSample) -> i32 {
    println!("{kind}:");
    println!("  local     : {:.9} s", sample.local_s);
    println!("  reference : {:.9} s", sample.reference_s());
    println!("  offset    : {:+.9} s", sample.offset_s());
    println!("  dispersion: {:.9} s", sample.dispersion_s());
    println!("  leap      : {:?}", sample.leap);
    match sample.validate(None) {
        Ok(()) => {
            println!("  usable    : yes");
            0
        }
        Err(e) => {
            // A refclock that reports a tidy but wrong time is the dangerous
            // case, so say exactly why it was refused.
            println!("  usable    : NO — {e}");
            1
        }
    }
}

#[cfg(unix)]
fn shm(unit: i32) -> i32 {
    match refclock::shm::ShmRefclock::attach(unit) {
        Ok(clock) => match clock.sample() {
            Some(sample) => report(&format!("SHM unit {unit}"), sample),
            None => {
                println!("SHM unit {unit}: attached, but no valid sample published");
                1
            }
        },
        Err(e) => {
            eprintln!("rtimed refclock shm: {e}");
            eprintln!("       (is gpsd running with SHM export enabled?)");
            1
        }
    }
}

#[cfg(unix)]
fn sock(path: &str, seconds: u64) -> i32 {
    let clock = match refclock::sock::SockRefclock::bind(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rtimed refclock sock: {e}");
            return 1;
        }
    };
    println!("listening on {path} for {seconds}s (point a producer at it)");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut count = 0;
    while std::time::Instant::now() < deadline {
        if let Some(sample) = clock.try_sample() {
            count += 1;
            let _ = report(&format!("SOCK sample {count}"), sample);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = std::fs::remove_file(path);
    if count == 0 {
        println!("no samples received");
        return 1;
    }
    println!("received {count} sample(s)");
    0
}

#[cfg(target_os = "linux")]
fn phc(index: u32) -> i32 {
    match refclock::phc::Phc::open(index) {
        Ok(clock) => match clock.sample() {
            Ok(sample) => report(&format!("PHC /dev/ptp{index}"), sample),
            Err(e) => {
                eprintln!("rtimed refclock phc: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("rtimed refclock phc: {e}");
            eprintln!("       (no PTP hardware clock, or no permission to read it)");
            1
        }
    }
}
