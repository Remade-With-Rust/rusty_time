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

/// Answer one request against live server state.
///
/// Pure apart from the lock: the tests drive this directly, so op behavior is
/// verified without a socket in the way.
pub fn handle(request: &ControlRequest, state: &Arc<Mutex<ServerState>>) -> ControlResponse {
    let Ok(guard) = state.lock() else {
        return ControlResponse::Error {
            message: "server state is poisoned".into(),
        };
    };

    match request {
        ControlRequest::Ping => ControlResponse::Pong {
            version: env!("CARGO_PKG_VERSION").to_string(),
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
pub fn serve(spec: &str, state: Arc<Mutex<ServerState>>) -> Result<(), String> {
    match rusty_time_api::control_endpoint(spec) {
        ControlEndpoint::UnixPath(path) => serve_unix(&path, state),
        ControlEndpoint::Loopback(port) => serve_loopback(port, state),
    }
}

#[cfg(unix)]
fn serve_unix(path: &str, state: Arc<Mutex<ServerState>>) -> Result<(), String> {
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
        let state = Arc::clone(&state);
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
fn serve_unix(_path: &str, _state: Arc<Mutex<ServerState>>) -> Result<(), String> {
    Err("unix domain sockets are not available on this platform".to_string())
}

/// Loopback TCP: the Windows transport until a named-pipe server lands.
///
/// Local-only, but it does **not** carry a pipe's SID-based authorization, so
/// any local process can connect. Stated plainly rather than treated as
/// equivalent to a 0600 unix socket.
fn serve_loopback(port: u16, state: Arc<Mutex<ServerState>>) -> Result<(), String> {
    use std::net::TcpListener;
    let bind = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&bind).map_err(|e| format!("binding {bind}: {e}"))?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = Arc::clone(&state);
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
    state: &Arc<Mutex<ServerState>>,
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

    fn state() -> Arc<Mutex<ServerState>> {
        let mut ring = KeyRing::new(3);
        ring.rotate_in(MasterKey {
            id: 0xABCD,
            key: [7; 32],
        });
        Arc::new(Mutex::new(ServerState {
            clients: ClientTable::new(64, RateLimitConfig::default()),
            ring,
            stratum: 2,
            started_unix: 1_000,
        }))
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
            let mut guard = s.lock().expect("lock");
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
            let mut guard = s.lock().expect("lock");
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
            let mut guard = s.lock().expect("lock");
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
