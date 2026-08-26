//! rusty_time-nts — the NTS (RFC 8915) building blocks that need no TLS stack:
//! the NTS-KE record codec and the AES-SIV-CMAC-256 extension-field AEAD.
//!
//! The NTS-KE TLS handshake itself (rustls) lands in M3; everything here is
//! transport-independent and fuzz-hardened.

pub mod aead;
pub mod records;

/// ALPN identifier for NTS-KE (RFC 8915 §4).
pub const ALPN: &[u8] = b"ntske/1";
/// NTS-KE TCP port (RFC 8915 §4).
pub const KE_PORT: u16 = 4460;
/// AEAD algorithm identifier for AES-SIV-CMAC-256 (RFC 5297 via IANA registry).
pub const AEAD_AES_SIV_CMAC_256: u16 = 15;
/// Next Protocol identifier for NTPv4.
pub const NEXT_PROTO_NTPV4: u16 = 0;
