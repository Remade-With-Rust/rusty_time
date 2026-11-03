//! Stopping on SIGTERM and SIGINT, including as the init process of a PID
//! namespace.
//!
//! Without a handler a process relies on the signal's default action, and the
//! kernel does not apply a default action to the init process of a PID
//! namespace: a signal sent from outside reaches it only if it has installed a
//! handler. OpenWrt's procd runs jailed services exactly that way (ujail puts
//! the service in its own PID namespace), and so does a container with no init
//! of its own. There `rtimed` ignored SIGTERM, and procd killed it with SIGKILL
//! after five seconds on every stop and restart. Installing a handler restores
//! the stop everywhere, and changes nothing where the default action already
//! worked.
//!
//! Lives in this crate because installing a handler is a syscall, and this is
//! the one crate permitted `unsafe`.

use std::sync::atomic::{AtomicBool, Ordering};

/// What a stop signal does once a handler is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopMode {
    /// Only record the request, for a loop that polls [`stop_requested`] and
    /// returns normally.
    Flag,
    /// End the process at once, with status 0, as the default action would
    /// have (for modes whose threads block in receive and have no such loop).
    Exit,
}

static STOP: AtomicBool = AtomicBool::new(false);

/// Whether SIGTERM or SIGINT has arrived since [`on_stop_signal`].
pub fn stop_requested() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// Install the SIGTERM and SIGINT handler. A no-op where there are no POSIX
/// signals.
pub fn on_stop_signal(mode: StopMode) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let handler: extern "C" fn(libc::c_int) = match mode {
            StopMode::Flag => record_stop,
            StopMode::Exit => exit_now,
        };
        for signal in [libc::SIGTERM, libc::SIGINT] {
            // SAFETY: an all-zero `sigaction` is a valid value of the type (no
            // flags, empty mask before `sigemptyset`, null restorer). The
            // handler does only async-signal-safe work: an atomic store, or
            // `_exit`. Replacing the disposition of these two signals affects
            // nothing else this process relies on.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handler as usize;
                libc::sigemptyset(&mut action.sa_mask);
                action.sa_flags = libc::SA_RESTART;
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = mode;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
extern "C" fn record_stop(_signal: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
extern "C" fn exit_now(_signal: libc::c_int) {
    // SAFETY: `_exit` is async-signal-safe and does not return.
    unsafe { libc::_exit(0) }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn a_stop_signal_sets_the_flag_instead_of_ending_the_process() {
        on_stop_signal(StopMode::Flag);
        assert!(!stop_requested());
        // SAFETY: `raise` delivers SIGTERM to this thread; the handler just
        // installed records it, so the test process survives.
        unsafe {
            libc::raise(libc::SIGTERM);
        }
        assert!(stop_requested());
    }
}
