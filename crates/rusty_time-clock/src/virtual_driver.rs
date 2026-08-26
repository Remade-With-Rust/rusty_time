//! The no-privilege driver: commands adjust a [`VirtualClock`] view, never the OS
//! clock. This is the wasm story and the unprivileged-process story, and it is
//! what the TIMECORP simulator validates against.

use crate::{ClockDrive, ClockError};
use rusty_time_core::{ClockCommand, VirtualClock};

#[derive(Debug, Default)]
pub struct VirtualDriver {
    vclock: VirtualClock,
    /// Accumulated step corrections commanded so far, seconds.
    stepped: f64,
    /// Last commanded frequency, ppm.
    freq_ppm: f64,
}

impl VirtualDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// The disciplined view of time. `mono` and `raw_wall` are the platform's
    /// current readings (or the host page's, on wasm).
    pub fn vclock(&mut self) -> &mut VirtualClock {
        &mut self.vclock
    }

    pub fn freq_ppm(&self) -> f64 {
        self.freq_ppm
    }

    pub fn stepped_total(&self) -> f64 {
        self.stepped
    }

    /// Feed the latest measurement (mono seconds, offset-to-add seconds, skew ppm,
    /// error bound) straight through to the virtual clock.
    pub fn update_measurement(
        &mut self,
        mono: f64,
        offset: f64,
        skew_ppm: Option<f64>,
        error_bound: f64,
    ) {
        self.vclock.update(mono, offset, skew_ppm, error_bound);
    }
}

impl ClockDrive for VirtualDriver {
    fn apply(&mut self, cmd: &ClockCommand) -> Result<(), ClockError> {
        match *cmd {
            ClockCommand::Step { add_seconds } => {
                self.stepped += add_seconds;
                Ok(())
            }
            ClockCommand::Slew { freq_ppm, .. } => {
                // The virtual clock absorbs offset corrections through
                // update_measurement; the drive side only tracks frequency.
                self.freq_ppm = freq_ppm;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_absorbed() {
        let mut d = VirtualDriver::new();
        d.apply(&ClockCommand::Step { add_seconds: 0.5 })
            .expect("step");
        d.apply(&ClockCommand::Slew {
            freq_ppm: -12.0,
            drain_offset: 0.001,
            drain_rate_ppm: 500.0,
        })
        .expect("slew");
        assert!((d.stepped_total() - 0.5).abs() < 1e-12);
        assert!((d.freq_ppm() + 12.0).abs() < 1e-12);
    }
}
