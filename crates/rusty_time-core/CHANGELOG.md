# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-10-08

### BREAKING

- `ClientRecord`'s three timestamp fields change type:
  `pub last_receive`, `pub last_transmit` and `pub last_receive_sent` are now
  `NtpTimestamp` rather than `Option<NtpTimestamp>`, with
  `NtpTimestamp::ZERO` meaning "unset" — which is how RFC 5905 already spells
  it on the wire. `Option<NtpTimestamp>` was sixteen bytes for eight bytes of
  payload, because `NtpTimestamp` is a bare `u64` with no niche, and every read
  tested a discriminant.

  **Migration:** `if let Some(ts) = rec.last_receive` becomes
  `if !rec.last_receive.is_zero()`, and `rec.last_receive` is the value
  directly.

  This is why the release is 0.3.0 and not 0.2.1. Note that
  `cargo semver-checks` reports *no semver update required* for it — it has no
  lint for a public field's TYPE changing — so the bump was decided by reading
  the diff, not by the tool. Do not trust that tool alone on a field-type
  change.

### Performance

Six deterministic wins in the client filter and the server's client table,
measured with callgrind Ir (exact: the same binary re-runs to zero) and gated
byte-identical throughout.

| instrument | before | after | delta |
|---|---:|---:|---:|
| `client_path` | 219,439,512 | 194,470,797 | **-24,968,715 (-11.38%)** |
| `hot_path` (steady) | 47,424,979 | 45,664,884 | **-1,760,095 (-3.71%)** |
| `hot_path` (churn) | 75,642,533 | 67,786,128 | **-7,856,405 (-10.39%)** |
| `mru_report` | 5,804,467 | 5,234,883 | **-569,584 (-9.81%)** |

- `regress` called `wls_fit` twice on identical data (-17,918,178).
- The spike filter recomputed residuals `spike_threshold` had already produced;
  they travel as a `u64` keep mask now (-2,441,711).
- A median taken by full sort now uses `select_nth_unstable_by` (-3,363,768).
- `2f64.powi(k)` builds the exponent field directly instead of calling
  `__powidf2` 16,000 times a run (-690,184).
- The regime-change trim advances a cursor instead of front-draining up to four
  times (-557,376).
- The client index is open-addressed with backward-shift deletion (so no
  tombstones) and does not duplicate the key, which `HashMap` had to. Costs
  +10 bytes/client (150 -> 160).

**No behaviour changed.** TIMECORP S1/S6/S8 at 31 seeds are byte-identical to
0.2.0, and S12a/b/c server-load counts are identical including 1,021,672
evictions in S12b. `cargo semver-checks`: no semver update required.

### Changed

- `ClientTable::bytes_per_client()` reports 160 rather than 150, because the
  index now holds four buckets per client instead of one entry. The signature
  is unchanged; the number is honest.

### Added

- `HOT_PATH_EVICT`, a churn arm for the `hot_path` bench. The default arm never
  evicts, so it could not gate any change to the client index -- and at a 0.5
  load factor the new index measured a clean win there while costing +9.86%
  under eviction. The arm is what caught it.

## [0.2.0] - 2026-09-09

### Added

- **A `no_std` leaf.** `ntp` — the NTPv4 packet codec, RFC 7822 extension
  iteration and the RFC 5905 §8 offset/delay arithmetic — now builds with no
  `std` and no `alloc`, so an SNTP client on a Cortex-M4F or RV32 part can use
  the house NTP engine directly. Take it with `default-features = false`.
- CI rungs on `thumbv7em-none-eabihf` and `riscv32imac-unknown-none-elf`, plus
  the leaf's own test suite run against the `no_std` code path on the host, so
  the crate's `no-std` category cannot rot back into a label.
- `bare-metal/esp32s3/` — a hand-run board row: the leaf building a request,
  parsing a response and computing an offset on an ESP32-S3, with no heap
  linked at all.

### Changed

- `default = ["std"]`. `filter`, `select`, `discipline`, `client`, `server`,
  `config`, `refclock` and `vclock` are behind that feature, because they need
  `Vec`/`String`. **Default builds are unaffected** — every existing dependant
  takes default features and sees the same crate it always did
  (`cargo semver-checks` against 0.1.10: 196 checks, no semver update
  required).

  **This is why the release is 0.2.0 and not 0.1.11.** Under Cargo's 0.x
  rules `0.1.10` and `0.1.11` are compatible, so a caret requirement
  (`rusty_time-core = "0.1"`) paired with `default-features = false` would
  have picked the narrower crate up silently on the next `cargo update`. A
  minor bump makes the one breaking arm an explicit choice by the consumer.

  The one arm that narrows is `default-features = false`. Before this release
  the crate had no `[features]` table at all, so that spelling was a silent
  no-op on a host and did not build for any bare-metal target — which is the
  defect this release fixes. It now selects the leaf, which is what a caller
  writing it meant.
- `ParseError` now implements `core::error::Error` instead of
  `std::error::Error`. The two have been the same trait since Rust 1.81, so
  hosted callers see no change; `no_std` callers gain the impl rather than
  losing it, which gating it behind `std` would have cost them.

