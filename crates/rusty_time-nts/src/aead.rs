//! AES-SIV-CMAC-256 seal/open for NTS extension fields (RFC 8915 §5.6, RFC 5297).
//!
//! Thin, typed wrappers over the RustCrypto `aes-siv` implementation. Key
//! separation (C2S vs S2C) is the caller's job; this module refuses nothing but
//! bad ciphertexts.

use aes_siv::KeyInit;
use aes_siv::siv::Aes128Siv;
use core::fmt;

/// The pair of keys NTS-KE exports (RFC 8915 §4.3, §5.1).
#[derive(Clone)]
pub struct NtsKeys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
}

/// Redacting, deliberately: a derived `Debug` would spill session keys into
/// every log line, panic message and error report that formats a struct
/// holding one. If you need the bytes, name the field.
impl fmt::Debug for NtsKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NtsKeys { c2s: <redacted>, s2c: <redacted> }")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AeadError {
    /// Authentication failed or ciphertext malformed.
    Open,
    /// The cipher rejected the inputs (wrong sizes).
    Seal,
}

impl fmt::Display for AeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AeadError::Open => write!(f, "NTS AEAD authentication failed"),
            AeadError::Seal => write!(f, "NTS AEAD seal rejected inputs"),
        }
    }
}

impl std::error::Error for AeadError {}

/// Encrypt-and-authenticate. Associated data slices are authenticated in order;
/// the SIV tag is prepended to the returned ciphertext (RFC 5297 layout).
pub fn seal(key: &[u8; 32], aad: &[&[u8]], plaintext: &[u8]) -> Result<Vec<u8>, AeadError> {
    let mut cipher = Aes128Siv::new(key.into());
    cipher.encrypt(aad, plaintext).map_err(|_| AeadError::Seal)
}

/// Verify-and-decrypt the counterpart of [`seal`].
pub fn open(key: &[u8; 32], aad: &[&[u8]], ciphertext: &[u8]) -> Result<Vec<u8>, AeadError> {
    let mut cipher = Aes128Siv::new(key.into());
    cipher.decrypt(aad, ciphertext).map_err(|_| AeadError::Open)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = [7u8; 32];
        let aad: &[&[u8]] = &[b"ntp header bytes"];
        let ct = seal(&key, aad, b"cookie plaintext").expect("seal");
        assert_ne!(&ct[..], b"cookie plaintext");
        let pt = open(&key, aad, &ct).expect("open");
        assert_eq!(pt, b"cookie plaintext");
    }

    #[test]
    fn tamper_is_rejected() {
        let key = [7u8; 32];
        let aad: &[&[u8]] = &[b"header"];
        let mut ct = seal(&key, aad, b"payload").expect("seal");
        ct[0] ^= 1;
        assert_eq!(open(&key, aad, &ct), Err(AeadError::Open));
    }

    #[test]
    fn wrong_aad_is_rejected() {
        let key = [7u8; 32];
        let ct = seal(&key, &[b"header A"], b"payload").expect("seal");
        assert_eq!(open(&key, &[b"header B"], &ct), Err(AeadError::Open));
    }
}
