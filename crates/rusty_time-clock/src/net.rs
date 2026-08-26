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

/// One received datagram: how many bytes, from whom, and — where the kernel
/// provides it — when it actually arrived.
#[derive(Clone, Copy, Debug)]
pub struct Received {
    pub len: usize,
    pub peer: std::net::SocketAddr,
    /// Kernel receive timestamp in Unix seconds, when `SO_TIMESTAMPING` is
    /// enabled and the stack supplied one.
    ///
    /// This is the timestamp that matters for accuracy: it is taken when the
    /// packet reaches the kernel, not when userspace happens to be scheduled
    /// to read it. The gap between those two is scheduling latency, and on a
    /// busy machine it is the single largest error in an otherwise good
    /// exchange.
    pub kernel_rx_s: Option<f64>,
}

/// Ask the kernel to attach receive timestamps to datagrams on this socket.
///
/// Requests hardware timestamps too; the kernel simply does not supply them
/// where the NIC cannot, and `Received::kernel_rx_s` is `None` then. Failure is
/// not fatal — the caller falls back to reading the clock after `recv`, which
/// is what every implementation did before this existed.
#[cfg(target_os = "linux")]
pub fn enable_rx_timestamps(socket: &UdpSocket) -> Result<(), ClockError> {
    use std::os::fd::AsRawFd;

    // SOF_TIMESTAMPING_* bits. Named here rather than pulled from a binding
    // crate because the set we want is small and fixed.
    const RX_HARDWARE: libc::c_int = 1 << 0;
    const RX_SOFTWARE: libc::c_int = 1 << 3;
    const SOFTWARE: libc::c_int = 1 << 4;
    const RAW_HARDWARE: libc::c_int = 1 << 6;

    let flags: libc::c_int = RX_SOFTWARE | SOFTWARE | RX_HARDWARE | RAW_HARDWARE;
    // SAFETY: setsockopt with a valid fd, level, name, and a pointer to an
    // int of the declared length.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TIMESTAMPING,
            (&raw const flags).cast::<libc::c_void>(),
            core::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(ClockError {
            op: "setsockopt(SO_TIMESTAMPING)",
            detail: std::io::Error::last_os_error().to_string(),
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn enable_rx_timestamps(_socket: &UdpSocket) -> Result<(), ClockError> {
    // Windows has SIO_TIMESTAMPING (Win10 2004+) and macOS has SO_TIMESTAMP;
    // neither is wired yet, and the caller's fallback is correct meanwhile.
    Err(ClockError {
        op: "enable_rx_timestamps",
        detail: "kernel receive timestamps are not implemented on this platform".into(),
    })
}

/// Adopt a listening socket the service manager already opened, if there is one.
///
/// systemd's socket activation passes descriptors starting at fd 3, which lets
/// the socket be bound by systemd (as root, on port 123) while the daemon runs
/// unprivileged — the only thing rtimed would otherwise need root for besides
/// the clock itself.
///
/// Lives in this crate rather than the daemon because adopting a raw descriptor
/// is a platform-seam operation, and this is the one crate permitted `unsafe`.
pub fn activated_udp_socket() -> Option<UdpSocket> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::FromRawFd;

        if !should_adopt_activated(
            std::env::var("LISTEN_PID").ok().as_deref(),
            std::env::var("LISTEN_FDS").ok().as_deref(),
            std::process::id(),
        ) {
            return None;
        }
        // SAFETY: fd 3 is the first activated descriptor per systemd's
        // protocol, and the check above confirmed the set was passed to this
        // process. Ownership is taken exactly once, here.
        Some(unsafe { UdpSocket::from_raw_fd(3) })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Should we adopt the activated descriptor set?
///
/// Pure, so the decision is tested with explicit inputs rather than by mutating
/// the process environment — which is `unsafe` in edition 2024 and races with
/// every other test in the binary.
///
/// `LISTEN_PID` is the load-bearing check: a descriptor inherited from a parent
/// sits at fd 3 just as an activated one does, and adopting it would hijack
/// whatever the parent had open there.
pub fn should_adopt_activated(
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
    our_pid: u32,
) -> bool {
    let for_us = listen_pid
        .and_then(|p| p.parse::<u32>().ok())
        .map(|pid| pid == our_pid)
        .unwrap_or(false);
    let count = listen_fds.and_then(|n| n.parse::<i32>().ok()).unwrap_or(0);
    for_us && count >= 1
}

/// Datagrams a single batch may collect. Past a few dozen the syscall saving
/// per message is already amortised, and the buffer stops fitting in cache.
pub const BATCH_SIZE: usize = 32;

/// Receive up to `bufs.len()` datagrams in **one** syscall on Linux
/// (`recvmmsg`), falling back to a single `recv_from` elsewhere.
///
/// This is the server-throughput lever the mission plan calls out: a
/// one-request-per-syscall server spends most of its CPU crossing the kernel
/// boundary, not answering NTP. The scalar path is kept as the oracle — both
/// must produce identical results, and the batch tests assert that.
#[cfg(target_os = "linux")]
pub fn recv_batch(
    socket: &UdpSocket,
    bufs: &mut [[u8; 1024]],
    out: &mut Vec<Received>,
) -> Result<usize, ClockError> {
    use std::os::fd::AsRawFd;

    out.clear();
    let count = bufs.len().min(BATCH_SIZE);
    if count == 0 {
        return Ok(0);
    }

    let mut iovecs: Vec<libc::iovec> = Vec::with_capacity(count);
    let mut addrs: Vec<libc::sockaddr_storage> = vec![unsafe { core::mem::zeroed() }; count];
    let mut msgs: Vec<libc::mmsghdr> = Vec::with_capacity(count);
    // Room for the kernel's SCM_TIMESTAMPING control message per datagram.
    // Requested unconditionally: if timestamping was never enabled the kernel
    // simply writes no control data and the field stays None.
    let mut controls: Vec<[u8; CONTROL_LEN]> = vec![[0u8; CONTROL_LEN]; count];

    for (i, buf) in bufs.iter_mut().take(count).enumerate() {
        iovecs.push(libc::iovec {
            iov_base: buf.as_mut_ptr().cast::<libc::c_void>(),
            iov_len: buf.len(),
        });
        let _ = i;
    }
    for (i, addr) in addrs.iter_mut().enumerate().take(count) {
        let mut hdr: libc::mmsghdr = unsafe { core::mem::zeroed() };
        hdr.msg_hdr.msg_name = (addr as *mut libc::sockaddr_storage).cast::<libc::c_void>();
        hdr.msg_hdr.msg_namelen = core::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        hdr.msg_hdr.msg_iov = iovecs[i..].as_ptr() as *mut libc::iovec;
        hdr.msg_hdr.msg_iovlen = 1;
        hdr.msg_hdr.msg_control = controls[i].as_mut_ptr().cast::<libc::c_void>();
        hdr.msg_hdr.msg_controllen = CONTROL_LEN as _;
        msgs.push(hdr);
    }

    // SAFETY: msgs is a valid array of `count` mmsghdr, each pointing at a
    // live iovec and sockaddr_storage owned by this frame for the duration of
    // the call. MSG_DONTWAIT keeps this non-blocking; readiness is the
    // caller's job.
    let received = unsafe {
        libc::recvmmsg(
            socket.as_raw_fd(),
            msgs.as_mut_ptr(),
            count as libc::c_uint,
            // glibc and musl disagree on this parameter's integer type, so let
            // the cast take whichever the target's signature declares.
            libc::MSG_DONTWAIT as _,
            core::ptr::null_mut(),
        )
    };
    if received < 0 {
        let err = std::io::Error::last_os_error();
        if matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) {
            return Ok(0);
        }
        return Err(ClockError {
            op: "recvmmsg",
            detail: err.to_string(),
        });
    }

    for msg in msgs.iter().take(received as usize) {
        let len = msg.msg_len as usize;
        // SAFETY: the kernel filled msg_name with a sockaddr of msg_namelen
        // bytes; we only read the family and the matching variant.
        let peer =
            unsafe { sockaddr_to_rust(&*(msg.msg_hdr.msg_name as *const libc::sockaddr_storage)) };
        if let Some(peer) = peer {
            // SAFETY: the kernel filled msg_control with msg_controllen bytes
            // of well-formed cmsgs, or set the length to zero.
            let kernel_rx_s = unsafe { kernel_timestamp(&msg.msg_hdr) };
            out.push(Received {
                len,
                peer,
                kernel_rx_s,
            });
        }
    }
    Ok(out.len())
}

/// Bytes reserved per datagram for control messages. One SCM_TIMESTAMPING
/// carries three timespecs; this leaves room for it and a little slack.
#[cfg(target_os = "linux")]
const CONTROL_LEN: usize = 128;

/// Pull the receive timestamp out of a message's control data.
///
/// `SCM_TIMESTAMPING` carries three timespecs: [0] software, [1] legacy
/// hardware (deprecated), [2] raw hardware. Software is preferred when
/// present because it is already on the system timescale; the raw hardware
/// stamp is on the NIC's own timescale and needs a PHC correlation before it
/// means anything, which is why it is not simply used when available.
///
/// # Safety
/// `hdr` must be a msghdr the kernel has just filled, with `msg_control`
/// pointing at `msg_controllen` valid bytes.
#[cfg(target_os = "linux")]
unsafe fn kernel_timestamp(hdr: &libc::msghdr) -> Option<f64> {
    if hdr.msg_controllen == 0 || hdr.msg_control.is_null() {
        return None;
    }
    // SAFETY: caller's contract; CMSG_FIRSTHDR handles an empty buffer.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(hdr) };
    while !cmsg.is_null() {
        // SAFETY: cmsg came from CMSG_FIRSTHDR/CMSG_NXTHDR and is in range.
        let header = unsafe { &*cmsg };
        if header.cmsg_level == libc::SOL_SOCKET && header.cmsg_type == libc::SCM_TIMESTAMPING {
            // SAFETY: SCM_TIMESTAMPING data is three timespecs.
            let stamps = unsafe { libc::CMSG_DATA(cmsg).cast::<libc::timespec>() };
            for index in [0usize, 2] {
                // SAFETY: index is within the three-element array above.
                let ts = unsafe { *stamps.add(index) };
                if ts.tv_sec != 0 || ts.tv_nsec != 0 {
                    return Some(ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9);
                }
            }
        }
        // SAFETY: as above.
        cmsg = unsafe { libc::CMSG_NXTHDR(hdr, cmsg) };
    }
    None
}

#[cfg(target_os = "linux")]
fn sockaddr_to_rust(storage: &libc::sockaddr_storage) -> Option<std::net::SocketAddr> {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            // SAFETY: family says this is a sockaddr_in.
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            Some(std::net::SocketAddr::V4(SocketAddrV4::new(
                ip,
                u16::from_be(sin.sin_port),
            )))
        }
        libc::AF_INET6 => {
            // SAFETY: family says this is a sockaddr_in6.
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            Some(std::net::SocketAddr::V6(SocketAddrV6::new(
                ip,
                u16::from_be(sin6.sin6_port),
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        _ => None,
    }
}

/// Non-Linux fallback: one datagram per call, same shape as the batch path so
/// the server loop above it is identical on every platform.
#[cfg(not(target_os = "linux"))]
pub fn recv_batch(
    socket: &UdpSocket,
    bufs: &mut [[u8; 1024]],
    out: &mut Vec<Received>,
) -> Result<usize, ClockError> {
    out.clear();
    let Some(buf) = bufs.first_mut() else {
        return Ok(0);
    };
    match socket.recv_from(buf) {
        Ok((len, peer)) => {
            out.push(Received {
                len,
                peer,
                kernel_rx_s: None,
            });
            Ok(1)
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(0)
        }
        Err(e) => Err(ClockError {
            op: "recv_from",
            detail: e.to_string(),
        }),
    }
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

    #[test]
    fn batch_receive_returns_every_datagram_with_its_sender() {
        let server = UdpSocket::bind("127.0.0.1:0").expect("bind server");
        let server_addr = server.local_addr().expect("addr");
        server
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("timeout");

        let client = UdpSocket::bind("127.0.0.1:0").expect("bind client");
        let client_addr = client.local_addr().expect("client addr");
        let sent = 5usize;
        for i in 0..sent {
            client.send_to(&[i as u8; 16], server_addr).expect("send");
        }

        // Collect until we have them all or we stop making progress: the batch
        // path may return fewer than were sent in one call.
        let mut bufs = [[0u8; 1024]; BATCH_SIZE];
        let mut out = Vec::new();
        let mut total = 0usize;
        let mut idle = 0;
        while total < sent && idle < 20 {
            if !wait_readable(&server, Duration::from_millis(200)).expect("wait") {
                idle += 1;
                continue;
            }
            let n = recv_batch(&server, &mut bufs, &mut out).expect("batch");
            if n == 0 {
                idle += 1;
                continue;
            }
            for received in out.iter().take(n) {
                assert_eq!(received.len, 16, "wrong length reported");
                assert_eq!(
                    received.peer, client_addr,
                    "batch reported the wrong sender"
                );
            }
            total += n;
        }
        assert_eq!(total, sent, "batch receive lost datagrams");
    }

    #[test]
    fn activation_is_refused_unless_the_descriptors_are_ours() {
        // The happy path: systemd names us and passes one descriptor.
        assert!(should_adopt_activated(Some("42"), Some("1"), 42));
        // A descriptor set meant for a different process must never be
        // adopted — fd 3 inherited from a parent looks identical otherwise.
        assert!(!should_adopt_activated(Some("41"), Some("1"), 42));
        // No environment at all: the ordinary case, run from a shell.
        assert!(!should_adopt_activated(None, None, 42));
        // Named but nothing passed.
        assert!(!should_adopt_activated(Some("42"), Some("0"), 42));
        // Garbage must not be read as consent.
        assert!(!should_adopt_activated(Some("not-a-pid"), Some("1"), 42));
        assert!(!should_adopt_activated(Some("42"), Some("many"), 42));
    }

    #[test]
    fn batch_receive_on_an_empty_socket_returns_zero_not_an_error() {
        let s = UdpSocket::bind("127.0.0.1:0").expect("bind");
        s.set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout");
        let mut bufs = [[0u8; 1024]; 4];
        let mut out = Vec::new();
        assert_eq!(recv_batch(&s, &mut bufs, &mut out).expect("batch"), 0);
        assert!(out.is_empty());
    }
}
