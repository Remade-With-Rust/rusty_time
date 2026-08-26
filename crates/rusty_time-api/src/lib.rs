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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrackingReport {
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
