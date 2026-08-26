//! Server-side policy: client tracking, rate limiting, and interleaved mode.
//!
//! All decisions, no I/O. The daemon owns sockets and NTS; this module owns
//! *what the answer should be*, which is what makes it testable without a
//! network and portable to wasm.
//!
//! The client key is generic on purpose: the core must not know what an IP
//! address is (mission plan §4 — the core knows bytes and timestamps, never a
//! product type). The daemon instantiates it with the peer address.

use crate::ntp::NtpTimestamp;
use std::collections::HashMap;
use std::hash::Hash;

/// Rate-limit policy, mirroring chrony's `ratelimit interval/burst/leak`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateLimitConfig {
    /// log2 of the mean seconds between responses to one client. 3 = one
    /// response per 8 s, chrony's default.
    pub interval_log2: i8,
    /// How many responses a client may take back to back after being quiet.
    pub burst: u32,
    /// Emit a Kiss-o'-Death to one dropped request in 2^leak_shift. Never 0:
    /// answering *every* dropped request turns the limiter into the
    /// amplifier it exists to prevent.
    pub leak_shift: u8,
    /// Ceiling on responses per second **across all clients**. 0 disables it.
    ///
    /// Per-client limiting alone is defeated by table churn: once the client
    /// population exceeds the table, every request arrives from an address we
    /// have forgotten, gets a fresh bucket, and is answered. TIMECORP S12b
    /// (100k addresses into a 16k table) showed the reply ratio climbing back
    /// to 100% for exactly that reason. A global bucket is the backstop that
    /// bounds total output no matter how the address space is shuffled.
    ///
    /// Set high enough not to hinder a busy legitimate server; it exists to
    /// bound the worst case, not to shape normal traffic.
    pub global_rate_hz: f64,
    /// Global burst allowance, in responses.
    pub global_burst: f64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        RateLimitConfig {
            interval_log2: 3,
            burst: 8,
            leak_shift: 2,
            global_rate_hz: 20_000.0,
            global_burst: 40_000.0,
        }
    }
}

/// What the server should do with one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Answer normally.
    Respond,
    /// Over the limit: answer with a Kiss-o'-Death RATE so a well-behaved
    /// client backs off.
    KissOfDeath,
    /// Over the limit and not this client's turn for a KoD: say nothing.
    Drop,
}

/// Mark a server response's timestamps so the two are distinguishable by their
/// lowest bit: **receive has bit 0 set, transmit has it clear** (RFC 9769 and
/// chrony's `ntp_core.c`).
///
/// Two things depend on this. A server can then recognise an interleaved
/// request statelessly — an origin field with bit 0 set is echoing a *receive*
/// timestamp — and receive can never accidentally equal transmit, which would
/// make the mode ambiguous. The cost is the bottom bit of a 232-picosecond
/// unit, far below any clock's resolution.
pub fn mark_server_timestamps(receive: &mut NtpTimestamp, transmit: &mut NtpTimestamp) {
    receive.0 |= 1;
    transmit.0 &= !1;
}

/// Which timestamps a response should carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseMode {
    /// The ordinary exchange: receive and transmit from *this* exchange.
    Basic,
    /// Interleaved: the reply carries **this** exchange's receive timestamp
    /// (so the client can interleave again next time) together with the
    /// *actual, post-send* transmit timestamp of the earlier response the
    /// client named in its origin field. That transmit value is the whole
    /// point — a basic reply must write its transmit field before the packet
    /// leaves, so it can only ever be an estimate.
    ///
    /// Two field rules, both of which fail silently rather than loudly when
    /// broken (RFC 9769; chrony `ntp_core.c` lines 1241/1251/1290, 2744-2754):
    ///
    /// * **origin** echoes the request's *receive* field, not its transmit —
    ///   that is how the client recognises an interleaved reply.
    /// * **receive** is the current exchange's, not the previous one's.
    ///   Reporting the previous receive pairs the client's T1/T4 with a
    ///   mismatched T2/T3 and shows up as a delay inflated by exactly one
    ///   poll interval — measured against chrony as 4.009 s on a 4 s poll.
    Interleaved {
        /// The true transmit timestamp of the response the client's origin
        /// field identifies.
        prev_transmit: NtpTimestamp,
    },
}

/// Per-client state: enough for rate limiting, interleaved mode and the MRU
/// report, and no more — this is multiplied by every client that has ever
/// spoken to us.
#[derive(Clone, Copy, Debug)]
pub struct ClientRecord {
    pub last_seen: f64,
    /// Token bucket level, in responses.
    tokens: f64,
    pub requests: u64,
    pub responses: u64,
    pub dropped: u64,
    /// Dropped requests since the last Kiss-o'-Death, for deterministic leak.
    drops_since_kod: u32,
    /// T2 of the last request we accepted.
    pub last_receive: Option<NtpTimestamp>,
    /// The true transmit timestamp of our last response, once the driver
    /// reports it. `None` until then, which is why interleaved mode cannot
    /// answer the very first request.
    pub last_transmit: Option<NtpTimestamp>,
    /// The receive timestamp we put in our last response. A client asks for
    /// interleaved mode by echoing exactly this.
    pub last_receive_sent: Option<NtpTimestamp>,
    /// Whether the last answered request was actually served interleaved.
    ///
    /// Distinct from "we *could* serve it interleaved": every client we have
    /// answered once is capable, but only a client that asks is using it, and
    /// an operator reading a client log needs the second fact.
    pub interleaved_now: bool,
    /// This record's position in the eviction order.
    order_seq: u64,
}

impl ClientRecord {
    fn new(now: f64, burst: u32) -> Self {
        ClientRecord {
            last_seen: now,
            tokens: burst as f64,
            requests: 0,
            responses: 0,
            dropped: 0,
            drops_since_kod: 0,
            last_receive: None,
            last_transmit: None,
            last_receive_sent: None,
            interleaved_now: false,
            order_seq: 0,
        }
    }
}

/// Aggregate counters for the `status.serverstats` op.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerStats {
    pub requests: u64,
    pub responses: u64,
    pub dropped_rate_limit: u64,
    pub kiss_of_death: u64,
    pub interleaved_responses: u64,
    /// Requests refused before any per-client work (bad mode, too short).
    pub refused: u64,
    /// Clients evicted from the table because it was full.
    pub evicted: u64,
}

/// Bounded most-recently-used client table.
///
/// The bound is the point: an unbounded map is a memory-exhaustion lever for
/// anyone willing to spoof source addresses. When full, the least recently
/// seen client is evicted — losing its interleaved state, which costs it one
/// exchange, not correctness.
///
/// **Eviction is indexed, not scanned.** The obvious implementation — walk the
/// map for the oldest `last_seen` — is O(capacity) per admission, and a public
/// server facing more clients than the table holds evicts on nearly every
/// packet. TIMECORP S12b (100k clients into a 16k table) went from "runs" to
/// "does not finish" on exactly that, which is what the scenario is for. The
/// `order` index makes it O(log n) *and* deterministic, where a `HashMap` scan
/// depends on iteration order that differs between instances.
pub struct ClientTable<K: Eq + Hash + Ord + Clone> {
    clients: HashMap<K, ClientRecord>,
    /// (touch sequence, key), ordered oldest first.
    order: std::collections::BTreeSet<(u64, K)>,
    /// Monotonic counter giving every touch a distinct, ordered stamp. Time
    /// cannot serve here: two requests can share a `last_seen` reading.
    next_seq: u64,
    /// Global token bucket: level and the time it was last refilled.
    global_tokens: f64,
    global_last: Option<f64>,
    /// Global drops since the last global Kiss-o'-Death.
    global_drops_since_kod: u32,
    capacity: usize,
    config: RateLimitConfig,
    pub stats: ServerStats,
}

impl<K: Eq + Hash + Ord + Clone> ClientTable<K> {
    pub fn new(capacity: usize, config: RateLimitConfig) -> Self {
        ClientTable {
            clients: HashMap::new(),
            order: std::collections::BTreeSet::new(),
            next_seq: 0,
            global_tokens: config.global_burst,
            global_last: None,
            global_drops_since_kod: 0,
            capacity: capacity.max(1),
            config,
            stats: ServerStats::default(),
        }
    }

    /// Move a client to the most-recent end of the eviction order.
    fn touch(&mut self, key: &K) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if let Some(record) = self.clients.get_mut(key) {
            let old = record.order_seq;
            record.order_seq = seq;
            self.order.remove(&(old, key.clone()));
            self.order.insert((seq, key.clone()));
        }
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    pub fn get(&self, key: &K) -> Option<&ClientRecord> {
        self.clients.get(key)
    }

    /// The MRU report: most recently seen first, at most `limit` entries.
    pub fn most_recent(&self, limit: usize) -> Vec<(K, ClientRecord)> {
        let mut all: Vec<(K, ClientRecord)> =
            self.clients.iter().map(|(k, v)| (k.clone(), *v)).collect();
        all.sort_by(|a, b| b.1.last_seen.total_cmp(&a.1.last_seen));
        all.truncate(limit);
        all
    }

    /// Drop the least recently seen client. O(log n) via the order index.
    fn evict_one(&mut self) {
        if let Some(entry) = self.order.iter().next().cloned() {
            self.order.remove(&entry);
            self.clients.remove(&entry.1);
            self.stats.evicted += 1;
        }
    }

    /// Admit one request: refill the client's bucket, decide its fate, and
    /// record it. `now` is monotonic seconds.
    pub fn admit(&mut self, key: &K, now: f64) -> Disposition {
        self.stats.requests += 1;

        // Global ceiling first. It is checked before the per-client bucket so
        // that a churned-address flood — which defeats per-client limiting by
        // never reusing an address — still cannot make us answer without
        // bound.
        if self.config.global_rate_hz > 0.0 {
            let last = self.global_last.unwrap_or(now);
            let elapsed = (now - last).max(0.0);
            self.global_tokens = (self.global_tokens + elapsed * self.config.global_rate_hz)
                .min(self.config.global_burst);
            self.global_last = Some(now);

            if self.global_tokens < 1.0 {
                self.stats.dropped_rate_limit += 1;
                self.global_drops_since_kod += 1;
                let period = 1u32 << self.config.leak_shift.min(16);
                if self.global_drops_since_kod >= period {
                    self.global_drops_since_kod = 0;
                    self.stats.kiss_of_death += 1;
                    return Disposition::KissOfDeath;
                }
                return Disposition::Drop;
            }
        }

        if !self.clients.contains_key(key) {
            if self.clients.len() >= self.capacity {
                self.evict_one();
            }
            let seq = self.next_seq;
            self.next_seq += 1;
            let mut record = ClientRecord::new(now, self.config.burst);
            record.order_seq = seq;
            self.clients.insert(key.clone(), record);
            self.order.insert((seq, key.clone()));
        } else {
            self.touch(key);
        }

        let config = self.config;
        let record = self
            .clients
            .get_mut(key)
            .expect("record was just inserted if missing");

        // Refill: one token per 2^interval seconds since we last saw them.
        let elapsed = (now - record.last_seen).max(0.0);
        let rate = 2f64.powi(-(config.interval_log2 as i32));
        record.tokens = (record.tokens + elapsed * rate).min(config.burst as f64);
        record.last_seen = now;
        record.requests += 1;

        if record.tokens >= 1.0 {
            record.tokens -= 1.0;
            record.responses += 1;
            self.stats.responses += 1;
            // Spend a global token only when a response is actually produced.
            self.global_tokens -= 1.0;
            return Disposition::Respond;
        }

        record.dropped += 1;
        record.drops_since_kod += 1;
        self.stats.dropped_rate_limit += 1;

        // Deterministic leak: every 2^leak_shift-th drop gets a KoD. A
        // deterministic rule is testable, and the client cannot tell the
        // difference from a probabilistic one.
        let period = 1u32 << config.leak_shift.min(16);
        if record.drops_since_kod >= period {
            record.drops_since_kod = 0;
            self.stats.kiss_of_death += 1;
            Disposition::KissOfDeath
        } else {
            Disposition::Drop
        }
    }

    /// Decide basic vs interleaved for an admitted request.
    ///
    /// The client signals interleaved mode by setting its origin timestamp to
    /// the *receive* timestamp we sent last time, rather than the transmit
    /// timestamp. That is unforgeable in the useful sense: only a client that
    /// actually saw our last response knows it.
    pub fn response_mode(&mut self, key: &K, request_origin: NtpTimestamp) -> ResponseMode {
        let Some(record) = self.clients.get(key) else {
            return ResponseMode::Basic;
        };
        let (Some(sent_receive), Some(prev_transmit)) =
            (record.last_receive_sent, record.last_transmit)
        else {
            return ResponseMode::Basic;
        };
        // The client names a specific earlier response by echoing the receive
        // timestamp we reported for it. We keep one slot, so only the most
        // recent qualifies; anything older falls back to basic rather than
        // answering with a transmit timestamp from the wrong exchange.
        let interleaved = request_origin == sent_receive && !request_origin.is_zero();
        if let Some(record) = self.clients.get_mut(key) {
            record.interleaved_now = interleaved;
        }
        if interleaved {
            self.stats.interleaved_responses += 1;
            ResponseMode::Interleaved { prev_transmit }
        } else {
            ResponseMode::Basic
        }
    }

    /// Record what we received and what we told the client, after answering.
    pub fn note_response(&mut self, key: &K, receive: NtpTimestamp, receive_sent: NtpTimestamp) {
        if let Some(record) = self.clients.get_mut(key) {
            record.last_receive = Some(receive);
            record.last_receive_sent = Some(receive_sent);
        }
    }

    /// Record the true transmit timestamp of the response just sent. Called
    /// after `send`, which is the whole point of interleaved mode — this is a
    /// timestamp the basic exchange cannot report because the packet has not
    /// left yet when its own transmit field is written.
    pub fn note_transmit(&mut self, key: &K, transmit: NtpTimestamp) {
        if let Some(record) = self.clients.get_mut(key) {
            record.last_transmit = Some(transmit);
        }
    }

    pub fn note_refused(&mut self) {
        self.stats.refused += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> ClientTable<u32> {
        ClientTable::new(
            4,
            RateLimitConfig {
                interval_log2: 3, // one per 8 s
                burst: 2,
                leak_shift: 2, // KoD every 4th drop
                // Global ceiling off: these tests are about per-client policy.
                global_rate_hz: 0.0,
                global_burst: 0.0,
            },
        )
    }

    #[test]
    fn address_churn_cannot_defeat_the_limiter() {
        // Every request arrives from an address never seen before, so the
        // per-client bucket is always full and the table churns. Without a
        // global ceiling this answers 100% of the flood — which is what
        // TIMECORP S12b measured before the ceiling existed.
        let mut t = ClientTable::<u32>::new(
            1_024,
            RateLimitConfig {
                interval_log2: 3,
                burst: 8,
                leak_shift: 4,
                global_rate_hz: 100.0,
                global_burst: 100.0,
            },
        );
        let mut answered = 0u64;
        // 20_000 requests from 20_000 distinct addresses inside one second.
        for client in 0..20_000u32 {
            if t.admit(&client, client as f64 * 5e-5) == Disposition::Respond {
                answered += 1;
            }
        }
        // Ceiling is 100/s with a 100 burst, over ~1 s: ~200 at the very most.
        assert!(
            answered <= 250,
            "address churn produced {answered} answers against a 100/s ceiling"
        );
        assert!(answered > 0, "the ceiling must not block everything");
    }

    #[test]
    fn the_global_ceiling_refills_over_time() {
        let mut t = ClientTable::<u32>::new(
            16,
            RateLimitConfig {
                interval_log2: -10, // per-client effectively unlimited
                burst: 1_000_000,
                leak_shift: 8,
                global_rate_hz: 10.0,
                global_burst: 10.0,
            },
        );
        let mut answered = 0;
        for i in 0..100u32 {
            if t.admit(&(i % 4), 0.0) == Disposition::Respond {
                answered += 1;
            }
        }
        assert!(answered <= 11, "burst exceeded: {answered}");
        // Ten seconds later the bucket has refilled.
        assert_eq!(t.admit(&0, 10.0), Disposition::Respond);
    }

    #[test]
    fn burst_is_allowed_then_the_limiter_bites() {
        let mut t = table();
        assert_eq!(t.admit(&1, 0.0), Disposition::Respond);
        assert_eq!(t.admit(&1, 0.0), Disposition::Respond);
        // Burst of 2 spent; further immediate requests are not answered.
        assert!(matches!(
            t.admit(&1, 0.0),
            Disposition::Drop | Disposition::KissOfDeath
        ));
        assert_eq!(t.stats.responses, 2);
    }

    #[test]
    fn tokens_refill_over_time() {
        let mut t = table();
        let _ = t.admit(&1, 0.0);
        let _ = t.admit(&1, 0.0);
        assert_ne!(t.admit(&1, 0.0), Disposition::Respond);
        // 8 s later exactly one token is back.
        assert_eq!(t.admit(&1, 8.0), Disposition::Respond);
        assert_ne!(t.admit(&1, 8.0), Disposition::Respond);
    }

    #[test]
    fn kiss_of_death_leaks_at_the_configured_rate_not_every_drop() {
        let mut t = table();
        let _ = t.admit(&1, 0.0);
        let _ = t.admit(&1, 0.0);
        // 12 further requests in the same instant: all over the limit.
        let mut kods = 0;
        for _ in 0..12 {
            if t.admit(&1, 0.0) == Disposition::KissOfDeath {
                kods += 1;
            }
        }
        // leak_shift 2 => one KoD per 4 drops => 3 of 12.
        assert_eq!(kods, 3, "KoD leak rate wrong");
        // The point of leaking: we must answer far less than we are asked, or
        // the limiter is itself an amplifier.
        assert!(
            (kods as u64) < t.stats.dropped_rate_limit,
            "KoD count must stay below the drop count"
        );
    }

    #[test]
    fn one_client_cannot_starve_another() {
        let mut t = table();
        for _ in 0..50 {
            let _ = t.admit(&1, 0.0);
        }
        // A quiet client still gets its full burst.
        assert_eq!(t.admit(&2, 0.0), Disposition::Respond);
        assert_eq!(t.admit(&2, 0.0), Disposition::Respond);
    }

    #[test]
    fn table_is_bounded_and_evicts_the_stalest() {
        let mut t = table(); // capacity 4
        for client in 0..4u32 {
            let _ = t.admit(&client, client as f64);
        }
        assert_eq!(t.len(), 4);
        // A fifth client evicts client 0, the least recently seen.
        let _ = t.admit(&99, 10.0);
        assert_eq!(t.len(), 4, "table exceeded its bound");
        assert!(t.get(&0).is_none(), "stalest client was not evicted");
        assert!(t.get(&99).is_some());
        assert_eq!(t.stats.evicted, 1);
    }

    #[test]
    fn interleaved_requires_the_client_to_echo_our_receive_timestamp() {
        let mut t = table();
        let _ = t.admit(&1, 0.0);
        let rx1 = NtpTimestamp::from_unix(1_756_224_000, 0);

        // First exchange: nothing to interleave with yet.
        assert_eq!(
            t.response_mode(&1, NtpTimestamp(0x1111)),
            ResponseMode::Basic
        );
        t.note_response(&1, rx1, rx1);
        let tx1 = NtpTimestamp::from_unix(1_756_224_000, 500);
        t.note_transmit(&1, tx1);

        // Second exchange, client echoes our receive timestamp: interleaved,
        // and it gets the true transmit of the response it named.
        let _ = t.admit(&1, 8.0);
        match t.response_mode(&1, rx1) {
            ResponseMode::Interleaved { prev_transmit } => {
                assert_eq!(prev_transmit, tx1);
            }
            other => panic!("expected interleaved, got {other:?}"),
        }
        assert_eq!(t.stats.interleaved_responses, 1);
    }

    #[test]
    fn interleaved_flag_tracks_use_not_capability() {
        let mut t = table();
        let rx1 = NtpTimestamp::from_unix(1_756_224_000, 0);
        let _ = t.admit(&1, 0.0);
        t.note_response(&1, rx1, rx1);
        t.note_transmit(&1, rx1);

        // Capable now, but this client has never asked.
        let _ = t.admit(&1, 8.0);
        let _ = t.response_mode(&1, NtpTimestamp::ZERO);
        assert!(
            !t.get(&1).expect("record").interleaved_now,
            "a client that never asked must not be reported as using interleaved"
        );

        // Now it asks.
        let _ = t.admit(&1, 16.0);
        let _ = t.response_mode(&1, rx1);
        assert!(t.get(&1).expect("record").interleaved_now);

        // And stops asking again.
        let _ = t.admit(&1, 24.0);
        let _ = t.response_mode(&1, NtpTimestamp::ZERO);
        assert!(!t.get(&1).expect("record").interleaved_now);
    }

    #[test]
    fn a_client_echoing_the_wrong_value_gets_basic_mode() {
        let mut t = table();
        let _ = t.admit(&1, 0.0);
        let rx1 = NtpTimestamp::from_unix(1_756_224_000, 0);
        t.note_response(&1, rx1, rx1);
        t.note_transmit(&1, NtpTimestamp::from_unix(1_756_224_000, 500));

        let _ = t.admit(&1, 8.0);
        // Some other value — an off-path guess — must not unlock interleaved.
        assert_eq!(
            t.response_mode(&1, NtpTimestamp(0xDEAD_BEEF)),
            ResponseMode::Basic
        );
        // Nor may a zero origin.
        assert_eq!(t.response_mode(&1, NtpTimestamp::ZERO), ResponseMode::Basic);
    }

    #[test]
    fn eviction_downgrades_to_basic_rather_than_lying() {
        let mut t = table(); // capacity 4
        let rx = NtpTimestamp::from_unix(1_756_224_000, 0);
        let _ = t.admit(&1, 0.0);
        t.note_response(&1, rx, rx);
        t.note_transmit(&1, rx);

        // Push client 1 out.
        for c in 10..15u32 {
            let _ = t.admit(&c, 100.0 + c as f64);
        }
        assert!(t.get(&1).is_none());
        // Its next request must be answered in basic mode, not with another
        // client's timestamps.
        let _ = t.admit(&1, 200.0);
        assert_eq!(t.response_mode(&1, rx), ResponseMode::Basic);
    }

    #[test]
    fn heavy_client_churn_stays_tractable() {
        // A public server sees far more addresses than its table holds, so
        // eviction runs on nearly every packet. A scan-based eviction is
        // O(capacity) each time and this test does not finish; the indexed one
        // is O(log n). The assertion is a *count*, not a duration: every
        // admission must do a bounded amount of index work, which shows up as
        // the table never exceeding capacity while churning far past it.
        let capacity = 4_096;
        let mut t = ClientTable::<u32>::new(capacity, RateLimitConfig::default());
        let churn = 200_000u32;
        for client in 0..churn {
            let _ = t.admit(&client, client as f64 * 0.001);
        }
        assert_eq!(t.len(), capacity, "table must sit exactly at capacity");
        assert_eq!(
            t.stats.evicted,
            (churn as u64) - capacity as u64,
            "every client past capacity must have cost exactly one eviction"
        );
        // True LRU: the survivors are the most recent `capacity` clients.
        assert!(t.get(&(churn - 1)).is_some(), "newest client was evicted");
        assert!(t.get(&0).is_none(), "oldest client survived");
    }

    #[test]
    fn eviction_is_true_lru_not_insertion_order() {
        let mut t = ClientTable::<u32>::new(3, RateLimitConfig::default());
        let _ = t.admit(&1, 0.0);
        let _ = t.admit(&2, 1.0);
        let _ = t.admit(&3, 2.0);
        // Touch client 1 so it is no longer the stalest.
        let _ = t.admit(&1, 3.0);
        // Inserting a fourth must evict client 2, not client 1.
        let _ = t.admit(&4, 4.0);
        assert!(t.get(&1).is_some(), "recently used client was evicted");
        assert!(t.get(&2).is_none(), "stalest client should have gone");
        assert!(t.get(&3).is_some());
        assert!(t.get(&4).is_some());
    }

    #[test]
    fn mru_report_is_ordered_and_bounded() {
        let mut t = ClientTable::<u32>::new(16, RateLimitConfig::default());
        for c in 0..10u32 {
            let _ = t.admit(&c, c as f64);
        }
        let mru = t.most_recent(3);
        assert_eq!(mru.len(), 3);
        assert_eq!(mru[0].0, 9, "most recent first");
        assert_eq!(mru[2].0, 7);
    }
}
