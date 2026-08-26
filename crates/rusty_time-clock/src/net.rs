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

/// One received datagram: how many bytes, and from whom.
#[derive(Clone, Copy, Debug)]
pub struct Received {
    pub len: usize,
    pub peer: std::net::SocketAddr,
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
            out.push(Received { len, peer });
        }
    }
    Ok(out.len())
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
            out.push(Received { len, peer });
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
