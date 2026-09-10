# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-09-09

### Changed

- Version moved to 0.2.0 with the rest of the workspace. **Nothing in this
  crate's own API changed.** The minor bump belongs to `rusty_time-core`,
  which gained a `no_std` leaf and now gates its `std`-only modules behind a
  default-on `std` feature — a break only for a consumer that was already
  writing `default-features = false`. See that crate's changelog.

## [0.1.10](https://github.com/Remade-With-Rust/rusty_time/compare/rusty_time-alloc-v0.1.9...rusty_time-alloc-v0.1.10) - 2026-08-28

### Other

- bump rusty_alloc-api to =1.1.6 ([#2](https://github.com/Remade-With-Rust/rusty_time/pull/2))
