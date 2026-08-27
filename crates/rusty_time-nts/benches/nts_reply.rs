//! The NTS server reply, as a deterministic instruction-count workload.
//!
//! For a server answering NTS this is the hot path, and it was never
//! instrumented: per request it mints fresh cookies, writes them into a
//! plaintext buffer, seals that under AES-SIV, and appends an authenticator.
//!
//! Deterministic by construction: fixed master key, fixed session keys, fixed
//! nonces. No `getrandom`, no clock — the entropy a real server draws is
//! replaced by a counter here so two runs produce the same instruction count
//! to the instruction, and so the reply bytes can serve as the gate.
//!
//! The checksum over every reply byte is that gate. This is an exact path:
//! any optimisation must leave it unchanged.
//!
//! Run:
//!   cargo build --release --bench nts_reply
//!   valgrind --tool=callgrind ./nts_reply

use rusty_time_nts::aead::{NtsKeys, seal};
use rusty_time_nts::cookie::{COOKIE_NONCE_LEN, KeyRing, MasterKey, mint_fields_into};
use rusty_time_nts::ef::{self, field_type};

/// Replies to build.
const REQUESTS: usize = 20_000;
/// Cookies per reply. RFC 8915 servers replace the spent cookie and honour
/// placeholders; 8 is the cap the daemon applies.
const COOKIES: usize = 8;
/// A plain NTP header's length — the reply starts as one.
const HEADER_LEN: usize = 48;
const NONCE_LEN: usize = 16;
const UNIQUE_ID_LEN: usize = 32;

fn main() {
    let mut ring = KeyRing::new(3);
    ring.rotate_in(MasterKey {
        id: 0x5a5a_5a5a,
        key: [0x11; 32],
    });
    let keys = NtsKeys {
        c2s: [0x22; 32],
        s2c: [0x33; 32],
    };

    let mut checksum: u64 = 0;
    let mut bytes_out: u64 = 0;

    for i in 0..REQUESTS {
        // Stand-ins for the per-request random draws, so the workload is
        // reproducible. A real server takes these from the OS.
        let seed = i as u8;
        let cookie_nonce = [seed ^ 0x5a; COOKIE_NONCE_LEN];
        let aead_nonce = [seed ^ 0xa5; NONCE_LEN];
        let unique_id = [seed ^ 0x3c; UNIQUE_ID_LEN];
        let header = [seed; HEADER_LEN];

        // --- exactly what the daemon builds per NTS request ---
        // Sized up front. A cookie field is ~104 bytes, so eight of them grow
        // an empty Vec through several reallocate-and-copy rounds.
        let mut plaintext = Vec::with_capacity(COOKIES * 112);
        let mut nonces = [[0u8; COOKIE_NONCE_LEN]; COOKIES];
        for (c, n) in nonces.iter_mut().enumerate() {
            *n = cookie_nonce;
            n[0] = c as u8;
        }
        mint_fields_into(&ring, &keys, &nonces, &mut plaintext).expect("mint");

        let mut reply = Vec::with_capacity(HEADER_LEN + 64 + COOKIES * 112 + 64);
        reply.extend_from_slice(&header);
        ef::write_field(&mut reply, field_type::UNIQUE_IDENTIFIER, &unique_id);
        let ciphertext = seal(&keys.s2c, &[&reply, &aead_nonce], &plaintext).expect("seal");
        ef::write_authenticator(&mut reply, &aead_nonce, &ciphertext);

        bytes_out += reply.len() as u64;
        let (words, _) = reply.as_chunks::<8>();
        for chunk in words {
            let word = u64::from_le_bytes(*chunk);
            checksum = checksum.wrapping_mul(0x0100_0000_01b3).wrapping_add(word);
        }
    }

    println!("requests   {REQUESTS}");
    println!("cookies    {COOKIES}");
    println!("bytes_out  {bytes_out}");
    println!("CHECKSUM   {checksum:#018x}");
}
