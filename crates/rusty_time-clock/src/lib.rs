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
