//! Reference-clock transports: gpsd's shared memory, chrony's SOCK protocol,
//! and PTP hardware clocks.
//!
//! These are the interfaces a GPS receiver or a NIC actually presents. The
//! decision about whether a reading is *usable* lives in
//! `rusty_time_core::refclock`; this module only gets the bytes.
//!
//! Both wire layouts below are ABI-compatible with what gpsd and chrony
//! already write, because the point of implementing them is to work with the
//! receivers people already run.

use crate::ClockError;
use rusty_time_core::refclock::{LeapWarning, RefclockSample};

/// gpsd / ntpd shared-memory refclock (the `SHM` driver).
///
/// gpsd writes a fixed struct into a SysV shared-memory segment keyed
/// `0x4E545030 + unit` — the ASCII of "NTP0", "NTP1" and so on. Every NTP
/// implementation reads the same layout, which is why it is spelled out here
/// rather than generated: the struct is the interface.
#[cfg(unix)]
pub mod shm {
    use super::*;

    /// Base SysV key: the bytes "NTP0".
    pub const SHM_KEY_BASE: i32 = 0x4E54_5030;

    /// Byte offsets into `struct shmTime` on a 64-bit platform, where
    /// `time_t` is 8 bytes and the compiler pads before each one.
    mod layout {
        pub const MODE: usize = 0;
        pub const COUNT: usize = 4;
        pub const CLOCK_SEC: usize = 8;
        pub const CLOCK_USEC: usize = 16;
        pub const RECEIVE_SEC: usize = 24;
        pub const RECEIVE_USEC: usize = 32;
        pub const LEAP: usize = 36;
        pub const PRECISION: usize = 40;
        pub const VALID: usize = 48;
        pub const CLOCK_NSEC: usize = 52;
        pub const RECEIVE_NSEC: usize = 56;
        /// Total size including trailing `dummy[8]` and padding.
        pub const SIZE: usize = 96;
    }

    /// An attached gpsd shared-memory segment.
    pub struct ShmRefclock {
        base: *mut u8,
        unit: i32,
    }

    // SAFETY: the pointer addresses a shared-memory mapping owned by this
    // process for the object's lifetime; all access below is through volatile
    // reads of plain integers at fixed offsets, and the type exposes no way to
    // alias it mutably.
    unsafe impl Send for ShmRefclock {}

    impl ShmRefclock {
        /// Attach to the segment for `unit` (0..=3 by convention).
        ///
        /// Read-only: a time client has no business writing a producer's
        /// segment, and attaching read-only means a bug here cannot corrupt
        /// gpsd's view.
        pub fn attach(unit: i32) -> Result<ShmRefclock, ClockError> {
            let key = SHM_KEY_BASE + unit;
            // SAFETY: shmget with size 0 and no IPC_CREAT looks up an existing
            // segment; it either returns a valid id or -1.
            let id = unsafe { libc::shmget(key, layout::SIZE, 0) };
            if id < 0 {
                return Err(ClockError {
                    op: "shmget",
                    detail: format!(
                        "no SHM segment for unit {unit} (key {key:#x}): {}",
                        std::io::Error::last_os_error()
                    ),
                });
            }
            // SAFETY: `id` is a valid segment id from shmget; SHM_RDONLY maps
            // it read-only.
            let base = unsafe { libc::shmat(id, core::ptr::null(), libc::SHM_RDONLY) };
            if base as isize == -1 {
                return Err(ClockError {
                    op: "shmat",
                    detail: std::io::Error::last_os_error().to_string(),
                });
            }
            Ok(ShmRefclock {
                base: base.cast::<u8>(),
                unit,
            })
        }

        pub fn unit(&self) -> i32 {
            self.unit
        }

        fn read_i32(&self, offset: usize) -> i32 {
            // SAFETY: offset is one of the fixed layout constants, all well
            // inside SIZE, and the mapping is at least SIZE bytes. Volatile
            // because a separate process writes this memory concurrently.
            unsafe { core::ptr::read_volatile(self.base.add(offset).cast::<i32>()) }
        }

        fn read_i64(&self, offset: usize) -> i64 {
            // SAFETY: as above; these offsets are 8-byte aligned by the layout.
            unsafe { core::ptr::read_volatile(self.base.add(offset).cast::<i64>()) }
        }

        /// Take a sample, or `None` if the producer has not published one.
        ///
        /// Implements the protocol's two modes. Mode 1 is the one that matters:
        /// the reader checks `count` before and after copying and retries if it
        /// changed, because the producer writes the struct without a lock and a
        /// sample torn across that write would be a plausible-looking wrong
        /// time.
        pub fn sample(&self) -> Option<RefclockSample> {
            for _ in 0..4 {
                if self.read_i32(layout::VALID) == 0 {
                    return None;
                }
                let mode = self.read_i32(layout::MODE);
                let count_before = self.read_i32(layout::COUNT);

                let clock_sec = self.read_i64(layout::CLOCK_SEC);
                let clock_usec = self.read_i32(layout::CLOCK_USEC);
                let clock_nsec = self.read_i32(layout::CLOCK_NSEC);
                let receive_sec = self.read_i64(layout::RECEIVE_SEC);
                let receive_usec = self.read_i32(layout::RECEIVE_USEC);
                let receive_nsec = self.read_i32(layout::RECEIVE_NSEC);
                let leap = self.read_i32(layout::LEAP);
                let precision = self.read_i32(layout::PRECISION);

                if mode == 1 {
                    let count_after = self.read_i32(layout::COUNT);
                    if count_before != count_after {
                        continue; // torn read; the producer was mid-update
                    }
                }

                // The nanosecond fields are authoritative when present; the
                // microsecond ones are the older interface kept for producers
                // that predate them.
                let sub_ns = |nsec: i32, usec: i32| -> i64 {
                    if nsec != 0 {
                        nsec as i64
                    } else {
                        usec as i64 * 1_000
                    }
                };
                let clock_ns = sub_ns(clock_nsec, clock_usec);
                let receive_ns = sub_ns(receive_nsec, receive_usec);

                // Difference the integer parts first. Converting both times to
                // f64 and subtracting would round each to ~100 ns at epoch
                // magnitude, losing exactly the precision a refclock exists to
                // provide.
                let offset_s =
                    (clock_sec - receive_sec) as f64 + (clock_ns - receive_ns) as f64 * 1e-9;

                return Some(RefclockSample {
                    // "clock" is the reference's time; "receive" is ours.
                    local_s: receive_sec as f64 + receive_ns as f64 * 1e-9,
                    offset_s,
                    precision_log2: precision.clamp(-32, 0) as i8,
                    leap: LeapWarning::from_wire(leap),
                });
            }
            None
        }
    }

    impl Drop for ShmRefclock {
        fn drop(&mut self) {
            // SAFETY: `base` came from shmat and is detached exactly once.
            unsafe {
                libc::shmdt(self.base.cast::<libc::c_void>());
            }
        }
    }
}

/// chrony's SOCK refclock protocol.
///
/// The producer (gpsd, or anything else) connects to a Unix datagram socket
/// and sends a fixed 40-byte struct per sample. Simpler and safer than shared
/// memory — no torn reads, and the kernel does the framing.
#[cfg(unix)]
pub mod sock {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    /// "SOCK" — the magic chrony checks so a stray datagram cannot be mistaken
    /// for a time sample.
    pub const SOCK_MAGIC: i32 = 0x534F_434B;
    /// Wire size of `struct sock_sample` on a 64-bit platform.
    pub const SAMPLE_SIZE: usize = 40;

    /// A listening SOCK refclock endpoint.
    pub struct SockRefclock {
        socket: UnixDatagram,
    }

    impl SockRefclock {
        /// Bind the socket a producer will send to.
        pub fn bind(path: &str) -> Result<SockRefclock, ClockError> {
            let _ = std::fs::remove_file(path);
            let socket = UnixDatagram::bind(path).map_err(|e| ClockError {
                op: "bind SOCK refclock",
                detail: format!("{path}: {e}"),
            })?;
            socket.set_nonblocking(true).map_err(|e| ClockError {
                op: "set_nonblocking",
                detail: e.to_string(),
            })?;
            Ok(SockRefclock { socket })
        }

        /// Read one pending sample, if any.
        pub fn try_sample(&self) -> Option<RefclockSample> {
            let mut buf = [0u8; SAMPLE_SIZE];
            let received = self.socket.recv(&mut buf).ok()?;
            if received != SAMPLE_SIZE {
                return None;
            }
            decode_sample(&buf)
        }
    }

    /// Decode one `struct sock_sample`. Separate and pure so the layout is
    /// tested without a socket.
    pub fn decode_sample(buf: &[u8]) -> Option<RefclockSample> {
        if buf.len() < SAMPLE_SIZE {
            return None;
        }
        // The length check above guarantees every fixed offset below is in
        // range, so these cannot fail; the arrays are sized at compile time.
        let i32_at = |o: usize| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&buf[o..o + 4]);
            i32::from_ne_bytes(b)
        };
        let i64_at = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[o..o + 8]);
            i64::from_ne_bytes(b)
        };
        let f64_at = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[o..o + 8]);
            f64::from_ne_bytes(b)
        };

        // struct sock_sample {
        //   struct timeval tv;  // 0: sec (8), usec (8)
        //   double offset;      // 16
        //   int pulse;          // 24
        //   int leap;           // 28
        //   int _pad;           // 32
        //   int magic;          // 36
        // }
        if i32_at(36) != SOCK_MAGIC {
            return None; // not one of ours
        }
        let sec = i64_at(0);
        let usec = i64_at(8);
        let offset = f64_at(16);
        let leap = i32_at(28);

        // The producer reports the offset directly, so it is stored verbatim
        // rather than folded into a reference time and subtracted back out.
        Some(RefclockSample {
            local_s: sec as f64 + usec as f64 * 1e-6,
            offset_s: offset,
            precision_log2: -20,
            leap: LeapWarning::from_wire(leap),
        })
    }
}

/// PTP hardware clocks (`/dev/ptp*`).
///
/// A PHC is a clock on the NIC (or, on a hypervisor, a paravirtual device)
/// that is disciplined independently of the system clock. Reading it tells us
/// what the system clock's error is against a source that did not go through
/// the network at all.
#[cfg(target_os = "linux")]
pub mod phc {
    use super::*;
    use std::os::fd::{AsRawFd, OwnedFd};

    /// An open PTP hardware clock.
    pub struct Phc {
        fd: OwnedFd,
        index: u32,
    }

    /// Turn a `/dev/ptpN` descriptor into the clockid `clock_gettime` wants.
    ///
    /// Linux encodes dynamic POSIX clocks as `(~fd << 3) | 3`; there is no
    /// constant for it, which is why it is spelled out.
    fn fd_to_clockid(fd: i32) -> libc::clockid_t {
        ((!fd) << 3) | 3
    }

    impl Phc {
        /// Open `/dev/ptp{index}`.
        pub fn open(index: u32) -> Result<Phc, ClockError> {
            let path = format!("/dev/ptp{index}");
            let file = std::fs::File::open(&path).map_err(|e| ClockError {
                op: "open PHC",
                detail: format!("{path}: {e}"),
            })?;
            Ok(Phc {
                fd: OwnedFd::from(file),
                index,
            })
        }

        pub fn index(&self) -> u32 {
            self.index
        }

        /// Read the PHC's current time, in Unix seconds.
        pub fn read_s(&self) -> Result<f64, ClockError> {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: ts is valid and exclusively owned; the clockid is derived
            // from a descriptor this object keeps open for the call's duration.
            let rc = unsafe { libc::clock_gettime(fd_to_clockid(self.fd.as_raw_fd()), &mut ts) };
            if rc != 0 {
                return Err(ClockError {
                    op: "clock_gettime(PHC)",
                    detail: std::io::Error::last_os_error().to_string(),
                });
            }
            Ok(ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9)
        }

        /// Sample the PHC against the system clock.
        ///
        /// Reads system, PHC, system and takes the midpoint of the two system
        /// readings, so the unavoidable read latency is split rather than
        /// charged entirely to one side. `PTP_SYS_OFFSET_PRECISE` does better
        /// where hardware supports it; this works everywhere.
        pub fn sample(&self) -> Result<RefclockSample, ClockError> {
            let clock = crate::SystemClock;
            let before = clock.wall_ns()? as f64 * 1e-9;
            let phc = self.read_s()?;
            let after = clock.wall_ns()? as f64 * 1e-9;
            let midpoint = (before + after) / 2.0;
            Ok(RefclockSample {
                local_s: midpoint,
                offset_s: phc - midpoint,
                // Half the read window is the honest precision claim.
                precision_log2: ((after - before).max(1e-9).log2().ceil() as i32).clamp(-32, 0)
                    as i8,
                leap: LeapWarning::None,
            })
        }
    }

    use crate::ClockRead;
}

#[cfg(test)]
#[path = "refclock_tests.rs"]
mod refclock_tests;
