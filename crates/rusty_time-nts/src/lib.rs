//! rusty_time-nts — the NTS (RFC 8915) building blocks that need no TLS stack:
//! the NTS-KE record codec and the AES-SIV-CMAC-256 extension-field AEAD.
//!
//! The NTS-KE TLS handshake itself (rustls) lands in M3; everything here is
//! transport-independent and fuzz-hardened.

pub mod aead;
pub mod cookie;
pub mod ef;
pub mod records;

/// NTS-KE over TLS 1.3. Behind the `ke` feature: it needs sockets and a TLS
/// stack, so wasm and codec-only consumers never pull one.
#[cfg(feature = "ke")]
pub mod ke;

/// The TLS seam — the single place the crypto provider is chosen.
#[cfg(feature = "ke")]
pub mod tls;

/// ALPN identifier for NTS-KE (RFC 8915 §4).
pub const ALPN: &[u8] = b"ntske/1";
/// NTS-KE TCP port (RFC 8915 §4).
pub const KE_PORT: u16 = 4460;
/// AEAD algorithm identifier for AES-SIV-CMAC-256 (RFC 5297 via IANA registry).
pub const AEAD_AES_SIV_CMAC_256: u16 = 15;
/// Next Protocol identifier for NTPv4.
pub const NEXT_PROTO_NTPV4: u16 = 0;
