//! rusty_time-wasm — disciplined time for the browser and the edge.
//!
//! This is the capability chrony cannot follow us into. There is no OS clock to
//! adjust here and no UDP socket to use, so instead of *setting* time this
//! crate *estimates* it: the page keeps calling `now_ms()` and gets a corrected
//! reading, with an honest error bound attached.
//!
//! **The split is deliberate.** JavaScript owns the transport — `fetch`,
//! WebTransport, a worker, whatever the page has — and this crate owns the time
//! math. That keeps the protocol identical to the native client (the same
//! packet codec, the same regression filter, both already fuzzed) and leaves
//! the one part that genuinely differs per environment in the environment's own
//! hands.
//!
//! The wire format is a real NTPv4 packet, not a JSON shape invented for the
//! browser: one codec, one set of tests, and a gateway that is an NTP server
//! with an HTTP door on it.

use rusty_time_core::ntp::{self, HEADER_LEN, LeapIndicator, Mode, NtpPacket, NtpTimestamp};
use rusty_time_core::{Sample, SampleRegister, VirtualClock};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Milliseconds of extra error we attribute to a browser exchange beyond the
/// measured delay — the page's timers are coarse and its scheduling is not
/// ours to control, so the bound stays honest about that.
const BROWSER_ERROR_FLOOR_MS: f64 = 1.0;

/// How many samples the register keeps. Small: a browser tab is not a daemon
/// and should not hold a long history it will never use.
const REGISTER_CAPACITY: usize = 16;

/// A disciplined view of time for a page or an edge function.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct TimeClient {
    register: SampleRegister,
    vclock: VirtualClock,
    /// Wall-clock time (ms) when the in-flight request was built.
    pending_t1_ms: Option<f64>,
    /// The nonce we put in that request's transmit field, echoed back as
    /// origin — the only spoofing check an unauthenticated exchange has.
    pending_nonce: NtpTimestamp,
    /// Monotonic reading paired with the last accepted sample.
    last_mono_ms: f64,
    accepted: u32,
    rejected: u32,
}

impl Default for TimeClient {
    fn default() -> Self {
        Self::new_inner()
    }
}

impl TimeClient {
    fn new_inner() -> Self {
        TimeClient {
            register: SampleRegister::new(REGISTER_CAPACITY),
            vclock: VirtualClock::new(),
            pending_t1_ms: None,
            pending_nonce: NtpTimestamp::ZERO,
            last_mono_ms: 0.0,
            accepted: 0,
            rejected: 0,
        }
    }

    /// Build a client request. `date_now_ms` is the page's `Date.now()`, and
    /// `nonce` must be unpredictable — it is echoed back as the origin
    /// timestamp and is what stops an off-path attacker forging a reply.
    pub fn build_request_with_nonce(&mut self, date_now_ms: f64, nonce: u64) -> Vec<u8> {
        let nonce = NtpTimestamp(nonce);
        self.pending_nonce = nonce;
        self.pending_t1_ms = Some(date_now_ms);
        NtpPacket::client_request(4, nonce).to_bytes().to_vec()
    }

    /// Feed a gateway response. Returns true if it was accepted.
    ///
    /// `date_now_ms` is `Date.now()` at receipt (T4) and `perf_now_ms` is
    /// `performance.now()`, which is monotonic and therefore what the estimate
    /// is anchored to — `Date.now()` can jump under us at any moment.
    pub fn process_response_inner(
        &mut self,
        bytes: &[u8],
        date_now_ms: f64,
        perf_now_ms: f64,
    ) -> bool {
        let Some(t1_ms) = self.pending_t1_ms else {
            self.rejected += 1;
            return false;
        };
        if bytes.len() < HEADER_LEN {
            self.rejected += 1;
            return false;
        }
        let Ok(packet) = NtpPacket::parse(bytes) else {
            self.rejected += 1;
            return false;
        };

        // Everything that makes a reply *ours* and *usable*, before any of its
        // numbers are allowed to influence the clock.
        let usable = packet.mode == Mode::Server
            && packet.origin_ts == self.pending_nonce
            && packet.stratum != 0
            && packet.stratum <= 15
            && packet.leap != LeapIndicator::Unsynchronized
            && !packet.receive_ts.is_zero()
            && !packet.transmit_ts.is_zero();
        if !usable {
            self.rejected += 1;
            return false;
        }
        self.pending_t1_ms = None;

        // Resolve the server's timestamps near our own reading, so the NTP era
        // is unambiguous without the page having to know what an era is.
        let t1 = t1_ms / 1e3;
        let t4 = date_now_ms / 1e3;
        let t2 = ntp_to_unix_near(packet.receive_ts, t1);
        let t3 = ntp_to_unix_near(packet.transmit_ts, t4);
        let (offset, delay) = ntp::offset_delay(t1, t2, t3, t4);
        if delay < 0.0 {
            // Non-causal: the exchange cannot have happened this way.
            self.rejected += 1;
            return false;
        }

        let mono_s = perf_now_ms / 1e3;
        self.register.push(Sample {
            t: mono_s,
            offset,
            delay,
            dispersion: packet.root_dispersion.to_seconds(),
        });
        self.last_mono_ms = perf_now_ms;
        self.accepted += 1;

        // Prefer the regression once it has enough spread; fall back to the
        // lowest-delay sample, which is the least contaminated single reading.
        let (best_offset, error_s) = match self.register.regress(mono_s) {
            Some(est) => (
                est.offset,
                est.offset_sd.max(delay / 2.0) + BROWSER_ERROR_FLOOR_MS / 1e3,
            ),
            None => (offset, delay / 2.0 + BROWSER_ERROR_FLOOR_MS / 1e3),
        };
        self.vclock.update(mono_s, best_offset, None, error_s);
        true
    }
}

/// Resolve an NTP timestamp into Unix seconds near a pivot (era disambiguation).
fn ntp_to_unix_near(ts: NtpTimestamp, pivot_unix_s: f64) -> f64 {
    const ERA: f64 = 4_294_967_296.0;
    let base = ts.seconds() as f64 - ntp::UNIX_EPOCH_OFFSET as f64 + ts.fraction() as f64 / ERA;
    let mut best = base;
    let mut best_dist = (base - pivot_unix_s).abs();
    for k in [-1.0f64, 1.0] {
        let candidate = base + k * ERA;
        let distance = (candidate - pivot_unix_s).abs();
        if distance < best_dist {
            best = candidate;
            best_dist = distance;
        }
    }
    best
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl TimeClient {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(constructor))]
    pub fn new() -> TimeClient {
        TimeClient::new_inner()
    }

    /// Build a request, drawing the nonce from the caller.
    ///
    /// Exposed this way because the page has `crypto.getRandomValues` and this
    /// crate should not carry a random source into wasm just to duplicate it.
    pub fn build_request(&mut self, date_now_ms: f64, nonce_hi: u32, nonce_lo: u32) -> Vec<u8> {
        let nonce = ((nonce_hi as u64) << 32) | nonce_lo as u64;
        self.build_request_with_nonce(date_now_ms, nonce)
    }

    /// Feed a gateway response; returns true if it was accepted.
    pub fn process_response(&mut self, bytes: &[u8], date_now_ms: f64, perf_now_ms: f64) -> bool {
        self.process_response_inner(bytes, date_now_ms, perf_now_ms)
    }

    /// Corrected Unix time in milliseconds. Never steps backwards.
    pub fn now_ms(&mut self, date_now_ms: f64, perf_now_ms: f64) -> f64 {
        self.vclock.now(perf_now_ms / 1e3, date_now_ms / 1e3) * 1e3
    }

    /// Current error bound in milliseconds; `Infinity` until the first
    /// accepted exchange. Mesh apps gate CRDT and capability decisions on this
    /// rather than assuming the clock is good.
    pub fn confidence_ms(&self, perf_now_ms: f64) -> f64 {
        self.vclock.confidence(perf_now_ms / 1e3) * 1e3
    }

    /// The correction currently applied, in milliseconds.
    pub fn offset_ms(&mut self, perf_now_ms: f64) -> f64 {
        let mono_s = perf_now_ms / 1e3;
        self.register
            .regress(mono_s)
            .map(|e| e.offset * 1e3)
            .or_else(|| self.register.best().map(|b| b.offset * 1e3))
            .unwrap_or(0.0)
    }

    pub fn is_synchronized(&self) -> bool {
        self.vclock.is_synchronized()
    }

    pub fn accepted(&self) -> u32 {
        self.accepted
    }

    pub fn rejected(&self) -> u32 {
        self.rejected
    }
}

#[cfg(test)]
mod tests;
