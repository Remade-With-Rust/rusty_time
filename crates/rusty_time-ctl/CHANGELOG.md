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
