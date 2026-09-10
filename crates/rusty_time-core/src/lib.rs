#![cfg_attr(not(feature = "std"), no_std)]
//! rusty_time-core — portable NTP protocol and clock-discipline algorithms.
//!
//! This crate knows bytes, timestamps, and estimates. It performs no I/O, reads no
//! OS clock, and holds no product types: a developer who has never heard of the rest
//! of the workspace can drive it. All platform work lives behind the seams in
//! `rusty_time-clock`; all wire transport lives in the deliverables.
//!
//! Sign convention used everywhere: an offset is **the number of seconds to ADD to
//! the local clock** to match the source (RFC 5905 θ). Positive offset = local is
//! behind.
//!
//! # The `no_std` leaf
//!
//! [`ntp`] compiles on every target with no `std` and no `alloc`: the NTPv4 packet
//! codec ([`NtpPacket`], [`NtpTimestamp`], [`NtpShort`]), RFC 7822 extension-field
//! iteration, and the RFC 5905 §8 offset/delay arithmetic. It reads no clock, opens
//! no socket and owns no buffer, so an SNTP client on a Cortex-M4F or RV32 part can
//! build a request, parse the response and compute an offset with nothing else.
//!
//! Everything above the wire — the regression sample filter, the discipline loop,
//! falseticker selection, the server's client table, the config parser and the
//! reference and virtual clocks — needs `Vec`/`String` and stays behind the
//! default-on `std` feature. Take the leaf alone with:
//!
//! ```toml
//! rusty_time-core = { version = "0.1", default-features = false }
//! ```

pub mod ntp;

#[cfg(feature = "std")]
pub mod client;
#[cfg(feature = "std")]
pub mod config;
#[cfg(feature = "std")]
pub mod discipline;
#[cfg(feature = "std")]
pub mod filter;
#[cfg(feature = "std")]
pub mod refclock;
#[cfg(feature = "std")]
pub mod select;
#[cfg(feature = "std")]
pub mod server;
#[cfg(feature = "std")]
pub mod vclock;

pub use ntp::{LeapIndicator, Mode, NtpPacket, NtpShort, NtpTimestamp, ParseError};

#[cfg(feature = "std")]
pub use discipline::{ClockCommand, Discipline, DisciplineConfig, LeapMode, Plan};
#[cfg(feature = "std")]
pub use filter::{RegressEstimate, Sample, SampleRegister};
#[cfg(feature = "std")]
pub use select::{Selection, SourceEstimate};
#[cfg(feature = "std")]
pub use server::{
    ClientHandle, ClientRecord, ClientTable, Disposition, RateLimitConfig, ResponseMode,
    ServerStats,
};
#[cfg(feature = "std")]
pub use vclock::VirtualClock;
