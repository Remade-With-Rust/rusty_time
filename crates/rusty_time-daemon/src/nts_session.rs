//! Client-side NTS session state: the cookie store and per-exchange protection.
//!
//! Cookie hygiene is the privacy property NTS exists for (RFC 8915 §5.7): a
//! cookie is spent exactly once and replaced by a fresh one from the encrypted
//! part of the reply, so an on-path observer cannot link two queries to the
//! same client. The store therefore *pops* on send and *pushes* only what a
//! verified response yielded — a failed exchange loses that cookie for good,
//! which is why we ask for replacements every time.

use rusty_time_nts::aead::NtsKeys;
use rusty_time_nts::ef::{self, NONCE_LEN, UNIQUE_ID_LEN};
use rusty_time_nts::ke;
use std::collections::VecDeque;
use std::time::Duration;

/// Cookies the client aims to hold. RFC 8915 §5.4 recommends 8.
const TARGET_COOKIES: usize = 8;

pub struct NtsSession {
    keys: NtsKeys,
    cookies: VecDeque<Vec<u8>>,
    /// The unique identifier sent with the in-flight request.
    pending_unique_id: [u8; UNIQUE_ID_LEN],
    /// Where NTS-KE told us to send NTP (may differ from the KE host).
    pub ntp_server: String,
    pub ntp_port: u16,
    pub ke_host: String,
}

#[derive(Debug)]
pub enum NtsSessionError {
    Ke(ke::KeError),
    /// Every cookie is spent; NTS-KE must be re-run.
    CookiesExhausted,
    Protect(ef::NtsError),
}

impl std::fmt::Display for NtsSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NtsSessionError::Ke(e) => write!(f, "{e}"),
            NtsSessionError::CookiesExhausted => {
                write!(f, "NTS cookies exhausted; re-run key establishment")
            }
            NtsSessionError::Protect(e) => write!(f, "{e}"),
        }
    }
}

impl NtsSession {
    /// Run NTS-KE and open a session. `extra_roots` are additional trust
    /// anchors (a private CA or pinned server cert); the KE server is verified
    /// either way.
    pub fn establish(
        ke_host: &str,
        ke_port: u16,
        timeout: Duration,
        extra_roots: &[rusty_time_nts::tls::pki_types::CertificateDer<'static>],
    ) -> Result<Self, NtsSessionError> {
        let result = ke::establish_with_roots(ke_host, ke_port, timeout, extra_roots)
            .map_err(NtsSessionError::Ke)?;
        Ok(NtsSession {
            keys: result.keys,
            cookies: result.cookies.into(),
            pending_unique_id: [0u8; UNIQUE_ID_LEN],
            ntp_server: result.ntp_server,
            ntp_port: result.ntp_port,
            ke_host: ke_host.to_string(),
        })
    }

    pub fn cookies_held(&self) -> usize {
        self.cookies.len()
    }

    /// The session keys. Needed to persist a resumable session — a saved
    /// cookie without these cannot be used.
    pub fn keys(&self) -> &NtsKeys {
        &self.keys
    }

    /// Cookies not yet spent, in spend order.
    pub fn unspent_cookies(&self) -> Vec<Vec<u8>> {
        self.cookies.iter().cloned().collect()
    }

    /// Rebuild a session from persisted state, skipping NTS-KE entirely.
    pub fn resume(
        keys: NtsKeys,
        cookies: Vec<Vec<u8>>,
        ntp_server: String,
        ntp_port: u16,
        ke_host: String,
    ) -> Self {
        NtsSession {
            keys,
            cookies: cookies.into(),
            pending_unique_id: [0u8; UNIQUE_ID_LEN],
            ntp_server,
            ntp_port,
            ke_host,
        }
    }

    /// Build a session from keys and cookies obtained some other way — used by
    /// the loopback tests that exercise the client and server halves against
    /// each other without a TLS handshake.
    #[cfg(test)]
    pub fn for_test(keys: NtsKeys, cookies: Vec<Vec<u8>>) -> Self {
        NtsSession {
            keys,
            cookies: cookies.into(),
            pending_unique_id: [0u8; UNIQUE_ID_LEN],
            ntp_server: "loopback".into(),
            ntp_port: 123,
            ke_host: "loopback".into(),
        }
    }

    /// Wrap an NTP header into an NTS-protected request, spending one cookie
    /// and asking for enough replacements to refill the store.
    pub fn protect(&mut self, header: &[u8; 48]) -> Result<Vec<u8>, NtsSessionError> {
        let cookie = self
            .cookies
            .pop_front()
            .ok_or(NtsSessionError::CookiesExhausted)?;

        let mut unique_id = [0u8; UNIQUE_ID_LEN];
        let mut nonce = [0u8; NONCE_LEN];
        ke::fill_random(&mut unique_id).map_err(NtsSessionError::Ke)?;
        ke::fill_random(&mut nonce).map_err(NtsSessionError::Ke)?;
        self.pending_unique_id = unique_id;

        // One placeholder replaces the cookie just spent; more refill toward
        // the target. Capped so a request cannot be inflated into an
        // amplification lever.
        let want = TARGET_COOKIES.saturating_sub(self.cookies.len()).min(7);

        ef::protect_request(header, &unique_id, &cookie, want, &self.keys.c2s, &nonce)
            .map_err(NtsSessionError::Protect)
    }

    /// Verify a response against the in-flight request and bank its cookies.
    pub fn verify(&mut self, packet: &[u8]) -> Result<(), NtsSessionError> {
        let verified = ef::verify_response(packet, &self.keys.s2c, &self.pending_unique_id)
            .map_err(NtsSessionError::Protect)?;
        for cookie in verified.new_cookies {
            if self.cookies.len() < TARGET_COOKIES * 2 {
                self.cookies.push_back(cookie);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_spends_a_cookie_and_refuses_when_empty() {
        // Build a session without touching the network.
        let mut s = NtsSession {
            keys: NtsKeys {
                c2s: [1; 32],
                s2c: [2; 32],
            },
            cookies: vec![vec![0xAB; 32]].into(),
            pending_unique_id: [0; UNIQUE_ID_LEN],
            ntp_server: "example".into(),
            ntp_port: 123,
            ke_host: "example".into(),
        };
        let mut header = [0u8; 48];
        header[0] = (4 << 3) | 3;

        assert_eq!(s.cookies_held(), 1);
        let packet = s.protect(&header).expect("protect");
        assert_eq!(s.cookies_held(), 0, "cookie must be spent exactly once");
        assert!(packet.len() > 48);
        // The unique id is fresh randomness, not the zero placeholder.
        assert_ne!(s.pending_unique_id, [0u8; UNIQUE_ID_LEN]);

        assert!(matches!(
            s.protect(&header),
            Err(NtsSessionError::CookiesExhausted)
        ));
    }

    #[test]
    fn each_request_uses_a_distinct_unique_id() {
        let mut s = NtsSession {
            keys: NtsKeys {
                c2s: [1; 32],
                s2c: [2; 32],
            },
            cookies: vec![vec![0xAB; 32], vec![0xCD; 32]].into(),
            pending_unique_id: [0; UNIQUE_ID_LEN],
            ntp_server: "example".into(),
            ntp_port: 123,
            ke_host: "example".into(),
        };
        let mut header = [0u8; 48];
        header[0] = (4 << 3) | 3;
        let _ = s.protect(&header).expect("first");
        let first = s.pending_unique_id;
        let _ = s.protect(&header).expect("second");
        assert_ne!(first, s.pending_unique_id, "unique id must not repeat");
    }
}
