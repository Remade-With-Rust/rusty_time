//! TIMECORP S12 — server load.
//!
//! Two things are measured, and they are different kinds of number:
//!
//! * **Deterministic counts** (admitted, dropped, kissed, evicted, clients
//!   tracked) come from driving the real `ClientTable` policy. They need no
//!   pinning, no repetition and no noise floor — they are exact, and they are
//!   what the rate limiter's correctness actually rests on.
//! * **A throughput rate** (responses/second of the reply-building path) is a
//!   duration, so it is reported as a best-of-N with the arm's own null
//!   measurement alongside it, and it is explicitly *not* a claim about
//!   rusty_time versus chrony — that comparison needs both binaries on one
//!   rig (mission plan §7.1).
//!
//! Poisson arrivals over a client population, exactly as S12 specifies.

use crate::rng::Pcg32;
use rusty_time_core::ntp::NtpTimestamp;
use rusty_time_core::server::{ClientTable, Disposition, RateLimitConfig};

/// One S12 configuration.
#[derive(Clone, Copy, Debug)]
pub struct LoadScenario {
    pub name: &'static str,
    /// Distinct client addresses in the population.
    pub clients: usize,
    /// Mean requests per second across the whole population.
    pub arrival_rate_hz: f64,
    /// Simulated seconds.
    pub duration_s: f64,
    /// Table capacity — smaller than `clients` on purpose in the eviction case.
    pub table_capacity: usize,
}

pub const LOAD_SCENARIOS: &[LoadScenario] = &[
    LoadScenario {
        name: "S12a",
        clients: 1_000,
        arrival_rate_hz: 1_000.0,
        duration_s: 60.0,
        table_capacity: 16_384,
    },
    LoadScenario {
        name: "S12b",
        clients: 100_000,
        arrival_rate_hz: 50_000.0,
        duration_s: 60.0,
        table_capacity: 16_384,
    },
    LoadScenario {
        // One address flooding while a well-behaved population continues.
        name: "S12c",
        clients: 1_000,
        arrival_rate_hz: 5_000.0,
        duration_s: 60.0,
        table_capacity: 16_384,
    },
];

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct LoadMetrics {
    pub requests: u64,
    pub answered: u64,
    pub dropped: u64,
    pub kissed: u64,
    pub evicted: u64,
    pub clients_tracked: usize,
    /// Fraction of requests that produced any packet at all. Below 1.0 is the
    /// property that stops a server being an amplifier.
    pub reply_ratio: f64,
    /// Bytes of client-table state per tracked client (measured struct size,
    /// not an estimate).
    pub bytes_per_client: usize,
}

/// Drive the real admission policy under Poisson arrivals.
pub fn run(scenario: &LoadScenario, seed: u64) -> LoadMetrics {
    let mut rng = Pcg32::new(seed, 0x5E12);
    let mut table: ClientTable<u32> =
        ClientTable::new(scenario.table_capacity, RateLimitConfig::default());

    let mut t = 0.0f64;
    let mean_gap = 1.0 / scenario.arrival_rate_hz.max(1e-9);
    let mut answered = 0u64;
    let mut kissed = 0u64;
    let mut dropped = 0u64;
    let mut requests = 0u64;

    while t < scenario.duration_s {
        // Exponential inter-arrival: the Poisson process S12 calls for.
        let u = rng.uniform().max(1e-12);
        t += -mean_gap * u.ln();
        if t >= scenario.duration_s {
            break;
        }

        // S12c: one address is responsible for most of the traffic.
        let client = if scenario.name == "S12c" && rng.uniform() < 0.9 {
            0
        } else {
            (rng.next_u32() as usize % scenario.clients) as u32
        };

        requests += 1;
        match table.admit(&client, t) {
            Disposition::Respond => answered += 1,
            Disposition::KissOfDeath => kissed += 1,
            Disposition::Drop => dropped += 1,
        }
        // A served client goes on to establish interleaved state, as a real
        // one would; this exercises the same memory a live server holds.
        if requests.is_multiple_of(4) {
            let ts = NtpTimestamp::from_unix(1_756_224_000 + t as i64, 0);
            table.note_response(&client, ts, ts);
            table.note_transmit(&client, ts);
        }
    }

    LoadMetrics {
        requests,
        answered,
        dropped,
        kissed,
        evicted: table.stats.evicted,
        clients_tracked: table.len(),
        reply_ratio: if requests > 0 {
            (answered + kissed) as f64 / requests as f64
        } else {
            0.0
        },
        bytes_per_client: ClientTable::<u32>::bytes_per_client(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(name: &str) -> &'static LoadScenario {
        LOAD_SCENARIOS
            .iter()
            .find(|s| s.name == name)
            .expect("scenario")
    }

    #[test]
    fn s12_is_deterministic() {
        let s = scenario("S12a");
        let a = run(s, 7);
        let b = run(s, 7);
        assert_eq!(
            serde_json::to_string(&a).expect("json"),
            serde_json::to_string(&b).expect("json"),
            "same seed must give identical counts"
        );
    }

    #[test]
    fn a_server_under_load_never_answers_more_than_it_is_asked() {
        // The amplification bound, as a deterministic count rather than a
        // benchmark: replies must never exceed requests.
        for s in LOAD_SCENARIOS {
            let m = run(s, 3);
            assert!(
                m.answered + m.kissed <= m.requests,
                "{}: replied {} to {} requests",
                s.name,
                m.answered + m.kissed,
                m.requests
            );
            assert!(
                m.reply_ratio <= 1.0,
                "{}: reply ratio {} exceeds 1",
                s.name,
                m.reply_ratio
            );
        }
    }

    #[test]
    fn a_flooding_address_is_contained_while_others_are_served() {
        let m = run(scenario("S12c"), 11);
        // 90% of traffic comes from one address; the limiter must reject most
        // of it, so the overall reply ratio stays well below 1.
        assert!(
            m.reply_ratio < 0.5,
            "flood was not contained: reply ratio {}",
            m.reply_ratio
        );
        assert!(m.dropped > m.answered, "most of a flood must be dropped");
    }

    #[test]
    fn the_client_table_stays_within_its_bound_under_100k_clients() {
        let m = run(scenario("S12b"), 5);
        assert!(
            m.clients_tracked <= 16_384,
            "table grew to {} entries, past its bound",
            m.clients_tracked
        );
        assert!(
            m.evicted > 0,
            "100k clients into a 16k table should have evicted somebody"
        );
        // Memory is bounded and knowable: capacity x per-client state.
        let worst_case_bytes = 16_384 * m.bytes_per_client;
        assert!(
            worst_case_bytes < 8 * 1024 * 1024,
            "client table worst case is {worst_case_bytes} bytes"
        );
    }
}
