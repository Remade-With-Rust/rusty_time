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

/// `adjtime` takes a whole-microsecond correction, so that is its granularity
/// expressed as a rate over the horizon we re-arm on.
const ADJTIME_RESOLUTION_US: f64 = 1.0;
/// macOS slews at a fixed modest rate; asking for more just takes longer.
const MAX_SLEW_PPM: f64 = 5_000.0;

fn errno_detail(op: &'static str) -> ClockError {
    ClockError {
        op,
        detail: std::io::Error::last_os_error().to_string(),
    }
}

pub(crate) fn platform_capabilities() -> crate::ClockCapabilities {
    crate::ClockCapabilities {
        os: "macos",
        arch: std::env::consts::ARCH,
        can_read: true,
        // SAFETY: geteuid takes no arguments and cannot fail.
        can_discipline: unsafe { libc::geteuid() } == 0,
        discipline_requirement: "root (macOS exposes no per-binary time capability); \
                                 run from launchd as a system daemon",
        slew_resolution_ppm: Some(ADJTIME_RESOLUTION_US),
        max_slew_ppm: MAX_SLEW_PPM,
        batch_receive: false,
        mono_resolution_ns: None,
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
                let amount =
                    crate::slew::macos_adjtime_amount(freq_ppm, drain_offset, drain_rate_ppm);
                // Below adjtime's own microsecond granularity there is nothing
                // to ask for; issuing it anyway would just round to zero.
                if amount.abs() < ADJTIME_RESOLUTION_US * 1e-6 {
                    return Ok(());
                }
                self.adjtime_by(amount)
            }
        }
    }
}
