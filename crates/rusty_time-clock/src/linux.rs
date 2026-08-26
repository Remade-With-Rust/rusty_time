//! Linux driver: clock_gettime + clock_adjtime (adjtimex).
//!
//! M2 scope: read, frequency slew, precise step. Hardware timestamping, PHC, and
//! refclocks are M7 (mission plan §6.1).

use crate::{ClockDrive, ClockError, ClockRead};
use rusty_time_core::ClockCommand;

pub struct SystemClock;

fn errno_detail(op: &'static str) -> ClockError {
    ClockError {
        op,
        detail: std::io::Error::last_os_error().to_string(),
    }
}

impl ClockRead for SystemClock {
    fn wall_ns(&self) -> Result<i128, ClockError> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: ts is a valid, exclusively-owned timespec for the duration of
        // the call; CLOCK_REALTIME is always available.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
        if rc != 0 {
            return Err(errno_detail("clock_gettime(REALTIME)"));
        }
        Ok(ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128)
    }

    fn mono_s(&self) -> Result<f64, ClockError> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: as above; MONOTONIC_RAW is immune to NTP slewing, which is the
        // point — sample timestamps must not see our own corrections.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
        if rc != 0 {
            return Err(errno_detail("clock_gettime(MONOTONIC_RAW)"));
        }
        Ok(ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9)
    }
}

/// scaled ppm: adjtimex frequency unit is 2^-16 ppm.
const FREQ_SCALE: f64 = 65_536.0;

impl ClockDrive for SystemClock {
    fn apply(&mut self, cmd: &ClockCommand) -> Result<(), ClockError> {
        match *cmd {
            ClockCommand::Step { add_seconds } => {
                let mut tx: libc::timex = // SAFETY: timex is a plain-old-data
                    // struct; zeroed is a valid initial state the kernel accepts.
                    unsafe { core::mem::zeroed() };
                tx.modes = libc::ADJ_SETOFFSET | libc::ADJ_NANO;
                let ns_total = (add_seconds * 1e9) as i64;
                let mut sec = ns_total / 1_000_000_000;
                let mut nsec = ns_total % 1_000_000_000;
                if nsec < 0 {
                    // ADJ_SETOFFSET requires tv_usec/tv_nsec in [0, 1e9).
                    sec -= 1;
                    nsec += 1_000_000_000;
                }
                tx.time.tv_sec = sec;
                tx.time.tv_usec = nsec; // ADJ_NANO: field carries nanoseconds
                // SAFETY: tx is valid and exclusively owned for the call.
                let rc = unsafe { libc::clock_adjtime(libc::CLOCK_REALTIME, &mut tx) };
                if rc < 0 {
                    return Err(errno_detail("clock_adjtime(ADJ_SETOFFSET)"));
                }
                Ok(())
            }
            ClockCommand::Slew {
                freq_ppm,
                drain_offset,
                drain_rate_ppm,
            } => {
                // The daemon re-plans every poll, so the drain is folded into the
                // commanded frequency until the next plan (mission plan §6.1; the
                // TIMECORP simulator models exactly this driver behavior).
                let drain = drain_rate_ppm.copysign(drain_offset);
                let total_ppm = (freq_ppm + drain).clamp(-32_767.0, 32_767.0);
                let mut tx: libc::timex =
                    // SAFETY: plain-old-data, zeroed is valid.
                    unsafe { core::mem::zeroed() };
                tx.modes = libc::ADJ_FREQUENCY;
                tx.freq = (total_ppm * FREQ_SCALE) as libc::c_long;
                // SAFETY: tx is valid and exclusively owned for the call.
                let rc = unsafe { libc::clock_adjtime(libc::CLOCK_REALTIME, &mut tx) };
                if rc < 0 {
                    return Err(errno_detail("clock_adjtime(ADJ_FREQUENCY)"));
                }
                Ok(())
            }
        }
    }
}
