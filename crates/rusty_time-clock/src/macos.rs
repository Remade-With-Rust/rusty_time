//! macOS driver: clock_gettime reads; adjtime(2) micro-slews.
//!
//! macOS has no adjtimex-class frequency interface, so frequency discipline is
//! executed as re-armed offset slews: the daemon re-plans every poll and this
//! driver converts the plan's net rate over the coming interval into one adjtime
//! call (chrony's macOS strategy, from its documentation). M5 refines this with
//! measured slew-rate calibration.

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
        // SAFETY: ts is valid and exclusively owned for the call.
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
        // SAFETY: ts is valid and exclusively owned; CLOCK_MONOTONIC_RAW exists on
        // macOS 10.12+ (our floor).
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
        if rc != 0 {
            return Err(errno_detail("clock_gettime(MONOTONIC_RAW)"));
        }
        Ok(ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9)
    }
}

impl SystemClock {
    fn adjtime_by(&self, seconds: f64) -> Result<(), ClockError> {
        let sec = seconds.trunc() as libc::time_t;
        let usec = ((seconds - seconds.trunc()) * 1e6) as libc::suseconds_t;
        let delta = libc::timeval {
            tv_sec: sec,
            tv_usec: usec,
        };
        // SAFETY: delta is a valid timeval; passing null for the old delta is
        // documented and means "don't report the outstanding correction".
        let rc = unsafe { libc::adjtime(&delta, core::ptr::null_mut()) };
        if rc != 0 {
            return Err(errno_detail("adjtime"));
        }
        Ok(())
    }
}

impl ClockDrive for SystemClock {
    fn apply(&mut self, cmd: &ClockCommand) -> Result<(), ClockError> {
        match *cmd {
            ClockCommand::Step { add_seconds } => {
                let now = self.wall_ns()?;
                let target_ns = now + (add_seconds * 1e9) as i128;
                let ts = libc::timespec {
                    tv_sec: (target_ns / 1_000_000_000) as libc::time_t,
                    tv_nsec: (target_ns % 1_000_000_000) as libc::c_long,
                };
                // SAFETY: ts is valid; requires root, and the error path reports
                // EPERM faithfully rather than pretending success.
                let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
                if rc != 0 {
                    return Err(errno_detail("clock_settime"));
                }
                Ok(())
            }
            ClockCommand::Slew {
                freq_ppm,
                drain_offset,
                drain_rate_ppm,
            } => {
                // Convert the plan into "seconds to smear before the next plan".
                // The daemon re-plans each poll interval; use the drain budget as
                // the amount, and fold the frequency term in at the drain horizon.
                let horizon_s = if drain_rate_ppm > 0.0 {
                    (drain_offset.abs() / (drain_rate_ppm * 1e-6)).min(1024.0)
                } else {
                    0.0
                };
                let freq_term = freq_ppm * 1e-6 * horizon_s;
                let amount = drain_offset + freq_term;
                if amount.abs() < 1e-7 {
                    return Ok(());
                }
                self.adjtime_by(amount)
            }
        }
    }
}
