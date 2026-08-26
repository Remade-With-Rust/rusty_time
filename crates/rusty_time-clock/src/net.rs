//! Socket readiness — the poll-before-recv seam.
//!
//! Two reasons this exists, both structural:
//!
//! 1. The daemon's event loop (M3+) multiplexes sockets and timers; readiness
//!    polling is the primitive it is built on.
//! 2. Simulation rigs that virtualize time (clknetsim) advance the simulated
//!    clock while a process blocks in `poll`/`select` — a plain blocking `recv`
//!    returns `EWOULDBLOCK` there and can never see a packet. Discovered by the
//!    TIMECORP interception probe (`tools/corpus/wsl_interception_probe.sh`).
//!
//! On Windows the blocking-`recv` + `SO_RCVTIMEO` path is sound and WSAPoll adds
//! nothing, so `wait_readable` reports ready immediately and the socket timeout
//! governs.

use crate::ClockError;
use std::net::UdpSocket;
use std::time::Duration;

/// Wait until `socket` has data to read, or the timeout passes. Returns
/// `Ok(true)` if readable, `Ok(false)` on timeout.
#[cfg(unix)]
pub fn wait_readable(socket: &UdpSocket, timeout: Duration) -> Result<bool, ClockError> {
    use std::os::fd::AsRawFd;

    let mut pfd = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    loop {
        // SAFETY: pfd is a valid, exclusively-owned pollfd array of length 1 for
        // the duration of the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc > 0 {
            return Ok(pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0);
        }
        if rc == 0 {
            return Ok(false);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(ClockError {
            op: "poll",
            detail: err.to_string(),
        });
    }
}

#[cfg(windows)]
pub fn wait_readable(_socket: &UdpSocket, _timeout: Duration) -> Result<bool, ClockError> {
    // SO_RCVTIMEO on the socket governs; recv itself blocks correctly here.
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_and_readable_paths() {
        let a = UdpSocket::bind("127.0.0.1:0").expect("bind a");
        let b = UdpSocket::bind("127.0.0.1:0").expect("bind b");
        // Nothing pending on Unix: times out quickly. On Windows the shim
        // reports ready by design; recv's own timeout governs there.
        let quick = wait_readable(&a, Duration::from_millis(50)).expect("wait");
        if cfg!(unix) {
            assert!(!quick, "empty socket reported readable");
        } else {
            assert!(quick);
        }
        b.send_to(b"x", a.local_addr().expect("addr"))
            .expect("send");
        let ready = wait_readable(&a, Duration::from_millis(2000)).expect("wait");
        assert!(ready, "pending datagram not reported readable");
    }
}
