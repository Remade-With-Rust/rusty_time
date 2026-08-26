//! Windows driver: GetSystemTimePreciseAsFileTime + QueryPerformanceCounter reads;
//! SetSystemTimeAdjustmentPrecise slews; SetSystemTime steps.
//!
//! Slew and step require SeSystemtimePrivilege (run elevated / as a service);
//! reads never do. `rtimec doctor` reports which half is available.

use crate::{ClockDrive, ClockError, ClockRead};
use rusty_time_core::ClockCommand;
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::SystemInformation::SetSystemTime;
use windows_sys::Win32::System::SystemInformation::{
    GetSystemTimeAdjustmentPrecise, GetSystemTimePreciseAsFileTime, SetSystemTimeAdjustmentPrecise,
};
use windows_sys::Win32::System::Time::FileTimeToSystemTime;

/// 100 ns intervals between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF_100NS: i128 = 116_444_736_000_000_000;

pub struct SystemClock;

fn last_error(op: &'static str) -> ClockError {
    ClockError {
        op,
        detail: std::io::Error::last_os_error().to_string(),
    }
}

impl ClockRead for SystemClock {
    fn wall_ns(&self) -> Result<i128, ClockError> {
        let mut ft = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        // SAFETY: ft is a valid, exclusively-owned FILETIME out-param.
        unsafe { GetSystemTimePreciseAsFileTime(&mut ft) };
        let ticks = ((ft.dwHighDateTime as i128) << 32) | ft.dwLowDateTime as i128;
        Ok((ticks - EPOCH_DIFF_100NS) * 100)
    }

    fn mono_s(&self) -> Result<f64, ClockError> {
        let mut count = 0i64;
        let mut freq = 0i64;
        // SAFETY: valid out-params; QPC cannot fail on XP+ per contract.
        let ok1 = unsafe { QueryPerformanceCounter(&mut count) };
        let ok2 = unsafe { QueryPerformanceFrequency(&mut freq) };
        if ok1 == 0 || ok2 == 0 || freq == 0 {
            return Err(last_error("QueryPerformanceCounter"));
        }
        Ok(count as f64 / freq as f64)
    }
}

impl ClockDrive for SystemClock {
    fn apply(&mut self, cmd: &ClockCommand) -> Result<(), ClockError> {
        match *cmd {
            ClockCommand::Step { add_seconds } => {
                let target_ns = self.wall_ns()? + (add_seconds * 1e9) as i128;
                let ticks = target_ns / 100 + EPOCH_DIFF_100NS;
                let ft = FILETIME {
                    dwLowDateTime: ticks as u32,
                    dwHighDateTime: (ticks >> 32) as u32,
                };
                let mut st = SYSTEMTIME {
                    wYear: 0,
                    wMonth: 0,
                    wDayOfWeek: 0,
                    wDay: 0,
                    wHour: 0,
                    wMinute: 0,
                    wSecond: 0,
                    wMilliseconds: 0,
                };
                // SAFETY: valid in/out params for the conversion.
                if unsafe { FileTimeToSystemTime(&ft, &mut st) } == 0 {
                    return Err(last_error("FileTimeToSystemTime"));
                }
                // SAFETY: st is a valid SYSTEMTIME; requires SeSystemtimePrivilege
                // and the error path reports that faithfully.
                if unsafe { SetSystemTime(&st) } == 0 {
                    return Err(last_error("SetSystemTime"));
                }
                Ok(())
            }
            ClockCommand::Slew {
                freq_ppm,
                drain_offset,
                drain_rate_ppm,
            } => {
                // Baseline increment: what one interrupt period advances the clock
                // by when no adjustment is active.
                let mut adjustment = 0u64;
                let mut increment = 0u64;
                let mut disabled = 0i32;
                // SAFETY: three valid out-params.
                if unsafe {
                    GetSystemTimeAdjustmentPrecise(&mut adjustment, &mut increment, &mut disabled)
                } == 0
                {
                    return Err(last_error("GetSystemTimeAdjustmentPrecise"));
                }
                let drain = drain_rate_ppm.copysign(drain_offset);
                let total_ppm = freq_ppm + drain;
                let new_adjustment = (increment as f64 * (1.0 + total_ppm * 1e-6)).max(0.0) as u64;
                // SAFETY: plain-value call; requires SeSystemtimePrivilege, error
                // path reports it.
                if unsafe { SetSystemTimeAdjustmentPrecise(new_adjustment, 0) } == 0 {
                    return Err(last_error("SetSystemTimeAdjustmentPrecise"));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_work_without_privilege() {
        let c = SystemClock;
        let w = c.wall_ns().expect("wall");
        // Sanity: after 2020-01-01 (1577836800 s), before 2100.
        assert!(w > 1_577_836_800_000_000_000 && w < 4_102_444_800_000_000_000i128);
        let m1 = c.mono_s().expect("mono");
        let m2 = c.mono_s().expect("mono");
        assert!(m2 >= m1);
    }
}
