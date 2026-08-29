//! The control plane: typed ops over a local socket.
//!
//! Transport is local-only by design. A Unix domain socket (or a Windows named
//! pipe) is reachable by processes on this machine and nothing else, so the
//! authorization question is "which local user", answered by filesystem
//! permissions — not by a password we would have to invent. Remote control
//! arrives later over the SpaceDB transport with an mID token (mission plan
//! §5); the op types are identical, which is the point of defining them in
//! `rusty_time-api` rather than here.
//!
//! Wire framing: one JSON request per line, one JSON response per line.

use crate::server::ServerState;
use rusty_time_api::{
    ClientRow, ControlEndpoint, ControlRequest, ControlResponse, ServerStatsReport,
    TrackingReport,
};
// `Read`/`Write` are reached through the generic bounds on `serve_connection`,
// so they need no import here; `BufRead` is needed for `read_line`.
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};

/// Bound on one request line: a control client has no business sending more,
/// and an unbounded read is a memory lever even from a local peer.
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

/// Where the control socket lives by default. Defined in `rusty_time-api` so
/// the daemon and `rtimec` cannot disagree about it.
pub fn default_path() -> String {
    rusty_time_api::default_control_spec()
}

/// What the discipline loop publishes for the control socket.
///
/// The loop already computes every field on each poll (`ControllerStep` calls
/// them "the estimate that produced the plan, for reporting") — this is where
/// that estimate stops being dropped on the floor.
pub struct TrackingState {
    report: TrackingReport,
    updated: std::time::Instant,
    /// The loop's current poll interval, which sets what "stale" means.
    poll_interval_s: f64,
}

/// Worst-case local oscillator drift used to age a stale estimate, in ppm.
/// Generous on purpose: an error bound that grows too fast only makes a
/// consumer fail closed sooner, which is the safe direction.
const STALE_DRIFT_PPM: f64 = 100.0;

impl TrackingState {
    /// A loop that has not yet produced an accepted estimate. Reported as
    /// unsynchronized with an infinite bound — never as "offset 0", which a
    /// consumer would read as a perfectly good clock.
    pub fn unsynchronized() -> Self {
        Self {
            report: TrackingReport {
                synchronized: false,
                offset_s: 0.0,
                freq_ppm: 0.0,
                error_bound_s: f64::INFINITY,
                poll_log2: 0,
            },
            updated: std::time::Instant::now(),
            poll_interval_s: 64.0,
        }
    }

    /// Record what the controller just decided.
    pub fn publish(
        &mut self,
        synchronized: bool,
        offset_s: f64,
        freq_ppm: f64,
        error_bound_s: f64,
        poll_interval_s: f64,
    ) {
        self.report = TrackingReport {
            synchronized,
            offset_s,
            freq_ppm,
            error_bound_s,
            poll_log2: poll_log2_of(poll_interval_s),
        };
        self.updated = std::time::Instant::now();
        self.poll_interval_s = poll_interval_s.max(1.0);
    }

    /// The report as of now, aged for how long the loop has been quiet.
    ///
    /// A daemon whose loop has stalled still holds a plausible-looking last
    /// estimate; serving it unchanged would let a consumer trust a clock that
    /// nothing is steering. So staleness downgrades `synchronized` and grows
    /// the error bound. (The consumer separately ages its own cache — a
    /// different interval, so no double count.)
    pub fn snapshot(&self) -> TrackingReport {
        let age_s = self.updated.elapsed().as_secs_f64();
        let stale_after_s = (4.0 * self.poll_interval_s).max(64.0);
        let mut r = self.report.clone();
        if age_s > stale_after_s {
            r.synchronized = false;
        }
        r.error_bound_s += age_s * STALE_DRIFT_PPM * 1e-6;
        r
    }
}

fn poll_log2_of(poll_interval_s: f64) -> i8 {
    if !poll_interval_s.is_finite() || poll_interval_s <= 0.0 {
        return 0;
    }
    poll_interval_s.log2().round().clamp(-128.0, 127.0) as i8
}

/// What this daemon can answer about. Both roles are optional and neither is
/// invented: `rtimed serve` has counters and no discipline loop, `rtimed sync`
/// has a discipline loop and no counters, and an op aimed at the role that is
/// not running gets a clear error instead of a fabricated zero.
#[derive(Clone, Default)]
pub struct ControlState {
    pub server: Option<Arc<Mutex<ServerState>>>,
    pub tracking: Option<Arc<Mutex<TrackingState>>>,
}

impl ControlState {
    pub fn for_server(server: Arc<Mutex<ServerState>>) -> Self {
        Self {
            server: Some(server),
            tracking: None,
        }
    }

    pub fn for_sync(tracking: Arc<Mutex<TrackingState>>) -> Self {
        Self {
            server: None,
            tracking: Some(tracking),
        }
    }
}

/// Answer one request against live server state.
///
/// Pure apart from the lock: the tests drive this directly, so op behavior is
/// verified without a socket in the way.
pub fn handle(request: &ControlRequest, state: &ControlState) -> ControlResponse {
    // Liveness answers in either role.
    if matches!(request, ControlRequest::Ping) {
        return ControlResponse::Pong {
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
    }

    // Tracking is the CLIENT role's state; it lives behind its own lock.
    if matches!(request, ControlRequest::Tracking) {
        let Some(tracking) = state.tracking.as_ref() else {
            return ControlResponse::Error {
                message: "this daemon runs no discipline loop; tracking is served by `rtimed sync`"
                    .into(),
            };
        };
        let Ok(guard) = tracking.lock() else {
            return ControlResponse::Error {
                message: "tracking state is poisoned".into(),
            };
        };
        return ControlResponse::Tracking(guard.snapshot());
    }

    // Everything below is the SERVER role's state.
    let Some(server) = state.server.as_ref() else {
        return ControlResponse::Error {
            message: "this daemon runs no server; these ops are served by `rtimed serve`".into(),
        };
    };
    let Ok(guard) = server.lock() else {
        return ControlResponse::Error {
            message: "server state is poisoned".into(),
        };
    };

    match request {
        ControlRequest::Ping | ControlRequest::Tracking => ControlResponse::Error {
            message: "unreachable: handled above".into(),
        },
        ControlRequest::ServerStats => {
            let stats = guard.clients.stats;
            let uptime_s = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().saturating_sub(guard.started_unix))
                .unwrap_or(0);
            ControlResponse::ServerStats(ServerStatsReport {
                ntp_requests: stats.requests,
                ntp_responses: stats.responses,
                dropped_rate_limit: stats.dropped_rate_limit,
                kiss_of_death: stats.kiss_of_death,
                interleaved_responses: stats.interleaved_responses,
                refused: stats.refused,
                clients_tracked: guard.clients.len(),
                clients_evicted: stats.evicted,
                uptime_s,
                stratum: guard.stratum,
            })
        }
        ControlRequest::Clients { limit } => {
            // `last_seen` is monotonic seconds; report an age, which is what an
            // operator can actually interpret.
            let now = rusty_time_clock::SystemClock.mono_s().unwrap_or_default();
            let rows = guard
                .clients
                .most_recent((*limit).min(4096))
                .into_iter()
                .map(|(addr, rec)| ClientRow {
                    address: addr.to_string(),
                    last_seen_ago_s: (now - rec.last_seen).max(0.0),
                    requests: rec.requests,
                    responses: rec.responses,
                    dropped: rec.dropped,
                    // Whether the client is *using* interleaved mode, not
                    // whether we could serve it.
                    interleaved: rec.interleaved_now,
                })
                .collect();
            ControlResponse::Clients { rows }
        }
        ControlRequest::NtsData => ControlResponse::NtsData {
            master_key_ids: guard.ring.key_ids(),
        },
    }
}

use rusty_time_clock::ClockRead;

/// Serve the control plane until the process ends.
///
/// Which transport that is depends on the platform, resolved identically here
/// and in `rtimec` by `rusty_time_api::control_endpoint`.
pub fn serve(spec: &str, state: ControlState) -> Result<(), String> {
    match rusty_time_api::control_endpoint(spec) {
        ControlEndpoint::UnixPath(path) => serve_unix(&path, state),
        ControlEndpoint::Loopback(port) => serve_loopback(port, state),
    }
}

#[cfg(unix)]
fn serve_unix(path: &str, state: ControlState) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    // A stale socket from a crashed run would otherwise block the bind.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| format!("binding {path}: {e}"))?;
    // 0600: the control socket can read the client log, so it is owner-only.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("securing {path}: {e}"))?;

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        std::thread::spawn(move || {
            let Ok(writer) = stream.try_clone() else {
                return;
            };
            let _ = serve_connection(stream, writer, &state);
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn serve_unix(_path: &str, _state: ControlState) -> Result<(), String> {
    Err("unix domain sockets are not available on this platform".to_string())
}

/// Loopback TCP: the Windows transport until a named-pipe server lands.
///
/// Local-only, but it does **not** carry a pipe's SID-based authorization, so
/// any local process can connect. Stated plainly rather than treated as
/// equivalent to a 0600 unix socket.
fn serve_loopback(port: u16, state: ControlState) -> Result<(), String> {
    use std::net::TcpListener;
    let bind = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&bind).map_err(|e| format!("binding {bind}: {e}"))?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        std::thread::spawn(move || {
            let Ok(writer) = stream.try_clone() else {
                return;
            };
            let _ = serve_connection(stream, writer, &state);
        });
    }
    Ok(())
}

/// One request, one response, then the connection closes. Reader and writer
/// are passed separately because the two socket types clone differently
/// (`try_clone` is fallible and belongs at the call site).
fn serve_connection<R, W>(
    read: R,
    mut writer: W,
    state: &ControlState,
) -> Result<(), String>
where
    R: std::io::Read,
    W: std::io::Write,
{
    let mut reader = BufReader::new(read.take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
        return Ok(());
    }
    let response = match serde_json::from_str::<ControlRequest>(line.trim()) {
        Ok(request) => handle(&request, state),
        Err(e) => ControlResponse::Error {
            message: format!("unrecognized request: {e}"),
        },
    };
    let mut body = serde_json::to_string(&response).map_err(|e| e.to_string())?;
    body.push('\n');
    writer
        .write_all(body.as_bytes())
        .map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    Ok(())
}

// The *client* half of this transport lives in `rusty_time-ctl`, so there is
// one implementation of "send an op, read the answer" and the daemon does not
// carry code it never runs.

#[cfg(test)]
mod tests {
    use super::*;
    use rusty_time_core::server::{ClientTable, RateLimitConfig};
    use rusty_time_nts::cookie::{KeyRing, MasterKey};

    fn state() -> ControlState {
        let mut ring = KeyRing::new(3);
        ring.rotate_in(MasterKey {
            id: 0xABCD,
            key: [7; 32],
        });
        ControlState::for_server(Arc::new(Mutex::new(ServerState {
            clients: ClientTable::new(64, RateLimitConfig::default()),
            ring,
            stratum: 2,
            started_unix: 1_000,
        })))
    }

    fn sync_state() -> ControlState {
        ControlState::for_sync(Arc::new(Mutex::new(TrackingState::unsynchronized())))
    }

    #[test]
    fn tracking_starts_unsynchronized_with_an_infinite_bound() {
        // A loop that has not yet accepted a correction must not look like a
        // perfect clock. Offset 0 with a finite bound would read as exactly
        // that, so the initial bound is infinite and the flag is false.
        let t = TrackingState::unsynchronized();
        let r = t.snapshot();
        assert!(!r.synchronized);
        assert!(r.error_bound_s.is_infinite());
    }

    #[test]
    fn publish_then_snapshot_reports_what_the_controller_decided() {
        let mut t = TrackingState::unsynchronized();
        t.publish(true, 0.004, 1.25, 0.02, 64.0);
        let r = t.snapshot();
        assert!(r.synchronized);
        assert!((r.offset_s - 0.004).abs() < 1e-12);
        assert!((r.freq_ppm - 1.25).abs() < 1e-12);
        // Bound is aged from the publish instant, so it is at or above what
        // was published, never below.
        assert!(r.error_bound_s >= 0.02);
        assert!(r.error_bound_s < 0.03);
        assert_eq!(r.poll_log2, 6, "2^6 = 64 s");
    }

    #[test]
    fn a_stalled_loop_stops_claiming_synchronized() {
        // The daemon still holds a plausible last estimate; serving it
        // unchanged would let a consumer trust a clock nothing is steering.
        let mut t = TrackingState::unsynchronized();
        t.publish(true, 0.004, 1.25, 0.02, 16.0);
        assert!(t.snapshot().synchronized);
        t.updated = std::time::Instant::now() - std::time::Duration::from_secs(600);
        let stale = t.snapshot();
        assert!(!stale.synchronized, "a stalled loop must not report synchronized");
        assert!(stale.error_bound_s > 0.02, "and its bound must have grown");
    }

    #[test]
    fn an_op_aimed_at_the_role_that_is_not_running_errors_rather_than_inventing() {
        // Tracking against a serve-only daemon, and server counters against a
        // sync-only daemon. Both must say so; a fabricated zero here is the
        // failure this whole op exists to avoid.
        match handle(&ControlRequest::Tracking, &state()) {
            ControlResponse::Error { message } => assert!(message.contains("rtimed sync")),
            other => panic!("expected an error, got {other:?}"),
        }
        match handle(&ControlRequest::ServerStats, &sync_state()) {
            ControlResponse::Error { message } => assert!(message.contains("rtimed serve")),
            other => panic!("expected an error, got {other:?}"),
        }
        // Liveness answers in either role.
        assert!(matches!(
            handle(&ControlRequest::Ping, &sync_state()),
            ControlResponse::Pong { .. }
        ));
    }

    #[test]
    fn the_tracking_op_answers_from_a_sync_daemon() {
        let s = sync_state();
        s.tracking
            .as_ref()
            .expect("tracking")
            .lock()
            .expect("lock")
            .publish(true, -0.25, 3.5, 0.01, 32.0);
        match handle(&ControlRequest::Tracking, &s) {
            ControlResponse::Tracking(r) => {
                assert!(r.synchronized);
                assert!((r.offset_s + 0.25).abs() < 1e-12);
                assert_eq!(r.poll_log2, 5);
            }
            other => panic!("expected tracking, got {other:?}"),
        }
    }

    /// The wire contract consumers depend on. `mata-time-rtimed` sends
    /// `{"op":"tracking"}` and reads `{"result":"tracking", <fields>}` — the
    /// internally-tagged newtype-over-a-struct shape `ServerStats` already
    /// uses. If this ever stops flattening, every consumer silently falls back
    /// to an undisciplined clock, so pin it here.
    #[test]
    fn the_tracking_wire_shape_is_flat_under_the_result_tag() {
        let req = serde_json::to_string(&ControlRequest::Tracking).expect("encode");
        assert_eq!(req, r#"{"op":"tracking"}"#);

        let report = TrackingReport {
            synchronized: true,
            offset_s: 0.004,
            freq_ppm: 1.25,
            error_bound_s: 0.02,
            poll_log2: 6,
        };
        let resp = ControlResponse::Tracking(report.clone());
        let json = serde_json::to_string(&resp).expect("encode");
        assert!(json.contains(r#""result":"tracking""#), "got {json}");
        assert!(json.contains(r#""synchronized":true"#), "got {json}");
        assert!(json.contains(r#""offset_s":0.004"#), "got {json}");
        assert!(json.contains(r#""error_bound_s":0.02"#), "got {json}");
        // No nesting: the fields sit beside the tag, not under a key.
        assert!(!json.contains(r#""Tracking""#), "got {json}");

        let back: ControlResponse = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, resp);
    }

    #[test]
    fn ping_reports_the_version() {
        let s = state();
        match handle(&ControlRequest::Ping, &s) {
            ControlResponse::Pong { version } => {
                assert_eq!(version, env!("CARGO_PKG_VERSION"));
            }
            other => panic!("expected pong, got {other:?}"),
        }
    }

    #[test]
    fn serverstats_reflects_real_traffic() {
        let s = state();
        {
            let mut guard = s.server.as_ref().expect("server").lock().expect("lock");
            let peer: std::net::IpAddr = "192.0.2.7".parse().expect("addr");
            for _ in 0..12 {
                let _ = guard.clients.admit(&peer, 0.0);
            }
        }
        match handle(&ControlRequest::ServerStats, &s) {
            ControlResponse::ServerStats(r) => {
                assert_eq!(r.ntp_requests, 12);
                assert!(r.ntp_responses > 0 && r.ntp_responses < 12);
                assert!(
                    r.dropped_rate_limit > 0,
                    "rate limiting should show in the stats"
                );
                assert_eq!(r.clients_tracked, 1);
                assert_eq!(r.stratum, 2);
            }
            other => panic!("expected serverstats, got {other:?}"),
        }
    }

    #[test]
    fn clients_op_lists_most_recent_first() {
        let s = state();
        {
            let mut guard = s.server.as_ref().expect("server").lock().expect("lock");
            for i in 0..5u8 {
                let peer: std::net::IpAddr = format!("192.0.2.{i}").parse().expect("addr");
                let _ = guard.clients.admit(&peer, i as f64);
            }
        }
        let response = handle(&ControlRequest::Clients { limit: 3 }, &s);
        match &response {
            ControlResponse::Clients { rows } => {
                assert_eq!(rows.len(), 3);
                assert_eq!(rows[0].address, "192.0.2.4", "most recent first");
                assert!(rows.iter().all(|r| r.requests == 1));
            }
            other => panic!("expected clients, got {other:?}"),
        }
        // Serializing is part of the op: a response that cannot cross the wire
        // is a response the caller never gets. JSON has no NaN or Infinity, so
        // any non-finite float here fails silently at the transport.
        serde_json::to_string(&response).expect("clients response must serialize");
    }

    #[test]
    fn client_ages_are_finite_even_when_clocks_disagree() {
        // `last_seen` comes from the NTP thread's monotonic clock and `now`
        // from the control thread's. If those ever disagree — or a clock read
        // fails and yields 0.0 — the age must still be a finite number, or
        // serde_json refuses to encode it and the caller sees an empty reply.
        let s = state();
        {
            let mut guard = s.server.as_ref().expect("server").lock().expect("lock");
            let peer: std::net::IpAddr = "192.0.2.1".parse().expect("addr");
            let _ = guard.clients.admit(&peer, f64::MAX);
        }
        let response = handle(&ControlRequest::Clients { limit: 4 }, &s);
        if let ControlResponse::Clients { rows } = &response {
            for r in rows {
                assert!(
                    r.last_seen_ago_s.is_finite(),
                    "age must be finite, got {}",
                    r.last_seen_ago_s
                );
            }
        }
        serde_json::to_string(&response).expect("must serialize");
    }

    #[test]
    fn ntsdata_reports_key_ids_only() {
        let s = state();
        match handle(&ControlRequest::NtsData, &s) {
            ControlResponse::NtsData { master_key_ids } => {
                assert_eq!(master_key_ids, vec![0xABCD]);
            }
            other => panic!("expected ntsdata, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_request_is_an_error_not_a_crash() {
        // The framing layer's contract: garbage in, structured error out.
        let parsed = serde_json::from_str::<ControlRequest>("{\"op\":\"not_an_op\"}");
        assert!(parsed.is_err());
    }
}
