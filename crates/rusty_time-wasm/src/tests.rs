//! Tests for the wasm time client.
//!
//! These run as plain Rust on any host: the logic is portable, only the
//! bindings are wasm-specific. The S2 test below is the mission plan's M6 exit
//! condition expressed as an assertion.

use super::*;
use rusty_time_core::ntp::NtpShort;

/// A simulated gateway sharing one true clock, so the *only* error the client
/// can show is the error its own algorithm introduces.
struct Gateway {
    /// The browser's `Date.now()` is wrong by this much (seconds to add).
    client_clock_error_s: f64,
}

impl Gateway {
    /// Answer a request. `t2`/`t3` are true times at the server.
    fn respond(&self, request: &[u8], t2_unix_s: f64, t3_unix_s: f64) -> Vec<u8> {
        let parsed = NtpPacket::parse(request).expect("valid request");
        let to_ts = |unix_s: f64| {
            NtpTimestamp::from_unix(unix_s.trunc() as i64, (unix_s.fract() * 1e9) as u32)
        };
        NtpPacket {
            leap: LeapIndicator::NoWarning,
            version: 4,
            mode: Mode::Server,
            stratum: 1,
            poll: 4,
            precision: -20,
            root_delay: NtpShort(0),
            root_dispersion: NtpShort::from_seconds(0.0001),
            reference_id: *b"RSTY",
            reference_ts: to_ts(t2_unix_s),
            origin_ts: parsed.transmit_ts,
            receive_ts: to_ts(t2_unix_s),
            transmit_ts: to_ts(t3_unix_s),
        }
        .to_bytes()
        .to_vec()
    }

    /// Run one exchange over a network with the given one-way delays.
    /// Returns the client's corrected time error, in milliseconds.
    fn exchange(
        &self,
        client: &mut TimeClient,
        true_now_s: f64,
        out_delay_s: f64,
        back_delay_s: f64,
        nonce: u64,
    ) {
        // The page reads its own (wrong) clock for T1 and T4.
        let t1_wall_ms = (true_now_s + self.client_clock_error_s) * 1e3;
        let request = client.build_request_with_nonce(t1_wall_ms, nonce);

        let t2 = true_now_s + out_delay_s;
        let t3 = t2 + 50e-6; // server turnaround
        let t4_true = t3 + back_delay_s;
        let t4_wall_ms = (t4_true + self.client_clock_error_s) * 1e3;

        let response = self.respond(&request, t2, t3);
        // performance.now() is monotonic and unaffected by the clock error.
        let perf_ms = true_now_s * 1e3;
        assert!(
            client.process_response_inner(&response, t4_wall_ms, perf_ms),
            "gateway response should be accepted"
        );
    }
}

#[test]
fn a_single_exchange_recovers_a_known_offset() {
    // The page's clock is 250 ms fast; a symmetric 10 ms path.
    let gw = Gateway {
        client_clock_error_s: 0.250,
    };
    let mut client = TimeClient::new();
    assert!(!client.is_synchronized());
    assert!(client.confidence_ms(0.0).is_infinite());

    let now = 1_756_224_000.0;
    gw.exchange(&mut client, now, 0.005, 0.005, 0xABCD);

    assert!(client.is_synchronized());
    // The correction must undo the error: the page is 250 ms fast, so the
    // offset to apply is -250 ms.
    let offset = client.offset_ms(now * 1e3);
    assert!(
        (offset + 250.0).abs() < 1.0,
        "expected about -250 ms, got {offset}"
    );
    // And the corrected reading must land on true time.
    let corrected = client.now_ms((now + gw.client_clock_error_s) * 1e3, now * 1e3);
    assert!(
        (corrected - now * 1e3).abs() < 1.0,
        "corrected time off by {} ms",
        corrected - now * 1e3
    );
}

#[test]
fn s2_network_holds_within_ten_milliseconds() {
    // === The M6 exit condition ===
    //
    // TIMECORP S2: 40 ms round trip with 2:1 path asymmetry. Asymmetry is the
    // hard part — NTP cannot observe it, so it becomes a systematic bias of
    // (out - back) / 2, which here is about 6.7 ms. The gate is that the
    // client stays inside 10 ms, i.e. that the algorithm adds little on top of
    // the bias the network makes unavoidable.
    let rtt = 0.040;
    let out = rtt * 2.0 / 3.0; // 26.7 ms
    let back = rtt / 3.0; // 13.3 ms
    let gw = Gateway {
        client_clock_error_s: 0.180, // the page is 180 ms fast
    };
    let mut client = TimeClient::new();

    // Poll for a few minutes at S2's cadence, with jitter on each leg.
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut jitter = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        // Pareto-ish: usually small, occasionally a spike.
        let u = (rng >> 11) as f64 / (1u64 << 53) as f64;
        if u > 0.9 { 0.020 * u } else { 0.002 * u }
    };

    let start = 1_756_224_000.0;
    let mut worst_ms: f64 = 0.0;
    for i in 0..40 {
        let now = start + i as f64 * 8.0;
        gw.exchange(
            &mut client,
            now,
            out + jitter(),
            back + jitter(),
            0x1000 + i as u64,
        );

        // After a few samples, check the corrected clock continuously.
        if i >= 4 {
            let corrected = client.now_ms((now + gw.client_clock_error_s) * 1e3, now * 1e3);
            let error_ms = (corrected - now * 1e3).abs();
            worst_ms = worst_ms.max(error_ms);
        }
    }

    // Printed so the measured margin lands in CI logs and the ledger, rather
    // than only the fact that a threshold was met.
    println!("S2 gate: worst corrected-clock error {worst_ms:.3} ms (bound 10 ms)");
    assert!(
        worst_ms < 10.0,
        "S2 gate: worst error {worst_ms:.2} ms exceeds the 10 ms bound"
    );
    // The residual should be dominated by the unavoidable asymmetry bias
    // (~6.7 ms), not by the estimator flailing.
    assert!(
        worst_ms > 1.0,
        "suspiciously good ({worst_ms:.2} ms) — asymmetry bias should be visible; \
         is the simulation actually asymmetric?"
    );
}

#[test]
fn confidence_widens_when_exchanges_stop() {
    let gw = Gateway {
        client_clock_error_s: 0.0,
    };
    let mut client = TimeClient::new();
    let now = 1_756_224_000.0;
    gw.exchange(&mut client, now, 0.005, 0.005, 1);

    let fresh = client.confidence_ms(now * 1e3);
    let stale = client.confidence_ms((now + 3600.0) * 1e3);
    assert!(fresh.is_finite());
    assert!(
        stale > fresh,
        "an hour without an exchange must widen the bound ({fresh} -> {stale})"
    );
}

#[test]
fn a_reply_to_someone_elses_request_is_refused() {
    // The origin timestamp is the only binding an unauthenticated exchange
    // has. A reply that does not echo our nonce must not move the clock.
    let gw = Gateway {
        client_clock_error_s: 5.0,
    };
    let mut client = TimeClient::new();
    let now = 1_756_224_000.0;

    let _ = client.build_request_with_nonce(now * 1e3, 0xAAAA);
    // Build a well-formed reply to a *different* request.
    let other = NtpPacket::client_request(4, NtpTimestamp(0xBBBB)).to_bytes();
    let response = gw.respond(&other, now, now);

    assert!(!client.process_response_inner(&response, now * 1e3, now * 1e3));
    assert!(!client.is_synchronized(), "forged reply moved the clock");
    assert_eq!(client.rejected(), 1);
}

#[test]
fn unsynchronized_and_kod_servers_are_refused() {
    let mut client = TimeClient::new();
    let now = 1_756_224_000.0;
    let nonce = 0xCAFE;

    // Stratum 0 is a Kiss-o'-Death, not a time source.
    let _ = client.build_request_with_nonce(now * 1e3, nonce);
    let mut kod = NtpPacket::client_request(4, NtpTimestamp(nonce));
    kod.mode = Mode::Server;
    kod.stratum = 0;
    kod.receive_ts = NtpTimestamp::from_unix(now as i64, 0);
    kod.transmit_ts = kod.receive_ts;
    kod.origin_ts = NtpTimestamp(nonce);
    assert!(!client.process_response_inner(&kod.to_bytes(), now * 1e3, now * 1e3));

    // A server that admits it is unsynchronized must be ignored too.
    let _ = client.build_request_with_nonce(now * 1e3, nonce);
    let mut unsync = kod;
    unsync.stratum = 2;
    unsync.leap = LeapIndicator::Unsynchronized;
    assert!(!client.process_response_inner(&unsync.to_bytes(), now * 1e3, now * 1e3));

    assert!(!client.is_synchronized());
}

#[test]
fn garbage_never_panics_and_never_synchronizes() {
    let mut client = TimeClient::new();
    let mut rng = 0xDEAD_BEEF_CAFE_1234u64;
    for i in 0..20_000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let len = (rng % 120) as usize;
        let mut bytes = vec![0u8; len];
        for b in bytes.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *b = rng as u8;
        }
        let _ = client.build_request_with_nonce(1e12, i);
        let _ = client.process_response_inner(&bytes, 1e12, 1e6);
    }
    assert!(
        !client.is_synchronized(),
        "random bytes must never be accepted as time"
    );
}

#[test]
fn corrected_time_never_steps_backwards() {
    // A page that sees time go backwards will produce out-of-order records,
    // which is precisely what the mesh's CRDTs must not be handed.
    let gw = Gateway {
        client_clock_error_s: 0.0,
    };
    let mut client = TimeClient::new();
    let now = 1_756_224_000.0;
    gw.exchange(&mut client, now, 0.005, 0.005, 1);
    let first = client.now_ms(now * 1e3, now * 1e3);

    // A later exchange says we were 50 ms fast: a naive correction would
    // report an earlier time than we already handed out.
    let gw2 = Gateway {
        client_clock_error_s: 0.050,
    };
    gw2.exchange(&mut client, now + 8.0, 0.005, 0.005, 2);
    let second = client.now_ms((now + 8.0 + 0.050) * 1e3, (now + 8.0) * 1e3);
    assert!(second >= first, "time went backwards: {first} -> {second}");
}
