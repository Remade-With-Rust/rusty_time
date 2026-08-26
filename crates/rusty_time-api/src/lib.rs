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

/// Daemon tracking state (the `status.tracking` op). Grows at M4.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TrackingReport {
    pub synchronized: bool,
    pub offset_s: f64,
    pub freq_ppm: f64,
    pub error_bound_s: f64,
    pub poll_log2: i8,
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
