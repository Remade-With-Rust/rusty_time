# rusty_time

chrony, remade with Rust. A pure-Rust, memory-safe NTP/NTS client + server targeting
Linux, macOS, Windows, and wasm.

- Mission plan: [docs/plans/time_mission.md](docs/plans/time_mission.md)
- Performance ledger: [corpus/LEDGER.md](corpus/LEDGER.md)

**Claims discipline:** this README makes no performance claim that is not in the ledger
with the run that produced it. The corpus (TIMECORP) measures rusty_time against chrony
(primary baseline) and ntpd-rs (secondary); see the mission plan §7.

## Status

Pre-1.0, milestone **M3** of the mission plan complete: workspace scaffold, NTPv4
packet codec, regression sample filter, source selection, discipline loop, the
deterministic TIMECORP simulation harness, and **NTS (RFC 8915) client and
server** with SpaceDB-backed persistence.

NTS interop passes in both directions — `rtimed` authenticates against chronyd
and time.cloudflare.com, and chronyd selects an `rtimed` server as its
synchronisation source (see [corpus/LEDGER.md](corpus/LEDGER.md)). TLS is rustls
on a pure-Rust crypto provider; nothing in the build needs a C toolchain, on any
of the eight supported targets.

```sh
# Authenticated query
rtimed query time.cloudflare.com --nts

# Run an NTS-capable server (self-signs a dev certificate if none is given)
rtimed serve --nts --state /var/lib/rusty_time/state.spacedb
```

## Layout

```
crates/rusty_time-core/    protocol + algorithms (portable, wasm-clean)
crates/rusty_time-nts/     NTS record layer + AEAD (RFC 8915)
crates/rusty_time-clock/   platform clock seam (linux / macos / windows / virtual)
crates/rusty_time-api/     typed op/report types (JSON public wire)
crates/rusty_time-daemon/  deliverable: rtimed
crates/rusty_time-ctl/     deliverable: rtimec
crates/rusty_time-wasm/    virtual clock for wasm targets
crates/rusty_time-alloc/   the allocator seam
crates/rusty_time-sim/     deliverable: timecorp — deterministic corpus runner
corpus/                    scenarios, results, LEDGER.md
fuzz/                      cargo-fuzz targets (packet, config, NTS records)
```

## License

MIT OR Apache-2.0. Clean-room implementation from RFCs 5905/7822/8915 and published
algorithm descriptions; no chrony (GPLv2) source is used.
