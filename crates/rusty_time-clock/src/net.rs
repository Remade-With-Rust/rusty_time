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
    /// Kernel receive time, in whole NANOSECONDS since the Unix epoch.
    ///
    /// Integer, not seconds-as-f64. The kernel hands over an exact `timespec`;
    /// folding it into an f64 of seconds-since-1970 rounds it to the 238 ns
    /// gap between representable values at today's epoch, and to 477 ns once
    /// Unix time crosses 2^31 in February 2038. The wire resolution this
    /// timestamp is compared against is 2^-32 s — 0.233 ns — so that
    /// conversion was discarding three orders of magnitude before any
    /// arithmetic happened.
    pub kernel_rx_ns: Option<i64>,
}

/// Ask the kernel to attach receive timestamps to datagrams on this socket.
///
/// Requests hardware timestamps too; the kernel simply does not supply them
/// where the NIC cannot, and `Received::kernel_rx_ns` is `None` then. Failure is
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

/// Reusable working memory for `recv_batch`.
///
/// The kernel needs an array of headers, one address slot and one control
/// block per datagram. None of it carries information between calls -- it is
/// all overwritten by the next `recvmmsg` -- so it is allocated once by the
/// caller and reused, exactly as the packet buffers already are, instead of
/// being built and thrown away on every receive.
#[cfg(target_os = "linux")]
#[derive(Default)]
pub struct BatchScratch {
    /// Whether to ask the kernel for control data (receive timestamps).
    ///
    /// Only worth paying for if the caller reads `Received::kernel_rx_ns`. The
    /// server does not — it stamps the batch itself — so it asks for none, and
    /// the kernel skips the control-message machinery for every datagram.
    want_control: bool,
    iovecs: Vec<libc::iovec>,
    addrs: Vec<libc::sockaddr_storage>,
    msgs: Vec<libc::mmsghdr>,
    controls: Vec<[u8; CONTROL_LEN]>,
}

#[cfg(target_os = "linux")]
impl BatchScratch {
    /// Scratch that collects kernel receive timestamps.
    pub fn new() -> Self {
        let mut scratch = BatchScratch {
            want_control: true,
            ..BatchScratch::default()
        };
        scratch.ensure(BATCH_SIZE);
        scratch
    }

    /// Scratch for a caller that does not read `Received::kernel_rx_ns`.
    pub fn without_timestamps() -> Self {
        let mut scratch = BatchScratch::default();
        scratch.ensure(BATCH_SIZE);
        scratch
    }

    /// Grow to hold `count` datagrams. After the first call this does nothing.
    fn ensure(&mut self, count: usize) {
        if self.msgs.len() >= count {
            return;
        }
        // SAFETY: every one of these is a plain-old-data C struct for which an
        // all-zero bit pattern is a valid, inert value; each is fully
        // initialised before the kernel sees it.
        self.iovecs
            .resize(count, unsafe { core::mem::zeroed::<libc::iovec>() });
        self.addrs.resize(count, unsafe {
            core::mem::zeroed::<libc::sockaddr_storage>()
        });
        self.msgs
            .resize(count, unsafe { core::mem::zeroed::<libc::mmsghdr>() });
        self.controls.resize(count, [0u8; CONTROL_LEN]);
    }
}

/// Non-Linux placeholder, so callers have one shape on every platform.
#[cfg(not(target_os = "linux"))]
#[derive(Default)]
pub struct BatchScratch;

#[cfg(not(target_os = "linux"))]
impl BatchScratch {
    pub fn new() -> Self {
        BatchScratch
    }

    /// Same shape as the Linux constructor. There is no control-message
    /// machinery to decline here, so the two are identical — the method exists
    /// so callers compile unchanged on every platform.
    pub fn without_timestamps() -> Self {
        BatchScratch
    }
}

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
    scratch: &mut BatchScratch,
    out: &mut Vec<Received>,
) -> Result<usize, ClockError> {
    use std::os::fd::AsRawFd;

    out.clear();
    let count = bufs.len().min(BATCH_SIZE);
    if count == 0 {
        return Ok(0);
    }
    scratch.ensure(count);

    // Point each header at this call's buffers and reset the two lengths the
    // kernel writes back. Nothing is allocated and nothing is zeroed: the
    // kernel reports how many bytes it put in the address and the control
    // block, so stale bytes beyond those lengths are never read.
    //
    // The previous form built all four arrays per call -- two of them with
    // `vec![zeroed; 32]`, which is ~4 KiB of sockaddr and 4 KiB of control
    // buffer memset on every `recvmmsg`, plus four allocations -- to hand the
    // kernel scratch space it immediately overwrites.
    let BatchScratch {
        want_control,
        iovecs,
        addrs,
        msgs,
        controls,
    } = scratch;
    for i in 0..count {
        iovecs[i] = libc::iovec {
            iov_base: bufs[i].as_mut_ptr().cast::<libc::c_void>(),
            iov_len: bufs[i].len(),
        };
    }
    for i in 0..count {
        let hdr = &mut msgs[i].msg_hdr;
        hdr.msg_name = (&mut addrs[i] as *mut libc::sockaddr_storage).cast::<libc::c_void>();
        hdr.msg_namelen = core::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        hdr.msg_iov = (&mut iovecs[i]) as *mut libc::iovec;
        hdr.msg_iovlen = 1;
        if *want_control {
            hdr.msg_control = controls[i].as_mut_ptr().cast::<libc::c_void>();
            hdr.msg_controllen = CONTROL_LEN as _;
        } else {
            hdr.msg_control = core::ptr::null_mut();
            hdr.msg_controllen = 0;
        }
        msgs[i].msg_len = 0;
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
            let kernel_rx_ns = unsafe { kernel_timestamp(&msg.msg_hdr) };
            out.push(Received {
                len,
                peer,
                kernel_rx_ns,
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
unsafe fn kernel_timestamp(hdr: &libc::msghdr) -> Option<i64> {
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
                    // Exact. i64 nanoseconds hold Unix time until the year
                    // 2262; f64 seconds lose 238 ns of it today.
                    return Some(ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64);
                }
            }
        }
        // SAFETY: as above.
        cmsg = unsafe { libc::CMSG_NXTHDR(hdr, cmsg) };
    }
    None
}

#[cfg(target_os = "linux")]
/// Fill a `sockaddr_storage` from a Rust address, returning its length.
#[cfg(target_os = "linux")]
fn rust_to_sockaddr(
    addr: &std::net::SocketAddr,
    out: &mut libc::sockaddr_storage,
) -> libc::socklen_t {
    match addr {
        std::net::SocketAddr::V4(v4) => {
            // SAFETY: sockaddr_storage is sized and aligned for sockaddr_in,
            // which is the whole reason it exists; every field is written.
            let sin = unsafe { &mut *(out as *mut _ as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
            core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
        }
        std::net::SocketAddr::V6(v6) => {
            // SAFETY: as above, for sockaddr_in6.
            let sin6 = unsafe { &mut *(out as *mut _ as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            sin6.sin6_flowinfo = v6.flowinfo();
            sin6.sin6_scope_id = v6.scope_id();
            core::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
        }
    }
}

/// Send many datagrams in **one** syscall on Linux (`sendmmsg`).
///
/// The receive side has been batched since M5; the send side was not, so a
/// server that collected up to 32 requests with one `recvmmsg` then answered
/// them with 32 separate `sendto` calls. Under load the reply path was the
/// syscall-heavy half of the loop.
///
/// It also makes the transmit timestamp *more* honest for interleaved mode
/// rather than less: the packets genuinely do leave in one syscall, so one
/// clock reading taken immediately afterwards describes all of them, where the
/// old loop stamped each reply after its own `sendto` and attributed the
/// accumulated delay of the whole loop to the last client in the batch.
///
/// Returns how many datagrams the kernel accepted.
#[cfg(target_os = "linux")]
pub fn send_batch(
    socket: &UdpSocket,
    messages: &[(&[u8], std::net::SocketAddr)],
    scratch: &mut BatchScratch,
) -> Result<usize, ClockError> {
    send_batch_by(socket, messages, |m| m.0, |m| m.1, scratch)
}

/// `send_batch` reading each datagram out of the caller's own items.
///
/// The slice form needs a `&[(&[u8], SocketAddr)]`, which a caller holding
/// replies in some other shape can only produce by building a temporary vector
/// every batch — an allocation, a copy of every element, and a free, to
/// describe data it already had. Accessors remove it.
#[cfg(target_os = "linux")]
pub fn send_batch_by<T>(
    socket: &UdpSocket,
    items: &[T],
    payload: impl Fn(&T) -> &[u8],
    peer: impl Fn(&T) -> std::net::SocketAddr,
    scratch: &mut BatchScratch,
) -> Result<usize, ClockError> {
    use std::os::fd::AsRawFd;

    let count = items.len().min(BATCH_SIZE);
    if count == 0 {
        return Ok(0);
    }
    scratch.ensure(count);
    let BatchScratch {
        iovecs,
        addrs,
        msgs,
        ..
    } = scratch;

    for (i, item) in items.iter().enumerate().take(count) {
        let bytes = payload(item);
        iovecs[i] = libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        };
        let namelen = rust_to_sockaddr(&peer(item), &mut addrs[i]);
        let hdr = &mut msgs[i].msg_hdr;
        hdr.msg_name = (&mut addrs[i] as *mut libc::sockaddr_storage).cast::<libc::c_void>();
        hdr.msg_namelen = namelen;
        hdr.msg_iov = (&mut iovecs[i]) as *mut libc::iovec;
        hdr.msg_iovlen = 1;
        hdr.msg_control = core::ptr::null_mut();
        hdr.msg_controllen = 0;
        msgs[i].msg_len = 0;
    }

    // SAFETY: msgs is a valid array of `count` mmsghdr, each naming a live
    // iovec and sockaddr owned by `scratch`, which outlives the call. The
    // payloads are borrowed for the duration of `messages`.
    let sent = unsafe {
        libc::sendmmsg(
            socket.as_raw_fd(),
            msgs.as_mut_ptr(),
            count as libc::c_uint,
            libc::MSG_DONTWAIT as _,
        )
    };
    if sent < 0 {
        let err = std::io::Error::last_os_error();
        if matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) {
            return Ok(0);
        }
        return Err(ClockError {
            op: "sendmmsg",
            detail: err.to_string(),
        });
    }
    Ok(sent as usize)
}

/// Non-Linux fallback: one datagram per call, same shape as the batch path.
#[cfg(not(target_os = "linux"))]
pub fn send_batch(
    socket: &UdpSocket,
    messages: &[(&[u8], std::net::SocketAddr)],
    scratch: &mut BatchScratch,
) -> Result<usize, ClockError> {
    send_batch_by(socket, messages, |m| m.0, |m| m.1, scratch)
}

/// Non-Linux fallback: one datagram per call, same shape as the batch path.
#[cfg(not(target_os = "linux"))]
pub fn send_batch_by<T>(
    socket: &UdpSocket,
    items: &[T],
    payload: impl Fn(&T) -> &[u8],
    peer: impl Fn(&T) -> std::net::SocketAddr,
    _scratch: &mut BatchScratch,
) -> Result<usize, ClockError> {
    let mut sent = 0;
    for item in items {
        match socket.send_to(payload(item), peer(item)) {
            Ok(_) => sent += 1,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(e) => {
                return Err(ClockError {
                    op: "send_to",
                    detail: e.to_string(),
                });
            }
        }
    }
    Ok(sent)
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
    _scratch: &mut BatchScratch,
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
                kernel_rx_ns: None,
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
            let n =
                recv_batch(&server, &mut bufs, &mut BatchScratch::new(), &mut out).expect("batch");
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
        assert_eq!(
            recv_batch(&s, &mut bufs, &mut BatchScratch::new(), &mut out).expect("batch"),
            0
        );
        assert!(out.is_empty());
    }
}
