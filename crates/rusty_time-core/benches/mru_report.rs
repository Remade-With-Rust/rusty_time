//! The MRU status report, as a deterministic instruction count.
//!
//! `rtimec` asks for the most recently seen clients; the daemon answers from a
//! table holding up to 16384 of them. This measures what that answer costs.
//!
//! The `old` arm reproduces the previous implementation verbatim — clone every
//! record, sort them all by `last_seen`, keep the first few — so the two arms
//! are a real A/B rather than an argument about complexity. Both print the
//! same rows, which is the correctness gate: the recency list and a sort by
//! `last_seen` must agree, or the new form is not a drop-in.
//!
//! Run:
//!   ARM=old  cargo build --release --bench mru_report && valgrind ...
//!   ARM=new  ...

use rusty_time_core::server::{ClientRecord, ClientTable, RateLimitConfig};
use std::net::{IpAddr, Ipv4Addr};

const CAPACITY: usize = 16_384;
const REPORTS: usize = 2_000;
const LIMIT: usize = 10;

/// The previous implementation, kept here so the comparison is measured.
fn most_recent_by_sort(
    table: &ClientTable<IpAddr>,
    keys: &[IpAddr],
    limit: usize,
) -> Vec<(IpAddr, ClientRecord)> {
    let mut all: Vec<(IpAddr, ClientRecord)> = keys
        .iter()
        .filter_map(|k| table.get(k).map(|r| (*k, *r)))
        .collect();
    all.sort_by(|a, b| b.1.last_seen.total_cmp(&a.1.last_seen));
    all.truncate(limit);
    all
}

fn main() {
    let arm = std::env::var("ARM").unwrap_or_else(|_| "new".to_string());
    let config = RateLimitConfig {
        interval_log2: -6,
        burst: 64,
        leak_shift: 4,
        global_rate_hz: 0.0,
        global_burst: 1e9,
    };
    let mut table: ClientTable<IpAddr> = ClientTable::new(CAPACITY, config);

    // Fill the table, so the report is answered from a full one.
    let mut keys = Vec::with_capacity(CAPACITY);
    let mut now = 1_000.0f64;
    for i in 0..CAPACITY {
        let key = IpAddr::V4(Ipv4Addr::from(0x0a00_0000 | i as u32));
        now += 1e-3;
        table.admit(&key, now);
        keys.push(key);
    }

    let mut checksum: u64 = 0;
    for _ in 0..REPORTS {
        let rows = match arm.as_str() {
            "old" => most_recent_by_sort(&table, &keys, LIMIT),
            _ => table.most_recent(LIMIT),
        };
        for (key, record) in &rows {
            if let IpAddr::V4(v4) = key {
                checksum = checksum
                    .wrapping_mul(0x0100_0000_01b3)
                    .wrapping_add(u32::from(*v4) as u64);
            }
            checksum = checksum.wrapping_add(record.requests);
        }
    }

    println!("arm        {arm}");
    println!("reports    {REPORTS}");
    println!("table_len  {}", table.len());
    println!("CHECKSUM   {checksum:#018x}");
}
