//! rusty_time-wasm — the wasm virtual clock.
//!
//! No UDP, no OS clock: the page (or edge function) feeds this clock exchange
//! results obtained through its transport (WebTransport gateway at M6), and reads
//! corrected time out. Milliseconds at the boundary because that is what
//! `performance.now()` and `Date.now()` speak; seconds inside.
//!
//! This crate must always compile for `wasm32-unknown-unknown` — it is the CI
//! tripwire that keeps `rusty_time-core` wasm-clean. wasm-bindgen bindings and
//! npm packaging land at M6.

use rusty_time_core::VirtualClock;

#[derive(Debug, Default)]
pub struct WasmClock {
    inner: VirtualClock,
}

impl WasmClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a completed time exchange.
    ///
    /// * `perf_now_ms` — `performance.now()` at processing time.
    /// * `offset_ms` — milliseconds to ADD to the host's `Date.now()`.
    /// * `error_bound_ms` — measurement error bound.
    pub fn update(&mut self, perf_now_ms: f64, offset_ms: f64, error_bound_ms: f64) {
        self.inner.update(
            perf_now_ms / 1e3,
            offset_ms / 1e3,
            None,
            error_bound_ms / 1e3,
        );
    }

    /// Corrected Unix time in milliseconds, given the host's current
    /// `performance.now()` and `Date.now()` readings.
    pub fn now_ms(&mut self, perf_now_ms: f64, date_now_ms: f64) -> f64 {
        self.inner.now(perf_now_ms / 1e3, date_now_ms / 1e3) * 1e3
    }

    /// Current error bound in milliseconds — `Infinity` until first update. Mesh
    /// apps gate CRDT/capability decisions on this.
    pub fn confidence_ms(&self, perf_now_ms: f64) -> f64 {
        self.inner.confidence(perf_now_ms / 1e3) * 1e3
    }

    pub fn is_synchronized(&self) -> bool {
        self.inner.is_synchronized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrects_the_host_clock() {
        let mut c = WasmClock::new();
        assert!(!c.is_synchronized());
        c.update(1000.0, 250.0, 5.0); // host clock 250 ms behind
        let t = c.now_ms(1000.0, 1_700_000_000_000.0);
        assert!((t - 1_700_000_000_250.0).abs() < 1e-6);
        assert!(c.confidence_ms(1000.0) >= 5.0);
    }
}
