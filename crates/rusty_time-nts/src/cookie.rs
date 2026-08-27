//! Server-side NTS cookies (RFC 8915 §6).
//!
//! A cookie is the server's own encrypted note-to-self holding the session's
//! C2S/S2C keys, so the server keeps **no per-client state** — that is what lets
//! one NTS server answer millions of clients, and what keeps a client
//! unlinkable across queries (each response carries fresh cookies).
//!
//! Format (ours; the RFC leaves it implementation-defined):
//!
//! ```text
//! key_id (4) || nonce (16) || AES-SIV(master_key, aad=key_id||nonce, c2s||s2c)
//! ```
//!
//! `key_id` selects the master key so rotation can retire a key without
//! invalidating cookies minted under its predecessor.

use crate::aead::{self, NtsKeys};
use core::fmt;

/// Nonce length inside a cookie.
pub const COOKIE_NONCE_LEN: usize = 16;
const KEY_ID_LEN: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CookieError {
    /// Too short to be one of ours.
    Malformed,
    /// No master key with that id — rotated out, or not ours at all.
    UnknownKeyId(u32),
    /// Authentication failed: forged or corrupted.
    Authentication,
    /// The decrypted payload was not a key pair.
    BadPayload,
}

impl fmt::Display for CookieError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CookieError::Malformed => write!(f, "cookie is malformed"),
            CookieError::UnknownKeyId(id) => write!(f, "cookie names unknown master key {id}"),
            CookieError::Authentication => write!(f, "cookie failed authentication"),
            CookieError::BadPayload => write!(f, "cookie payload is not a key pair"),
        }
    }
}

impl std::error::Error for CookieError {}

/// One server master key.
#[derive(Clone)]
pub struct MasterKey {
    pub id: u32,
    pub key: [u8; 32],
}

/// The server's key ring: one current key for minting, plus recent keys kept
/// only for validation so a rotation does not strand clients holding cookies.
#[derive(Clone, Default)]
pub struct KeyRing {
    keys: Vec<MasterKey>,
    max_retained: usize,
}

impl KeyRing {
    /// `max_retained` includes the current key; chrony-equivalent default is 3
    /// (roughly a day of validity at 8-hour rotation).
    pub fn new(max_retained: usize) -> Self {
        KeyRing {
            keys: Vec::new(),
            max_retained: max_retained.max(1),
        }
    }

    /// Install a new current key, retiring the oldest beyond the retention.
    pub fn rotate_in(&mut self, key: MasterKey) {
        self.keys.push(key);
        while self.keys.len() > self.max_retained {
            self.keys.remove(0);
        }
    }

    pub fn current(&self) -> Option<&MasterKey> {
        self.keys.last()
    }

    pub fn by_id(&self, id: u32) -> Option<&MasterKey> {
        self.keys.iter().find(|k| k.id == id)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Ids currently held, oldest first — for the `status.ntsdata` op.
    pub fn key_ids(&self) -> Vec<u32> {
        self.keys.iter().map(|k| k.id).collect()
    }
}

/// Mint a cookie carrying `keys`, sealed under the ring's current master key.
pub fn mint(
    ring: &KeyRing,
    keys: &NtsKeys,
    nonce: &[u8; COOKIE_NONCE_LEN],
) -> Result<Vec<u8>, CookieError> {
    let master = ring.current().ok_or(CookieError::Malformed)?;
    let mut payload = Vec::with_capacity(64);
    payload.extend_from_slice(&keys.c2s);
    payload.extend_from_slice(&keys.s2c);

    let mut header = Vec::with_capacity(KEY_ID_LEN + COOKIE_NONCE_LEN);
    header.extend_from_slice(&master.id.to_be_bytes());
    header.extend_from_slice(nonce);

    let sealed =
        aead::seal(&master.key, &[&header], &payload).map_err(|_| CookieError::Authentication)?;

    let mut cookie = header;
    cookie.extend_from_slice(&sealed);
    Ok(cookie)
}

/// Mint several cookies at once, writing each straight into `out` as an
/// `NTS_COOKIE` extension field.
///
/// Byte-for-byte the same fields as calling [`mint`] per nonce and handing
/// each result to `ef::write_field`, with the per-cookie overhead removed:
///
/// * **One key schedule instead of one per cookie.** All the cookies in a
///   reply are sealed under the same master key, so the cipher is built once.
/// * **No per-cookie allocations.** The payload and header are fixed-size and
///   live on the stack; the cookie is written into the caller's buffer rather
///   than assembled in a `Vec` and copied in.
///
/// A server answering one NTS request mints up to eight of these, so the old
/// shape cost eight key expansions and about thirty allocations per reply.
pub fn mint_fields_into(
    ring: &KeyRing,
    keys: &NtsKeys,
    nonces: &[[u8; COOKIE_NONCE_LEN]],
    out: &mut Vec<u8>,
) -> Result<(), CookieError> {
    let master = ring.current().ok_or(CookieError::Malformed)?;
    let mut sealer = aead::Sealer::new(&master.key);

    let mut payload = [0u8; 64];
    payload[..32].copy_from_slice(&keys.c2s);
    payload[32..].copy_from_slice(&keys.s2c);

    let mut header = [0u8; KEY_ID_LEN + COOKIE_NONCE_LEN];
    header[..KEY_ID_LEN].copy_from_slice(&master.id.to_be_bytes());

    // One scratch buffer for every cookie, rather than a fresh allocation per
    // cookie that is copied out and dropped immediately.
    let mut sealed = Vec::with_capacity(payload.len() + 16);
    for nonce in nonces {
        header[KEY_ID_LEN..].copy_from_slice(nonce);
        sealed.clear();
        sealed.extend_from_slice(&payload);
        sealer
            .seal_in_place(&[&header], &mut sealed)
            .map_err(|_| CookieError::Authentication)?;
        // The cookie is `header || sealed`; written as two parts so it never
        // has to exist as one buffer.
        crate::ef::write_field_parts(out, crate::ef::field_type::NTS_COOKIE, &[&header, &sealed]);
    }
    Ok(())
}

/// Recover the session keys from a cookie, or reject it.
pub fn redeem(ring: &KeyRing, cookie: &[u8]) -> Result<NtsKeys, CookieError> {
    if cookie.len() < KEY_ID_LEN + COOKIE_NONCE_LEN + 16 {
        return Err(CookieError::Malformed);
    }
    let key_id = u32::from_be_bytes([cookie[0], cookie[1], cookie[2], cookie[3]]);
    let master = ring
        .by_id(key_id)
        .ok_or(CookieError::UnknownKeyId(key_id))?;

    let header = &cookie[..KEY_ID_LEN + COOKIE_NONCE_LEN];
    let sealed = &cookie[KEY_ID_LEN + COOKIE_NONCE_LEN..];
    let payload =
        aead::open(&master.key, &[header], sealed).map_err(|_| CookieError::Authentication)?;

    if payload.len() != 64 {
        return Err(CookieError::BadPayload);
    }
    let mut keys = NtsKeys {
        c2s: [0u8; 32],
        s2c: [0u8; 32],
    };
    keys.c2s.copy_from_slice(&payload[..32]);
    keys.s2c.copy_from_slice(&payload[32..]);
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_with(ids: &[u32]) -> KeyRing {
        let mut r = KeyRing::new(3);
        for (i, id) in ids.iter().enumerate() {
            r.rotate_in(MasterKey {
                id: *id,
                key: [(i as u8) + 1; 32],
            });
        }
        r
    }

    fn keys() -> NtsKeys {
        NtsKeys {
            c2s: [0xAA; 32],
            s2c: [0xBB; 32],
        }
    }

    #[test]
    fn mint_then_redeem_recovers_the_keys() {
        let ring = ring_with(&[42]);
        let cookie = mint(&ring, &keys(), &[7; COOKIE_NONCE_LEN]).expect("mint");
        let back = redeem(&ring, &cookie).expect("redeem");
        assert_eq!(back.c2s, keys().c2s);
        assert_eq!(back.s2c, keys().s2c);
    }

    #[test]
    fn cookie_is_opaque_and_carries_no_plaintext_key() {
        let ring = ring_with(&[42]);
        let cookie = mint(&ring, &keys(), &[7; COOKIE_NONCE_LEN]).expect("mint");
        // The session keys must not appear in the clear anywhere in the cookie.
        assert!(
            !cookie.windows(32).any(|w| w == keys().c2s),
            "C2S key leaked in cookie plaintext"
        );
        assert!(
            !cookie.windows(32).any(|w| w == keys().s2c),
            "S2C key leaked in cookie plaintext"
        );
    }

    #[test]
    fn tampering_is_rejected_at_every_offset() {
        let ring = ring_with(&[42]);
        let cookie = mint(&ring, &keys(), &[7; COOKIE_NONCE_LEN]).expect("mint");
        for i in 0..cookie.len() {
            let mut bad = cookie.clone();
            bad[i] ^= 0x01;
            assert!(
                redeem(&ring, &bad).is_err(),
                "flipping byte {i} produced an accepted cookie"
            );
        }
    }

    #[test]
    fn rotation_keeps_recent_cookies_valid_and_retires_old_ones() {
        let mut ring = ring_with(&[1]);
        let old = mint(&ring, &keys(), &[7; COOKIE_NONCE_LEN]).expect("mint");

        // Two rotations: the original key is still retained (max 3).
        ring.rotate_in(MasterKey {
            id: 2,
            key: [9; 32],
        });
        ring.rotate_in(MasterKey {
            id: 3,
            key: [10; 32],
        });
        assert!(redeem(&ring, &old).is_ok(), "cookie stranded too early");

        // A third pushes key 1 out; the cookie must now be refused, not
        // silently mis-decrypted.
        ring.rotate_in(MasterKey {
            id: 4,
            key: [11; 32],
        });
        assert!(matches!(
            redeem(&ring, &old),
            Err(CookieError::UnknownKeyId(1))
        ));
        assert_eq!(ring.key_ids(), vec![2, 3, 4]);
    }

    #[test]
    fn foreign_cookie_is_rejected() {
        let ours = ring_with(&[42]);
        let mut theirs = KeyRing::new(3);
        theirs.rotate_in(MasterKey {
            id: 42, // same id, different key
            key: [0xFF; 32],
        });
        let cookie = mint(&theirs, &keys(), &[7; COOKIE_NONCE_LEN]).expect("mint");
        assert!(matches!(
            redeem(&ours, &cookie),
            Err(CookieError::Authentication)
        ));
    }

    #[test]
    fn garbage_never_panics() {
        let ring = ring_with(&[42]);
        let mut rng = 0xDEAD_BEEF_1234_5678u64;
        for len in 0..80usize {
            let mut c = vec![0u8; len];
            for b in c.iter_mut() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                *b = rng as u8;
            }
            let _ = redeem(&ring, &c);
        }
    }
}
