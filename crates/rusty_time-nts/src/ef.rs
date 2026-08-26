//! NTS extension fields for NTP packets (RFC 8915 §5.x) — building a protected
//! client request and verifying a protected server response.
//!
//! Wire layout of every extension field is RFC 7822: 2-byte type, 2-byte total
//! length (header included, multiple of 4, at least 16), then the value.
//!
//! No TLS here and no I/O: this module is wasm-clean, so the browser client can
//! speak authenticated NTS over whatever transport it has.

use crate::aead::{self, AeadError};
use core::fmt;

/// Extension-field types NTS defines (RFC 8915 §5.1–5.6).
pub mod field_type {
    pub const UNIQUE_IDENTIFIER: u16 = 0x0104;
    pub const NTS_COOKIE: u16 = 0x0204;
    pub const NTS_COOKIE_PLACEHOLDER: u16 = 0x0304;
    pub const NTS_AUTHENTICATOR: u16 = 0x0404;
}

/// Fixed NTPv4 header length; extension fields begin here.
const HEADER_LEN: usize = 48;
/// RFC 8915 §5.3: the unique identifier must be at least 32 octets.
pub const UNIQUE_ID_LEN: usize = 32;
/// RFC 8915 §5.6 recommends a 16-octet nonce for AES-SIV-CMAC-256.
pub const NONCE_LEN: usize = 16;
/// RFC 7822: no extension field is shorter than this.
const MIN_EF_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NtsError {
    /// The response carried no authenticator extension field.
    MissingAuthenticator,
    /// AEAD verification failed: forged, corrupted, or wrong key.
    Authentication,
    /// The response's unique identifier did not match the request's.
    UniqueIdMismatch,
    /// An extension field was malformed.
    MalformedField,
    /// The server answered with an NTS NAK (cookie rejected; re-run NTS-KE).
    Nak,
    /// The packet was shorter than an NTP header.
    TooShort,
}

impl fmt::Display for NtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            NtsError::MissingAuthenticator => "response has no NTS authenticator field",
            NtsError::Authentication => "NTS authentication failed",
            NtsError::UniqueIdMismatch => "NTS unique identifier does not match the request",
            NtsError::MalformedField => "malformed NTS extension field",
            NtsError::Nak => "server sent an NTS NAK: the cookie was rejected",
            NtsError::TooShort => "packet is shorter than an NTP header",
        };
        f.write_str(s)
    }
}

impl std::error::Error for NtsError {}

impl From<AeadError> for NtsError {
    fn from(_: AeadError) -> Self {
        NtsError::Authentication
    }
}

/// Append one extension field, applying RFC 7822 padding: the total length is
/// rounded up to a multiple of 4 and raised to the 16-octet minimum.
pub fn write_field(out: &mut Vec<u8>, field_type: u16, body: &[u8]) {
    let total = (4 + body.len()).next_multiple_of(4).max(MIN_EF_LEN);
    out.extend_from_slice(&field_type.to_be_bytes());
    out.extend_from_slice(&(total as u16).to_be_bytes());
    out.extend_from_slice(body);
    out.resize(out.len() + (total - 4 - body.len()), 0);
}

/// One parsed extension field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field<'a> {
    pub field_type: u16,
    pub body: &'a [u8],
    /// Offset of this field's first byte within the packet.
    pub offset: usize,
}

/// Iterate the extension fields after the NTP header. Stops at the first
/// malformed field rather than guessing (a legacy MAC tail is not an extension
/// field and must not be parsed as one).
pub fn fields(packet: &[u8]) -> impl Iterator<Item = Field<'_>> {
    let mut at = HEADER_LEN.min(packet.len());
    core::iter::from_fn(move || {
        let rest = packet.get(at..)?;
        if rest.len() < 4 {
            return None;
        }
        let field_type = u16::from_be_bytes([rest[0], rest[1]]);
        let total = u16::from_be_bytes([rest[2], rest[3]]) as usize;
        if total < MIN_EF_LEN || !total.is_multiple_of(4) || total > rest.len() {
            return None;
        }
        let field = Field {
            field_type,
            body: &rest[4..total],
            offset: at,
        };
        at += total;
        Some(field)
    })
}

/// Build an NTS-protected client request.
///
/// `header` is the 48-byte NTPv4 client header (its transmit timestamp should
/// already be the unpredictable nonce the plain client uses). `cookie` is one
/// unused cookie from NTS-KE; `extra_cookies` placeholders ask the server to
/// replenish that many more, keeping the client's cookie store from draining.
pub fn protect_request(
    header: &[u8; HEADER_LEN],
    unique_id: &[u8; UNIQUE_ID_LEN],
    cookie: &[u8],
    extra_cookies: usize,
    c2s_key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
) -> Result<Vec<u8>, NtsError> {
    let mut packet = Vec::with_capacity(HEADER_LEN + 64 + cookie.len() * (1 + extra_cookies));
    packet.extend_from_slice(header);
    write_field(&mut packet, field_type::UNIQUE_IDENTIFIER, unique_id);
    write_field(&mut packet, field_type::NTS_COOKIE, cookie);
    // Placeholders must be the same size as a real cookie: that is how the
    // server learns how much room the reply has (RFC 8915 §5.5).
    let placeholder = vec![0u8; cookie.len()];
    for _ in 0..extra_cookies {
        write_field(
            &mut packet,
            field_type::NTS_COOKIE_PLACEHOLDER,
            &placeholder,
        );
    }

    // The authenticator covers everything written so far. A client request
    // carries no encrypted extension fields, so the plaintext is empty.
    let ciphertext = aead::seal(c2s_key, &[&packet, nonce], &[])?;
    packet.extend_from_slice(&authenticator_field(nonce, &ciphertext));
    Ok(packet)
}

/// Assemble the NTS Authenticator extension field, its 4-byte header included.
/// Servers use this to seal their reply (with the cookies as plaintext); the
/// client's own request path uses it with an empty plaintext.
pub fn authenticator_field(nonce: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + nonce.len() + ciphertext.len() + 8);
    body.extend_from_slice(&(nonce.len() as u16).to_be_bytes());
    body.extend_from_slice(&(ciphertext.len() as u16).to_be_bytes());
    body.extend_from_slice(nonce);
    body.resize(
        body.len() + nonce.len().next_multiple_of(4) - nonce.len(),
        0,
    );
    body.extend_from_slice(ciphertext);
    body.resize(
        body.len() + ciphertext.len().next_multiple_of(4) - ciphertext.len(),
        0,
    );
    let mut field = Vec::with_capacity(4 + body.len());
    write_field(&mut field, field_type::NTS_AUTHENTICATOR, &body);
    field
}

/// What a verified server response yielded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerifiedResponse {
    /// Fresh cookies the server sent inside the encrypted field. Each replaces
    /// the one just spent; a cookie is never reused (RFC 8915 §5.7 privacy).
    pub new_cookies: Vec<Vec<u8>>,
}

/// Verify an NTS-protected server response and recover its new cookies.
///
/// Checks, in this order: the unique identifier echoes our request (binds the
/// reply to it), an authenticator field exists, and the AEAD tag verifies over
/// exactly the bytes preceding that field.
pub fn verify_response(
    packet: &[u8],
    s2c_key: &[u8; 32],
    expect_unique_id: &[u8; UNIQUE_ID_LEN],
) -> Result<VerifiedResponse, NtsError> {
    if packet.len() < HEADER_LEN {
        return Err(NtsError::TooShort);
    }

    let mut unique_ok = false;
    let mut authenticator: Option<(usize, &[u8])> = None;
    let mut saw_any = false;
    for field in fields(packet) {
        saw_any = true;
        match field.field_type {
            field_type::UNIQUE_IDENTIFIER => {
                // Constant-time-ish compare is unnecessary here (the value is
                // public once sent), but the match must be exact.
                unique_ok = field.body == expect_unique_id.as_slice();
            }
            field_type::NTS_AUTHENTICATOR => {
                authenticator = Some((field.offset, field.body));
                break; // nothing after the authenticator is authenticated
            }
            _ => {}
        }
    }

    if !unique_ok {
        return Err(NtsError::UniqueIdMismatch);
    }
    let Some((offset, body)) = authenticator else {
        // A response that echoes our unique id but carries no authenticator is
        // the NTS NAK shape: the server rejected the cookie.
        return Err(if saw_any {
            NtsError::Nak
        } else {
            NtsError::MissingAuthenticator
        });
    };

    if body.len() < 4 {
        return Err(NtsError::MalformedField);
    }
    let nonce_len = u16::from_be_bytes([body[0], body[1]]) as usize;
    let ct_len = u16::from_be_bytes([body[2], body[3]]) as usize;
    let nonce_end = 4 + nonce_len;
    let ct_start = 4 + nonce_len.next_multiple_of(4);
    let ct_end = ct_start + ct_len;
    if nonce_len == 0 || ct_end > body.len() {
        return Err(NtsError::MalformedField);
    }
    let nonce = &body[4..nonce_end];
    let ciphertext = &body[ct_start..ct_end];

    // Associated data is exactly the packet up to the authenticator field.
    let aad = &packet[..offset];
    let plaintext = aead::open(s2c_key, &[aad, nonce], ciphertext)?;

    // The decrypted payload is itself a sequence of extension fields; the
    // cookies live there so an observer cannot link a client across queries.
    let mut new_cookies = Vec::new();
    let mut at = 0usize;
    while at + 4 <= plaintext.len() {
        let ft = u16::from_be_bytes([plaintext[at], plaintext[at + 1]]);
        let total = u16::from_be_bytes([plaintext[at + 2], plaintext[at + 3]]) as usize;
        if total < MIN_EF_LEN || !total.is_multiple_of(4) || at + total > plaintext.len() {
            break;
        }
        if ft == field_type::NTS_COOKIE {
            new_cookies.push(plaintext[at + 4..at + total].to_vec());
        }
        at += total;
    }

    Ok(VerifiedResponse { new_cookies })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0] = (4 << 3) | 3; // v4, client
        h[40..48].copy_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
        h
    }

    /// Stand in for a server: verify the client's request, then build the
    /// authenticated reply the client must accept.
    fn server_reply(request: &[u8], c2s: &[u8; 32], s2c: &[u8; 32], cookies: &[&[u8]]) -> Vec<u8> {
        // Authenticate the request the way a server would.
        let auth = fields(request)
            .find(|f| f.field_type == field_type::NTS_AUTHENTICATOR)
            .expect("request authenticator");
        let nonce_len = u16::from_be_bytes([auth.body[0], auth.body[1]]) as usize;
        let ct_len = u16::from_be_bytes([auth.body[2], auth.body[3]]) as usize;
        let ct_start = 4 + nonce_len.next_multiple_of(4);
        aead::open(
            c2s,
            &[&request[..auth.offset], &auth.body[4..4 + nonce_len]],
            &auth.body[ct_start..ct_start + ct_len],
        )
        .expect("server verifies client request");

        let unique = fields(request)
            .find(|f| f.field_type == field_type::UNIQUE_IDENTIFIER)
            .expect("unique id");

        let mut reply = Vec::new();
        let mut h = header();
        h[0] = (4 << 3) | 4; // server mode
        reply.extend_from_slice(&h);
        write_field(&mut reply, field_type::UNIQUE_IDENTIFIER, unique.body);

        let mut plaintext = Vec::new();
        for c in cookies {
            write_field(&mut plaintext, field_type::NTS_COOKIE, c);
        }
        let nonce = [9u8; NONCE_LEN];
        let ct = aead::seal(s2c, &[&reply, &nonce], &plaintext).expect("seal reply");
        reply.extend_from_slice(&authenticator_field(&nonce, &ct));
        reply
    }

    #[test]
    fn request_response_round_trip() {
        let c2s = [1u8; 32];
        let s2c = [2u8; 32];
        let uid = [7u8; UNIQUE_ID_LEN];
        let nonce = [3u8; NONCE_LEN];
        let cookie = b"cookie-from-nts-ke--------------".to_vec();

        let req = protect_request(&header(), &uid, &cookie, 2, &c2s, &nonce).expect("protect");

        // Structure: unique id, cookie, 2 placeholders, authenticator.
        let types: Vec<u16> = fields(&req).map(|f| f.field_type).collect();
        assert_eq!(
            types,
            vec![
                field_type::UNIQUE_IDENTIFIER,
                field_type::NTS_COOKIE,
                field_type::NTS_COOKIE_PLACEHOLDER,
                field_type::NTS_COOKIE_PLACEHOLDER,
                field_type::NTS_AUTHENTICATOR,
            ]
        );
        // Every field is 4-aligned and at least 16 bytes.
        for f in fields(&req) {
            assert!(f.body.len() + 4 >= MIN_EF_LEN);
            assert!((f.body.len() + 4).is_multiple_of(4));
        }

        let reply = server_reply(&req, &c2s, &s2c, &[b"new-cookie-one------------------"]);
        let verified = verify_response(&reply, &s2c, &uid).expect("verify");
        assert_eq!(verified.new_cookies.len(), 1);
        assert_eq!(
            verified.new_cookies[0],
            b"new-cookie-one------------------".to_vec()
        );
    }

    #[test]
    fn tampered_response_is_rejected() {
        let (c2s, s2c, uid, nonce) = ([1u8; 32], [2u8; 32], [7u8; UNIQUE_ID_LEN], [3u8; NONCE_LEN]);
        let cookie = vec![0xAB; 32];
        let req = protect_request(&header(), &uid, &cookie, 1, &c2s, &nonce).expect("protect");
        let mut reply = server_reply(&req, &c2s, &s2c, &[&[0xCD; 32]]);
        // Flip a bit in the NTP header — inside the authenticated AD.
        reply[8] ^= 0x01;
        assert_eq!(
            verify_response(&reply, &s2c, &uid),
            Err(NtsError::Authentication)
        );
    }

    #[test]
    fn wrong_unique_id_is_rejected() {
        let (c2s, s2c, uid, nonce) = ([1u8; 32], [2u8; 32], [7u8; UNIQUE_ID_LEN], [3u8; NONCE_LEN]);
        let cookie = vec![0xAB; 32];
        let req = protect_request(&header(), &uid, &cookie, 1, &c2s, &nonce).expect("protect");
        let reply = server_reply(&req, &c2s, &s2c, &[&[0xCD; 32]]);
        // A reply to somebody else's query must not be accepted as ours.
        assert_eq!(
            verify_response(&reply, &s2c, &[8u8; UNIQUE_ID_LEN]),
            Err(NtsError::UniqueIdMismatch)
        );
    }

    #[test]
    fn wrong_key_is_rejected() {
        let (c2s, s2c, uid, nonce) = ([1u8; 32], [2u8; 32], [7u8; UNIQUE_ID_LEN], [3u8; NONCE_LEN]);
        let cookie = vec![0xAB; 32];
        let req = protect_request(&header(), &uid, &cookie, 1, &c2s, &nonce).expect("protect");
        let reply = server_reply(&req, &c2s, &s2c, &[&[0xCD; 32]]);
        assert_eq!(
            verify_response(&reply, &[0xFFu8; 32], &uid),
            Err(NtsError::Authentication)
        );
    }

    #[test]
    fn malformed_fields_never_panic() {
        // Structured garbage after a valid header must be handled, not trusted.
        let mut rng = 0x1234_5678_9ABC_DEF0u64;
        for _ in 0..20_000 {
            let mut p = vec![0u8; HEADER_LEN];
            p[0] = (4 << 3) | 4;
            let n = (rng % 96) as usize;
            for _ in 0..n {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                p.push(rng as u8);
            }
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let _ = verify_response(&p, &[1u8; 32], &[7u8; UNIQUE_ID_LEN]);
            for _ in fields(&p) {}
        }
    }
}
