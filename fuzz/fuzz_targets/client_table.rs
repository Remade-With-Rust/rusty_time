#![no_main]
//! The server's per-client table, driven by an arbitrary operation sequence.
//!
//! This is the structure that faces the open internet: one entry per source
//! address, bounded capacity, LRU eviction, and a rate limiter. It is also the
//! structure with the most dangerous failure mode in the daemon, which is why
//! it is worth fuzzing beyond the parsers.
//!
//! `admit_handle` hands back a `ClientHandle { slot, generation }` so the three
//! follow-up calls can address the record by index instead of re-hashing the
//! address. That is a deliberate optimisation, and it puts an index into the
//! caller's hands that outlives the thing it points at: between taking a handle
//! and using it, the client can be evicted and its slot recycled for somebody
//! else. If the generation check were wrong — or missing — a stale handle would
//! write one client's timestamps into another client's record. On a server that
//! uses those timestamps to decide interleaved-mode responses, that is
//! cross-client state confusion reachable from unauthenticated packets.
//!
//! So the invariants asserted here are:
//!
//!   * the table never exceeds its capacity, however hard eviction churns;
//!   * a handle NEVER writes to a record belonging to a different key;
//!   * no sequence panics.
//!
//! The capacity is deliberately tiny relative to the key space, so eviction is
//! the common case rather than a rare one.

use libfuzzer_sys::fuzz_target;
use rusty_time_core::ntp::NtpTimestamp;
use rusty_time_core::server::{ClientTable, RateLimitConfig};

/// Small enough that a few dozen distinct keys churn the table constantly.
const CAPACITY: usize = 8;

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }

    let mut table: ClientTable<u16> = ClientTable::new(CAPACITY, RateLimitConfig::default());
    // A handle taken earlier, deliberately used long after the client it was
    // issued for may have been evicted.
    let mut stale: Option<(u16, rusty_time_core::server::ClientHandle)> = None;
    let mut now = 0.0f64;

    for op in data.chunks(4) {
        if op.len() < 4 {
            break;
        }
        // Keys drawn from a space far wider than the table, so eviction churns.
        let key = u16::from(op[1]) % 64;
        now += f64::from(op[2]) / 64.0;
        let ts = NtpTimestamp(u64::from(op[3]) << 24);

        match op[0] % 5 {
            0 => {
                let (_disposition, handle) = table.admit_handle(&key, now);
                // Keep the FIRST handle we are given and never refresh it, so
                // that later operations exercise the stale case on purpose.
                if stale.is_none() {
                    stale = Some((key, handle));
                }
            }
            1 => {
                let _ = table.admit(&key, now);
            }
            2 => {
                let _ = table.response_mode(&key, ts);
            }
            3 => {
                table.note_response(&key, ts, ts);
            }
            _ => {
                // Use the stale handle. If its slot has been recycled, the
                // generation check must make this a no-op for whoever now owns
                // the slot.
                if let Some((issued_for, handle)) = stale {
                    let before: Vec<(u16, Option<NtpTimestamp>)> = (0..64u16)
                        .filter(|k| *k != issued_for)
                        .filter_map(|k| table.get(&k).map(|r| (k, r.last_receive)))
                        .collect();

                    table.note_response_at(handle, ts, ts);
                    table.note_transmit_at(handle, ts);

                    for (k, was) in before {
                        let now_is = table.get(&k).and_then(|r| r.last_receive);
                        assert_eq!(
                            was, now_is,
                            "a handle issued for key {issued_for} altered key {k}: \
                             the generation guard did not catch a recycled slot"
                        );
                    }
                }
            }
        }

        assert!(
            table.len() <= CAPACITY,
            "table holds {} entries against a capacity of {CAPACITY}",
            table.len()
        );
    }

    // Whatever the sequence did, the reporting path must still work and must
    // respect its own limit.
    let recent = table.most_recent(4);
    assert!(recent.len() <= 4, "most_recent returned {}", recent.len());
    assert!(recent.len() <= table.len());
});
