//! rusty_time-clock — the platform seam.
//!
//! Three capabilities, each a trait so the daemon, the tests, and the TIMECORP
//! simulator plug the same discipline loop into different worlds:
//!
//! * [`ClockRead`] — wall + monotonic time.
//! * [`ClockDrive`] — execute [`ClockCommand`]s (slew / step).
//! * [`VirtualDriver`] — the no-privilege fallback: applies commands to a
//!   [`rusty_time_core::VirtualClock`] view instead of the OS clock.
//!
//! This is the only crate in the workspace where `unsafe` is permitted; every
//! block names its invariant.

use core::fmt;
use rusty_time_core::ClockCommand;

#[derive(Clone, Debug)]
pub struct ClockError {
    /// Which operation failed (static, greppable).
    pub op: &'static str,
    /// OS-level detail.
    pub detail: String,
}

impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "clock {}: {}", self.op, self.detail)
    }
}

impl std::error::Error for ClockError {}

/// Read the platform clocks.
pub trait ClockRead {
    /// Wall-clock nanoseconds since the Unix epoch.
    fn wall_ns(&self) -> Result<i128, ClockError>;
    /// Monotonic seconds from an arbitrary origin, unaffected by steps.
    fn mono_s(&self) -> Result<f64, ClockError>;
}

/// Drive the platform clock.
pub trait ClockDrive {
    fn apply(&mut self, cmd: &ClockCommand) -> Result<(), ClockError>;
}

/// What this machine can actually do, probed without perturbing the clock.
///
/// Deliberately separates *reading* from *disciplining*: reading needs no
/// privilege anywhere, disciplining needs it everywhere, and a daemon that
/// cannot tell the difference reports "working" while silently never
/// correcting anything. The smoke rigs assert on these fields.
#[derive(Clone, Debug, PartialEq)]
pub struct ClockCapabilities {
    pub os: &'static str,
    pub arch: &'static str,
    /// Wall and monotonic clocks readable.
    pub can_read: bool,
    /// Whether this process holds what disciplining requires. Probed by
    /// inspecting privileges, never by attempting an adjustment — a probe that
    /// moves the clock to find out whether it may is not a probe.
    pub can_discipline: bool,
    /// What disciplining needs here, for the operator who has `false` above.
    pub discipline_requirement: &'static str,
    /// Granularity of the platform's frequency-adjustment knob, in ppm.
    /// `None` where the platform exposes no frequency knob at all.
    pub slew_resolution_ppm: Option<f64>,
    /// Ceiling the platform imposes on frequency adjustment, in ppm.
    pub max_slew_ppm: f64,
    /// Whether receives are batched (one syscall for many datagrams).
    pub batch_receive: bool,
    /// Measured granularity of a monotonic clock read, nanoseconds.
    pub mono_resolution_ns: Option<f64>,
}

impl ClockCapabilities {
    /// Measure how finely the monotonic clock actually ticks, by finding the
    /// smallest non-zero step across a burst of reads. A *measurement*, not the
    /// value the platform advertises — those disagree often enough to matter.
    pub fn measure_mono_resolution<C: ClockRead>(clock: &C) -> Option<f64> {
        let mut smallest = f64::INFINITY;
        let mut last = clock.mono_s().ok()?;
        for _ in 0..20_000 {
            let now = clock.mono_s().ok()?;
            let delta = now - last;
            if delta > 0.0 && delta < smallest {
                smallest = delta;
            }
            last = now;
        }
        smallest.is_finite().then_some(smallest * 1e9)
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::SystemClock;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::SystemClock;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::SystemClock;

mod virtual_driver;
pub use virtual_driver::VirtualDriver;

/// Platform slew arithmetic, testable on every host (see the module docs for
/// why that is not merely tidiness).
pub mod slew;

/// Reference-clock transports: gpsd shared memory, chrony's SOCK protocol,
/// and PTP hardware clocks.
#[cfg(any(unix, target_os = "linux"))]
pub mod refclock;

#[cfg(any(unix, windows))]
pub mod net;

/// Probe this machine's clock capabilities.
pub fn capabilities() -> ClockCapabilities {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        let mut caps = platform_capabilities();
        let clock = SystemClock;
        caps.can_read = clock.wall_ns().is_ok() && clock.mono_s().is_ok();
        caps.mono_resolution_ns = ClockCapabilities::measure_mono_resolution(&clock);
        caps
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        ClockCapabilities {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            can_read: false,
            can_discipline: false,
            discipline_requirement: "no platform driver for this target",
            slew_resolution_ppm: None,
            max_slew_ppm: 0.0,
            batch_receive: false,
            mono_resolution_ns: None,
        }
    }
}

#[cfg(target_os = "linux")]
use linux::platform_capabilities;
#[cfg(target_os = "macos")]
use macos::platform_capabilities;
#[cfg(windows)]
use windows::platform_capabilities;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_are_self_consistent() {
        let caps = capabilities();
        assert_eq!(caps.os, std::env::consts::OS);
        // Reading is unprivileged on every platform we support, so a build
        // that cannot read its own clock is broken, not merely unprivileged.
        assert!(caps.can_read, "clock must be readable without privilege");
        assert!(
            !caps.discipline_requirement.is_empty(),
            "an operator who cannot discipline must be told what it needs"
        );
        // Resolution is a measurement; if present it must be sane.
        if let Some(res) = caps.mono_resolution_ns {
            assert!(
                res > 0.0 && res < 1e9,
                "implausible monotonic resolution {res} ns"
            );
        }
        assert!(caps.max_slew_ppm > 0.0);
    }

    #[test]
    fn batch_receive_matches_the_platform() {
        // recvmmsg is Linux-only; claiming it elsewhere would make a smoke rig
        // assert a capability that does not exist.
        assert_eq!(capabilities().batch_receive, cfg!(target_os = "linux"));
    }
}
