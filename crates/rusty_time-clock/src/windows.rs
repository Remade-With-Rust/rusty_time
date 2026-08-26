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

/// Windows clamps the adjustment to roughly ±10% of the increment; we stay well
/// inside that, and the discipline loop's own `max_freq_ppm` is tighter still.
const MAX_SLEW_PPM: f64 = 100_000.0;

pub struct SystemClock;

/// Does this process hold `SeSystemtimePrivilege`?
///
/// Checked with `PrivilegeCheck`, which inspects the token and changes
/// nothing. The alternative — trying an adjustment to see whether it works —
/// would move the very clock we are asking about.
pub fn has_system_time_privilege() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows_sys::Win32::Security::{
        LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, PRIVILEGE_SET, PrivilegeCheck,
        SE_PRIVILEGE_ENABLED, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // "SeSystemtimePrivilege" as a NUL-terminated wide string.
    let name: Vec<u16> = "SeSystemtimePrivilege"
        .encode_utf16()
        .chain(core::iter::once(0))
        .collect();

    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    // SAFETY: `name` is a valid NUL-terminated wide string that outlives the
    // call; `luid` is a valid out-param.
    if unsafe { LookupPrivilegeValueW(core::ptr::null(), name.as_ptr(), &mut luid) } == 0 {
        return false;
    }

    let mut token: HANDLE = core::ptr::null_mut();
    // SAFETY: valid out-param; the pseudo-handle from GetCurrentProcess needs
    // no closing.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }

    let mut set = PRIVILEGE_SET {
        PrivilegeCount: 1,
        Control: 1, // PRIVILEGE_SET_ALL_NECESSARY
        Privilege: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    let mut result: i32 = 0;
    // SAFETY: `token` is an open token handle, `set` and `result` are valid.
    let ok = unsafe { PrivilegeCheck(token, &mut set, &mut result) } != 0;
    // SAFETY: `token` came from OpenProcessToken and is not used again.
    unsafe { CloseHandle(token) };

    ok && result != 0
}

pub(crate) fn platform_capabilities() -> crate::ClockCapabilities {
    // The interrupt period sets the granularity of the frequency knob: one
    // 100 ns unit of adjustment per increment.
    let resolution = system_time_adjustment()
        .ok()
        .map(|(_, increment, _)| 1e6 / increment as f64);
    crate::ClockCapabilities {
        os: "windows",
        arch: std::env::consts::ARCH,
        can_read: true,
        can_discipline: has_system_time_privilege(),
        discipline_requirement: "SeSystemtimePrivilege (run elevated or as a service), \
                                 and the Windows Time service must not be disciplining too",
        slew_resolution_ppm: resolution,
        max_slew_ppm: MAX_SLEW_PPM,
        batch_receive: false,
        mono_resolution_ns: None,
    }
}

/// (current adjustment, increment, adjustment disabled) in 100 ns units.
fn system_time_adjustment() -> Result<(u64, u64, bool), ClockError> {
    let mut adjustment = 0u64;
    let mut increment = 0u64;
    let mut disabled = 0i32;
    // SAFETY: three valid out-params.
    if unsafe { GetSystemTimeAdjustmentPrecise(&mut adjustment, &mut increment, &mut disabled) }
        == 0
    {
        return Err(last_error("GetSystemTimeAdjustmentPrecise"));
    }
    Ok((adjustment, increment, disabled != 0))
}

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
                // The increment is what one interrupt period advances the clock
                // by with no adjustment active; the adjustment replaces it.
                let (_, increment, _) = system_time_adjustment()?;
                let total_ppm =
                    crate::slew::total_ppm(freq_ppm, drain_offset, drain_rate_ppm, MAX_SLEW_PPM);
                let new_adjustment = crate::slew::windows_adjustment(increment, total_ppm);
                // SAFETY: plain-value call. Requires SeSystemtimePrivilege;
                // the error path reports its absence rather than pretending.
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
    fn privilege_probe_does_not_disturb_the_clock() {
        // Probing must be side-effect free: the clock before and after must
        // differ only by elapsed time, never by a jump.
        let clock = SystemClock;
        let before = clock.wall_ns().expect("wall");
        let privileged = has_system_time_privilege();
        let after = clock.wall_ns().expect("wall");
        let elapsed_ms = (after - before) as f64 / 1e6;
        assert!(
            (0.0..500.0).contains(&elapsed_ms),
            "probe moved the clock by {elapsed_ms} ms"
        );
        // Whatever the answer, capabilities must agree with the probe.
        assert_eq!(platform_capabilities().can_discipline, privileged);
    }

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
