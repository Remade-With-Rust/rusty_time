//! rusty_time-api — typed reports and ops.
//!
//! Everything a human sees through `rtimec` or `rtimed --json` is one of these
//! types serialized as JSON: the CLI is a thin consumer, and a test or an agent
//! is the same consumer with a different transport (mission plan §5). Internal
//! wire moves to oxicode at M4; the public shape stays JSON.

use serde::{Deserialize, Serialize};

/// One measured exchange, as reported by `rtimed query`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct SampleReport {
    /// Seconds to ADD to the local clock.
    pub offset_s: f64,
    /// Round-trip delay, seconds.
    pub delay_s: f64,
    /// Server stratum.
    pub stratum: u8,
    /// Server-reported root delay + dispersion, seconds.
    pub root_delay_s: f64,
    pub root_dispersion_s: f64,
}

/// Where the control plane actually listens, resolved from what the operator
/// typed.
///
/// Unix has domain sockets, Windows does not (its equivalent is a named pipe,
/// which lands with the Win32 pipe server). Rather than make every script and
/// CI job branch on the platform, the *same* `--control` argument resolves on
/// both: a path becomes a deterministic loopback port on Windows, derived from
/// the path text so the daemon and `rtimec` independently agree on it.
///
/// The daemon prints the resolved endpoint at startup, so the mapping is
/// visible rather than a silent surprise.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ControlEndpoint {
    /// A Unix domain socket at this path.
    UnixPath(String),
    /// A TCP endpoint on loopback.
    Loopback(u16),
}

/// Loopback ports we derive into: the IANA dynamic range, avoiding anything an
/// OS is likely to hand out for an ephemeral connection.
const DERIVED_PORT_BASE: u16 = 49_200;
const DERIVED_PORT_SPAN: u16 = 300;

/// The default control name.
///
/// Defined here, once, because the daemon and `rtimec` must agree: on Windows
/// the name is hashed into a port, so two *different* default strings would
/// resolve to two different ports and `rtimec` would quietly fail to find a
/// daemon that is running perfectly well.
pub fn default_control_spec() -> String {
    #[cfg(windows)]
    {
        "rusty_time".to_string()
    }
    #[cfg(not(windows))]
    {
        match std::env::var("XDG_RUNTIME_DIR") {
            Ok(dir) if !dir.is_empty() => format!("{dir}/rusty_time.sock"),
            _ => "/tmp/rusty_time.sock".to_string(),
        }
    }
}

/// Resolve a `--control` argument for this platform.
pub fn control_endpoint(spec: &str) -> ControlEndpoint {
    // An explicit host:port is honoured everywhere.
    if let Some((_, port)) = spec.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
    {
        return ControlEndpoint::Loopback(port);
    }

    if cfg!(windows) {
        ControlEndpoint::Loopback(derive_port(spec))
    } else {
        ControlEndpoint::UnixPath(spec.to_string())
    }
}

/// A stable port for a given control name. FNV-1a: tiny, dependency-free, and
/// — the property that matters — identical in both processes and across runs.
fn derive_port(spec: &str) -> u16 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in spec.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    DERIVED_PORT_BASE + (hash % DERIVED_PORT_SPAN as u64) as u16
}

/// What NTS did during a query (`status.ntsdata`'s client half).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NtsReport {
    /// The host key establishment ran against.
    pub ke_host: String,
    /// The NTP server KE pointed us at — may differ from `ke_host`.
    pub ntp_server: String,
    pub ntp_port: u16,
    /// Responses that passed AEAD verification.
    pub authenticated: u32,
    /// Responses dropped because they did not (forged, stale cookie, NAK).
    pub rejected: u32,
    /// Unspent cookies remaining when the query finished.
    pub cookies_after: usize,
}

/// The result of a one-shot `rtimed query`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QueryReport {
    pub server: String,
    pub address: String,
    /// Exchanges attempted and completed.
    pub sent: u32,
    pub received: u32,
    pub samples: Vec<SampleReport>,
    /// Minimum-delay sample's offset — the headline number.
    pub best_offset_s: Option<f64>,
    pub best_delay_s: Option<f64>,
    /// Regression view when enough samples exist.
    pub regress_offset_s: Option<f64>,
    pub regress_freq_ppm: Option<f64>,
    pub regress_sd_s: Option<f64>,
    /// Reference ID of the server (textual for stratum 1, hex otherwise).
    pub reference_id: String,
    pub leap: String,
    /// Present only when the query ran under NTS.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub nts: Option<NtsReport>,
}

/// Daemon tracking state (the `status.tracking` op).
///
/// **Read `synchronized` first.** It is true only when a correction actually
/// reached the clock, and the other fields are only meaningful when it is: an
/// unsynchronized daemon is running an OPEN loop (a dry run never steers, and
/// an acquiring loop has not converged), so its `freq_ppm` is not a drift
/// measurement and its `offset_s` is an estimate nothing has acted on. A
/// consumer deciding anything on this report — a capability expiry, a
/// settlement timestamp — should treat `synchronized == false` as "no usable
/// time from here" rather than reading the numbers beside it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrackingReport {
    /// Whether a correction actually reached the clock. See the type docs:
    /// every other field is conditioned on this.
    pub synchronized: bool,
    pub offset_s: f64,
    pub freq_ppm: f64,
    pub error_bound_s: f64,
    pub poll_log2: i8,
}

/// Server counters (`status.serverstats`) — the chronyc `serverstats` analog.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerStatsReport {
    pub ntp_requests: u64,
    pub ntp_responses: u64,
    pub dropped_rate_limit: u64,
    pub kiss_of_death: u64,
    pub interleaved_responses: u64,
    pub refused: u64,
    pub clients_tracked: usize,
    pub clients_evicted: u64,
    pub uptime_s: u64,
    pub stratum: u8,
}

/// One row of the MRU client log (`debug.clients`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClientRow {
    pub address: String,
    /// Seconds since this client was last seen.
    pub last_seen_ago_s: f64,
    pub requests: u64,
    pub responses: u64,
    pub dropped: u64,
    /// Whether this client is currently using interleaved mode.
    pub interleaved: bool,
}

/// A request on the control socket. One variant per op (mission plan §5): the
/// CLI, a test and an agent are all just clients of these.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlRequest {
    /// `status.serverstats`
    ServerStats,
    /// `debug.clients`
    Clients { limit: usize },
    /// `status.ntsdata` — key ids only, never key material.
    NtsData,
    /// `status.tracking` — the client discipline loop's current estimate:
    /// how far off this clock believes it is, how well it knows that, and
    /// whether it is synchronized at all.
    ///
    /// Answered by `rtimed sync`; a `rtimed serve` daemon has no discipline
    /// loop and reports that rather than inventing a number.
    Tracking,
    /// Liveness.
    Ping,
}

/// The answer to a [`ControlRequest`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ControlResponse {
    ServerStats(ServerStatsReport),
    /// A struct variant, not `Clients(Vec<..>)`, deliberately: serde's
    /// internally-tagged representation cannot encode a newtype variant that
    /// wraps a *sequence*, and the failure appears only at serialization —
    /// the op works in-process and returns an empty reply over the socket.
    Clients {
        rows: Vec<ClientRow>,
    },
    NtsData {
        /// Master key identifiers currently held. Key material is never
        /// serialized — an operator needs to know rotation happened, not what
        /// the keys are.
        master_key_ids: Vec<u32>,
    },
    /// A newtype over a STRUCT, which the internally-tagged representation
    /// flattens to `{"result":"tracking", <fields>}` — the same shape
    /// `ServerStats` already produces. (Contrast `Clients` above, which cannot
    /// be a newtype because it wraps a sequence.)
    Tracking(TrackingReport),
    Pong {
        version: String,
    },
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_report_json_shape_is_stable() {
        let r = QueryReport {
            server: "pool.ntp.org".into(),
            address: "1.2.3.4:123".into(),
            sent: 4,
            received: 4,
            samples: vec![SampleReport {
                offset_s: 0.0012,
                delay_s: 0.031,
                stratum: 2,
                root_delay_s: 0.01,
                root_dispersion_s: 0.002,
            }],
            best_offset_s: Some(0.0012),
            best_delay_s: Some(0.031),
            regress_offset_s: None,
            regress_freq_ppm: None,
            regress_sd_s: None,
            reference_id: "c0a80101".into(),
            leap: "no-warning".into(),
            nts: None,
        };
        let json = serde_json::to_string(&r).expect("serialize");
        let back: QueryReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
        // Public wire fields are snake_case with explicit units in the name.
        assert!(json.contains("\"best_offset_s\""));
        // A plain query must not emit an empty nts object.
        assert!(!json.contains("\"nts\""));
    }

    #[test]
    fn the_default_spec_resolves_the_same_way_for_everyone() {
        // The daemon and rtimec each call this independently. If they ever
        // produced different strings, Windows would hash them to different
        // ports and rtimec would report "is rtimed running?" about a daemon
        // that is running fine.
        let a = control_endpoint(&default_control_spec());
        let b = control_endpoint(&default_control_spec());
        assert_eq!(a, b);
    }

    #[test]
    fn an_explicit_port_is_honoured_on_every_platform() {
        assert_eq!(
            control_endpoint("127.0.0.1:9999"),
            ControlEndpoint::Loopback(9999)
        );
    }

    #[test]
    fn the_same_path_resolves_identically_in_both_processes() {
        // The daemon and rtimec each resolve independently; if they disagreed,
        // rtimec would connect to a port nothing is listening on.
        let a = control_endpoint("/run/rusty_time.sock");
        let b = control_endpoint("/run/rusty_time.sock");
        assert_eq!(a, b, "resolution must be deterministic");
    }

    #[test]
    fn different_names_get_different_endpoints() {
        // Two daemons with different control names must not collide, or the
        // second would fail to bind and the first would answer for both.
        let mut seen = std::collections::HashSet::new();
        let names = [
            "/run/rusty_time.sock",
            "/tmp/a.sock",
            "/tmp/b.sock",
            "rusty_time",
            "test-rig-1",
            "test-rig-2",
        ];
        for name in names {
            seen.insert(control_endpoint(name));
        }
        assert!(
            seen.len() >= names.len() - 1,
            "control names collided: {seen:?}"
        );
    }

    #[test]
    fn derived_ports_stay_in_the_intended_range() {
        for name in ["a", "b", "/very/long/path/to/a/socket", ""] {
            let port = derive_port(name);
            assert!(
                (DERIVED_PORT_BASE..DERIVED_PORT_BASE + DERIVED_PORT_SPAN).contains(&port),
                "{name} derived out-of-range port {port}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_path_stays_a_unix_socket_on_unix() {
        assert_eq!(
            control_endpoint("/run/rusty_time.sock"),
            ControlEndpoint::UnixPath("/run/rusty_time.sock".into())
        );
    }

    #[test]
    fn control_ops_round_trip_over_the_wire() {
        // Every op must survive the JSON hop unchanged: rtimec, a test and an
        // agent are the same client with different transports.
        let requests = vec![
            ControlRequest::Ping,
            ControlRequest::ServerStats,
            ControlRequest::Clients { limit: 10 },
            ControlRequest::NtsData,
        ];
        for req in requests {
            let json = serde_json::to_string(&req).expect("serialize");
            let back: ControlRequest = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(req, back, "op did not survive the wire: {json}");
        }

        let responses = vec![
            ControlResponse::Pong {
                version: "0.1.0".into(),
            },
            ControlResponse::ServerStats(ServerStatsReport {
                ntp_requests: 10,
                ntp_responses: 8,
                dropped_rate_limit: 2,
                ..ServerStatsReport::default()
            }),
            ControlResponse::Clients {
                rows: vec![ClientRow {
                    address: "192.0.2.1:123".into(),
                    last_seen_ago_s: 1.5,
                    requests: 3,
                    responses: 3,
                    dropped: 0,
                    interleaved: true,
                }],
            },
            ControlResponse::NtsData {
                master_key_ids: vec![1, 2],
            },
            ControlResponse::Error {
                message: "nope".into(),
            },
        ];
        for resp in responses {
            let json = serde_json::to_string(&resp).expect("serialize");
            let back: ControlResponse = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(resp, back);
        }
    }

    #[test]
    fn nts_data_never_carries_key_material() {
        // The type makes it unrepresentable: there is nowhere to put a key.
        let resp = ControlResponse::NtsData {
            master_key_ids: vec![0xDEAD_BEEF],
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(json.contains("master_key_ids"));
        assert!(!json.contains("key\":\"") && !json.to_lowercase().contains("secret"));
    }

    #[test]
    fn nts_report_round_trips() {
        let mut r = QueryReport {
            server: "time.cloudflare.com".into(),
            address: "1.1.1.1:123".into(),
            sent: 4,
            received: 4,
            samples: Vec::new(),
            best_offset_s: Some(-0.002),
            best_delay_s: Some(0.02),
            regress_offset_s: None,
            regress_freq_ppm: None,
            regress_sd_s: None,
            reference_id: "0a0a0a0a".into(),
            leap: "no-warning".into(),
            nts: Some(NtsReport {
                ke_host: "time.cloudflare.com".into(),
                ntp_server: "time.cloudflare.com".into(),
                ntp_port: 123,
                authenticated: 4,
                rejected: 0,
                cookies_after: 8,
            }),
        };
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("\"authenticated\":4"));
        let back: QueryReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
        r.nts = None;
        assert_ne!(r, back);
    }
}
