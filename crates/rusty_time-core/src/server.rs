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
use std::hash::{Hash, Hasher};

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

/// End-of-list sentinel for the intrusive recency links.
const NIL: u32 = u32::MAX;

/// Odd 64-bit multiplier for the key mixer. Any odd constant with a good bit
/// spread works; this one is xxHash's prime 1.
const MIX: u64 = 0x9e37_79b1_85eb_ca87;

/// A fast, **seeded** hasher for short keys.
///
/// The default `HashMap` hasher is SipHash-1-3, chosen for resistance to
/// collision floods. That resistance is not optional here — the key is a
/// client's source address, which an attacker picks — but SipHash's cost is
/// out of proportion to a 4-to-17-byte key: with everything else in the client
/// table fixed, callgrind still attributed ~32% of the server's per-request
/// instructions to hashing one address once.
///
/// So this keeps the property and drops the price. The seed is drawn from the
/// OS once per process via `RandomState`, exactly as SipHash's keys are, so an
/// attacker cannot compute which addresses collide without first learning a
/// secret they never see. What is given up is SipHash's *proof* against an
/// adversary who somehow does learn the seed; what is kept is the practical
/// defence, on a table that is additionally bounded to a fixed capacity with
/// LRU eviction, so even a successful collision attack cannot grow a chain
/// without bound.
#[derive(Clone, Copy)]
pub struct ClientHashBuilder {
    seed: u64,
}

impl Default for ClientHashBuilder {
    fn default() -> Self {
        // `RUSTY_TIME_HASH_SEED` pins the seed. It exists for measurement: a
        // random seed changes which keys collide, which moves the probe count,
        // which makes an instruction-count harness reproducible only to about
        // 0.002% instead of exactly. That is far below any effect worth acting
        // on, but an instrument that is exact is worth more than one that is
        // nearly exact, and the pin costs one environment read per table.
        //
        // It is emphatically NOT for production: a known seed is a known
        // collision set, which is the property this hasher is seeded to deny.
        if let Ok(pinned) = std::env::var("RUSTY_TIME_HASH_SEED")
            && let Ok(seed) = pinned.parse::<u64>()
        {
            return ClientHashBuilder { seed };
        }
        // One OS-random draw per table, reusing std's entropy source rather
        // than adding a dependency for it.
        use std::hash::BuildHasher as _;
        let seed = std::collections::hash_map::RandomState::new().hash_one(0xA5A5_5A5Au64);
        ClientHashBuilder { seed }
    }
}

impl std::hash::BuildHasher for ClientHashBuilder {
    type Hasher = ClientHasher;
    fn build_hasher(&self) -> ClientHasher {
        ClientHasher { state: self.seed }
    }
}

pub struct ClientHasher {
    state: u64,
}

impl ClientHasher {
    #[inline]
    fn mix(&mut self, value: u64) {
        self.state = (self.state ^ value).wrapping_mul(MIX);
    }
}

impl Hasher for ClientHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.mix(u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut buf = [0u8; 8];
            buf[..rest.len()].copy_from_slice(rest);
            // Fold the length in so trailing zero bytes cannot alias a shorter
            // key against a longer one.
            self.mix(u64::from_le_bytes(buf) ^ (rest.len() as u64) << 56);
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.mix(value as u64);
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.mix(value as u64);
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.mix(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.mix(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        // splitmix64's finalizer: full avalanche in a handful of instructions,
        // which is what stops near-adjacent addresses landing in near-adjacent
        // buckets.
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// One client's storage: its key, its record, and its place in the recency
/// list. Slots are stable — an index handed out stays valid until the client
/// is evicted — which is what makes the list links safe as plain integers.
struct Slot<K> {
    key: K,
    record: ClientRecord,
    /// Toward the most-recently-used end.
    prev: u32,
    /// Toward the least-recently-used end.
    next: u32,
    /// Bumped every time this slot is handed to a new client, so a handle kept
    /// across an eviction is detected rather than silently addressing whoever
    /// took the slot over.
    generation: u32,
}

/// A resolved position in the table.
///
/// One request touches the same client four times — admit, choose a response
/// mode, record what was sent, then record the true transmit time after the
/// packet leaves. Looking the key up each time cost four hashes of the same
/// address, which callgrind put at ~51% of what the server does per request
/// once the recency index was fixed. A handle turns the three follow-ups into
/// array indexing.
///
/// It is deliberately not a raw index: the generation makes a handle that
/// outlived its client detectably stale instead of quietly wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientHandle {
    slot: u32,
    generation: u32,
}

impl ClientHandle {
    /// A handle that resolves to nothing — what a request rejected by the
    /// global limiter gets, having never reached a per-client record.
    pub const INVALID: ClientHandle = ClientHandle {
        slot: NIL,
        generation: 0,
    };
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
    /// Key to slot. The only hashed structure, and the reason a request costs
    /// one hash instead of several.
    index: HashMap<K, u32, ClientHashBuilder>,
    /// Records in stable storage. Slots are handed out from `free` and never
    /// move, which is what lets the ordering below be pointers rather than
    /// comparisons.
    slots: Vec<Slot<K>>,
    free: Vec<u32>,
    /// Ends of the intrusive most-recently-used list threaded through `slots`.
    mru: u32,
    lru: u32,
    /// Global token bucket: level and the time it was last refilled.
    global_tokens: f64,
    global_last: Option<f64>,
    /// Global drops since the last global Kiss-o'-Death.
    global_drops_since_kod: u32,
    capacity: usize,
    config: RateLimitConfig,
    /// Tokens a client earns per second — `2^-interval_log2`, precomputed.
    ///
    /// It derives only from `config`, which is fixed at construction, so
    /// recomputing it per request was a `powi` call on the hot path for a
    /// constant. Callgrind put the server's per-request cost at 3053 Ir; this
    /// is one of the cheaper pieces of that, and it is free to remove.
    refill_per_s: f64,
    pub stats: ServerStats,
}

impl<K: Eq + Hash + Ord + Clone> ClientTable<K> {
    pub fn new(capacity: usize, config: RateLimitConfig) -> Self {
        ClientTable {
            index: HashMap::with_capacity_and_hasher(capacity.max(1), ClientHashBuilder::default()),
            slots: Vec::with_capacity(capacity.max(1)),
            free: Vec::new(),
            mru: NIL,
            lru: NIL,
            global_tokens: config.global_burst,
            global_last: None,
            global_drops_since_kod: 0,
            capacity: capacity.max(1),
            config,
            refill_per_s: 2f64.powi(-(config.interval_log2 as i32)),
            stats: ServerStats::default(),
        }
    }

    /// Detach a slot from the recency list.
    fn unlink(&mut self, i: u32) {
        let (prev, next) = {
            let slot = &self.slots[i as usize];
            (slot.prev, slot.next)
        };
        if prev == NIL {
            self.mru = next;
        } else {
            self.slots[prev as usize].next = next;
        }
        if next == NIL {
            self.lru = prev;
        } else {
            self.slots[next as usize].prev = prev;
        }
    }

    /// Put a detached slot at the most-recent end.
    fn link_front(&mut self, i: u32) {
        let old = self.mru;
        {
            let slot = &mut self.slots[i as usize];
            slot.prev = NIL;
            slot.next = old;
        }
        if old == NIL {
            self.lru = i;
        } else {
            self.slots[old as usize].prev = i;
        }
        self.mru = i;
    }

    /// Move a client to the most-recent end of the eviction order.
    ///
    /// Six pointer writes, no hashing, no comparisons, no allocation. The
    /// previous form kept a `BTreeSet<(seq, K)>` and did a remove plus an
    /// insert — two O(log n) tree walks and two key clones — on *every*
    /// request. Callgrind attributed 23% of the whole server hot path to that
    /// tree's `search_tree`, for an index that is only ever read when the
    /// table is full and something has to be evicted.
    fn touch(&mut self, i: u32) {
        if self.mru == i {
            return; // already most-recent; the common case for a chatty client
        }
        self.unlink(i);
        self.link_front(i);
    }

    /// The real per-client footprint: the record, the slot links that order
    /// it, and the index entry that finds it.
    ///
    /// Reported by the type rather than estimated by the caller, because the
    /// corpus quotes this number and an estimate drifts silently when the
    /// structure changes. It did: the figure used to be `size_of::<ClientRecord>()`
    /// alone, which stopped being the whole story the moment records moved
    /// into slots.
    pub fn bytes_per_client() -> usize {
        core::mem::size_of::<Slot<K>>()
            + core::mem::size_of::<K>()          // the index's own copy of the key
            + core::mem::size_of::<u32>()        // the slot number it maps to
            + 1 // hashbrown's control byte
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn get(&self, key: &K) -> Option<&ClientRecord> {
        let i = *self.index.get(key)?;
        Some(&self.slots[i as usize].record)
    }

    /// The MRU report: most recently seen first, at most `limit` entries.
    ///
    /// Walks the recency list, which is **already** in this order — `admit`
    /// moves a client to the front and sets `last_seen` in the same breath, so
    /// list order and descending `last_seen` are the same thing.
    ///
    /// The previous form cloned every record in the table into a `Vec`, sorted
    /// all of them, and threw away all but `limit`. At the daemon's capacity
    /// of 16384 that is ~1.5 MiB copied and an O(n log n) sort to answer a
    /// ten-row status query. This is O(limit) and allocates once, for exactly
    /// the rows returned.
    pub fn most_recent(&self, limit: usize) -> Vec<(K, ClientRecord)> {
        let mut out = Vec::with_capacity(limit.min(self.index.len()));
        let mut at = self.mru;
        while at != NIL && out.len() < limit {
            let slot = &self.slots[at as usize];
            out.push((slot.key.clone(), slot.record));
            at = slot.next;
        }
        out
    }

    /// Drop the least recently seen client. O(1): it is the list's tail.
    fn evict_one(&mut self) {
        let victim = self.lru;
        if victim == NIL {
            return;
        }
        self.unlink(victim);
        let key = self.slots[victim as usize].key.clone();
        self.index.remove(&key);
        self.free.push(victim);
        self.stats.evicted += 1;
    }

    /// Take a slot for a new client, reusing an evicted one where possible.
    fn alloc_slot(&mut self, key: K, record: ClientRecord) -> u32 {
        let i = match self.free.pop() {
            Some(i) => {
                let slot = &mut self.slots[i as usize];
                slot.key = key;
                slot.record = record;
                slot.prev = NIL;
                slot.next = NIL;
                // New occupant, new generation: any handle still naming this
                // slot from its previous client now fails to resolve.
                slot.generation = slot.generation.wrapping_add(1);
                i
            }
            None => {
                self.slots.push(Slot {
                    key,
                    record,
                    prev: NIL,
                    next: NIL,
                    generation: 0,
                });
                (self.slots.len() - 1) as u32
            }
        };
        self.link_front(i);
        i
    }

    /// Turn a handle back into a slot, or `None` if it has gone stale.
    fn resolve(&self, handle: ClientHandle) -> Option<usize> {
        let i = handle.slot as usize;
        let slot = self.slots.get(i)?;
        (slot.generation == handle.generation).then_some(i)
    }

    fn handle_for(&self, slot: u32) -> ClientHandle {
        ClientHandle {
            slot,
            generation: self.slots[slot as usize].generation,
        }
    }

    /// Admit one request: refill the client's bucket, decide its fate, and
    /// record it. `now` is monotonic seconds.
    pub fn admit(&mut self, key: &K, now: f64) -> Disposition {
        self.admit_handle(key, now).0
    }

    /// `admit`, also returning the handle that addresses this client, so the
    /// rest of the request never has to hash the key again.
    pub fn admit_handle(&mut self, key: &K, now: f64) -> (Disposition, ClientHandle) {
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
                    return (Disposition::KissOfDeath, ClientHandle::INVALID);
                }
                return (Disposition::Drop, ClientHandle::INVALID);
            }
        }

        // One hash for the common case (a client we already know), two for a
        // client we have never seen. The previous form hashed three times per
        // request: `contains_key`, then `touch`'s `get_mut`, then a final
        // `get_mut` to reach the record.
        let slot = match self.index.get(key) {
            Some(&i) => {
                self.touch(i);
                i
            }
            None => {
                if self.index.len() >= self.capacity {
                    self.evict_one();
                }
                let i = self.alloc_slot(key.clone(), ClientRecord::new(now, self.config.burst));
                self.index.insert(key.clone(), i);
                i
            }
        };

        let config = self.config;
        let rate = self.refill_per_s;
        let handle = self.handle_for(slot);
        let record = &mut self.slots[slot as usize].record;

        // Refill: one token per 2^interval seconds since we last saw them.
        let elapsed = (now - record.last_seen).max(0.0);
        record.tokens = (record.tokens + elapsed * rate).min(config.burst as f64);
        record.last_seen = now;
        record.requests += 1;

        if record.tokens >= 1.0 {
            record.tokens -= 1.0;
            record.responses += 1;
            self.stats.responses += 1;
            // Spend a global token only when a response is actually produced.
            self.global_tokens -= 1.0;
            return (Disposition::Respond, handle);
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
            (Disposition::KissOfDeath, handle)
        } else {
            (Disposition::Drop, handle)
        }
    }

    /// Decide basic vs interleaved for an admitted request.
    ///
    /// The client signals interleaved mode by setting its origin timestamp to
    /// the *receive* timestamp we sent last time, rather than the transmit
    /// timestamp. That is unforgeable in the useful sense: only a client that
    /// actually saw our last response knows it.
    pub fn response_mode(&mut self, key: &K, request_origin: NtpTimestamp) -> ResponseMode {
        match self.index.get(key) {
            Some(&i) => {
                let handle = self.handle_for(i);
                self.response_mode_at(handle, request_origin)
            }
            None => ResponseMode::Basic,
        }
    }

    /// `response_mode` addressed by handle — no hashing.
    pub fn response_mode_at(
        &mut self,
        handle: ClientHandle,
        request_origin: NtpTimestamp,
    ) -> ResponseMode {
        // One lookup, not two. The original read the record with `get` and then
        // re-found the same entry with `get_mut` to store `interleaved_now` —
        // a second hash of the same key on the per-request path, where hashing
        // was measured at ~38% of all instructions.
        let Some(i) = self.resolve(handle) else {
            return ResponseMode::Basic;
        };
        let record = &mut self.slots[i].record;
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
        record.interleaved_now = interleaved;
        if interleaved {
            self.stats.interleaved_responses += 1;
            ResponseMode::Interleaved { prev_transmit }
        } else {
            ResponseMode::Basic
        }
    }

    /// Record what we received and what we told the client, after answering.
    pub fn note_response(&mut self, key: &K, receive: NtpTimestamp, receive_sent: NtpTimestamp) {
        if let Some(&i) = self.index.get(key) {
            let handle = self.handle_for(i);
            self.note_response_at(handle, receive, receive_sent);
        }
    }

    /// `note_response` addressed by handle — no hashing.
    pub fn note_response_at(
        &mut self,
        handle: ClientHandle,
        receive: NtpTimestamp,
        receive_sent: NtpTimestamp,
    ) {
        if let Some(i) = self.resolve(handle) {
            let record = &mut self.slots[i].record;
            record.last_receive = Some(receive);
            record.last_receive_sent = Some(receive_sent);
        }
    }

    /// Record the true transmit timestamp of the response just sent. Called
    /// after `send`, which is the whole point of interleaved mode — this is a
    /// timestamp the basic exchange cannot report because the packet has not
    /// left yet when its own transmit field is written.
    pub fn note_transmit(&mut self, key: &K, transmit: NtpTimestamp) {
        if let Some(&i) = self.index.get(key) {
            self.slots[i as usize].record.last_transmit = Some(transmit);
        }
    }

    /// `note_transmit` addressed by handle — no hashing.
    ///
    /// This is the one called after `send`, so a handle taken before the write
    /// is used after it. The generation check is what makes that safe: if the
    /// client was evicted in between, the update is dropped rather than landing
    /// on whoever inherited the slot.
    pub fn note_transmit_at(&mut self, handle: ClientHandle, transmit: NtpTimestamp) {
        if let Some(i) = self.resolve(handle) {
            self.slots[i].record.last_transmit = Some(transmit);
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
