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

pub mod client;
pub mod config;
pub mod discipline;
pub mod filter;
pub mod ntp;
pub mod refclock;
pub mod select;
pub mod server;
pub mod vclock;

pub use discipline::{ClockCommand, Discipline, DisciplineConfig, LeapMode, Plan};
pub use filter::{RegressEstimate, Sample, SampleRegister};
pub use ntp::{LeapIndicator, Mode, NtpPacket, NtpShort, NtpTimestamp, ParseError};
pub use select::{Selection, SourceEstimate};
pub use server::{
    ClientHandle, ClientRecord, ClientTable, Disposition, RateLimitConfig, ResponseMode,
    ServerStats,
};
pub use vclock::VirtualClock;
