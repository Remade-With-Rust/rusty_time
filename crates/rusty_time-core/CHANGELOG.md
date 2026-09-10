# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

