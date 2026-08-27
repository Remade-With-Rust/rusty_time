//! The server hot path, as a deterministic instruction-count workload.
//!
//! This is the harness every optimisation in this area is measured with. It is
//! deliberately NOT a timing benchmark: the wins being chased are individually
//! well under 1% of a wall clock, and at that size a clock cannot be promoted
//! to the verdict no matter how many pairs are run. A counter can. So this
//! binary is built to be run under `valgrind --tool=callgrind`, whose Ir
//! (instruction read) count is exact, reproducible, and attributable per
//! function.
//!
//! Everything here is deterministic by construction: a fixed client
//! population, an LCG that picks clients from a fixed seed, and a simulated
//! monotonic clock that advances by a fixed step. No system clock, no
//! allocation that depends on timing, no randomness. Two runs of the same
//! binary produce the same Ir count to the instruction.
//!
//! It also prints a checksum over every byte of every reply. That is the
//! correctness gate: this is an integer/exact path, so any optimisation here
//! must leave the checksum **byte-identical**. A speed number without its gate
//! is not evidence.
//!
//! Run:
//!   cargo build --release --bench hot_path
//!   valgrind --tool=callgrind --callgrind-out-file=cg.out ./hot_path
//!   callgrind_annotate cg.out

use rusty_time_core::ntp::{self, HEADER_LEN, LeapIndicator, Mode, NtpPacket, NtpTimestamp};
use rusty_time_core::server::{ClientTable, Disposition, RateLimitConfig, ResponseMode};
use std::net::{IpAddr, Ipv4Addr};

/// How many requests one pass serves.
const REQUESTS: usize = 200_000;
/// Distinct client addresses. Wide enough that the table is exercised as a
/// table rather than as one hot entry, which is what a real server sees.
const CLIENTS: u32 = 4_096;

/// A deterministic client picker. An LCG, not a real RNG: reproducibility is
/// the whole point, and `rand` would pull entropy from the OS.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        // Numerical Recipes constants; any full-period LCG works here.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

fn client_addr(index: u32) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(0x0a00_0000 | index))
}

/// A realistic client request: mode 3, version 4, a transmit timestamp.
fn client_request(transmit: NtpTimestamp) -> [u8; HEADER_LEN] {
    NtpPacket {
        leap: LeapIndicator::NoWarning,
        version: 4,
        mode: Mode::Client,
        stratum: 0,
        poll: 6,
        precision: -20,
        root_delay: ntp::NtpShort(0),
        root_dispersion: ntp::NtpShort(0),
        reference_id: [0; 4],
        reference_ts: NtpTimestamp::ZERO,
        origin_ts: NtpTimestamp::ZERO,
        receive_ts: NtpTimestamp::ZERO,
        transmit_ts: transmit,
    }
    .to_bytes()
}

fn main() {
    // `REQUESTS` env override, so the same binary can serve two jobs: a short
    // pass under callgrind (where Ir is the verdict) and a long pass on the
    // clock (where a few hundred instructions per request is otherwise far too
    // little work to time at all -- the default pass measures 0.00 s).
    let requests: usize = std::env::var("HOT_PATH_REQUESTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(REQUESTS);

    // Generous limits: this measures the cost of ANSWERING requests, so the
    // limiter must mostly admit. A config that dropped everything would
    // benchmark the reject path and report a flattering number for work the
    // server does not do when it is busy.
    let config = RateLimitConfig {
        interval_log2: -6,
        burst: 64,
        leak_shift: 4,
        global_rate_hz: 0.0,
        global_burst: 1e9,
    };
    let mut table: ClientTable<IpAddr> = ClientTable::new(CLIENTS as usize * 2, config);

    let mut lcg = Lcg(0x0123_4567_89ab_cdef);
    let mut checksum: u64 = 0;
    let mut answered: u64 = 0;
    let mut kod: u64 = 0;
    let mut dropped: u64 = 0;
    // A simulated monotonic clock. Fixed step, so the token buckets evolve
    // identically on every run and on every machine.
    let mut now = 1_000.0f64;

    for i in 0..requests {
        now += 1e-4;
        let key = client_addr((lcg.next() as u32) % CLIENTS);
        let recv_ts = NtpTimestamp::from_unix(
            1_800_000_000 + (i as i64 / 1000),
            (i as u32 % 1000) * 1_000_000,
        );
        let request = client_request(NtpTimestamp(0x1234_5678_9abc_def0 ^ i as u64));

        // --- exactly the sequence the daemon runs per request ---
        let parsed = match NtpPacket::parse(&request) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if parsed.mode != Mode::Client {
            continue;
        }
        // Hash the key once; everything after addresses the client by handle,
        // which is exactly what the daemon does.
        let (disposition, handle) = table.admit_handle(&key, now);
        let mode = if disposition == Disposition::Respond {
            table.response_mode_at(handle, parsed.origin_ts)
        } else {
            ResponseMode::Basic
        };
        match disposition {
            Disposition::Drop => {
                dropped += 1;
                continue;
            }
            Disposition::KissOfDeath => {
                kod += 1;
                continue;
            }
            Disposition::Respond => {}
        }

        let (mut receive_field, mut transmit_field) = match mode {
            ResponseMode::Basic => (recv_ts, recv_ts),
            ResponseMode::Interleaved { prev_transmit } => (recv_ts, prev_transmit),
        };
        rusty_time_core::server::mark_server_timestamps(&mut receive_field, &mut transmit_field);
        let origin_field = match mode {
            ResponseMode::Basic => parsed.transmit_ts,
            ResponseMode::Interleaved { .. } => parsed.receive_ts,
        };

        let header = NtpPacket {
            leap: LeapIndicator::NoWarning,
            version: parsed.version,
            mode: Mode::Server,
            stratum: 2,
            poll: parsed.poll,
            precision: -20,
            root_delay: ntp::NtpShort(0),
            root_dispersion: ntp::NtpShort::from_seconds(0.000_1),
            reference_id: *b"RSTY",
            reference_ts: recv_ts,
            origin_ts: origin_field,
            receive_ts: receive_field,
            transmit_ts: transmit_field,
        };
        let reply = header.to_bytes();
        table.note_response_at(handle, recv_ts, receive_field);
        table.note_transmit_at(handle, transmit_field);

        // Fold every reply byte in, so nothing above can be eliminated as dead
        // and so the gate covers the whole output rather than a length.
        //
        // Folded eight bytes at a time rather than one. A byte-wise loop over
        // a 48-byte reply is 48 iterations per request, which callgrind priced
        // at 68 Ir/request -- about 20% of the whole measurement once the
        // client table had been fixed. A measuring tap that large distorts
        // every share derived from it, so it is sized down here; coverage is
        // unchanged, every byte still enters the checksum.
        for chunk in reply.chunks_exact(8) {
            let word = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
            checksum = checksum.wrapping_mul(0x0100_0000_01b3).wrapping_add(word);
        }
        answered += 1;
    }

    let stats = table.stats;
    println!("requests   {requests}");
    println!("answered   {answered}");
    println!("kod        {kod}");
    println!("dropped    {dropped}");
    println!("evicted    {}", stats.evicted);
    println!("table_len  {}", table.len());
    println!("CHECKSUM   {checksum:#018x}");
}
