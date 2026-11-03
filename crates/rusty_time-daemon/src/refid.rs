//! The reference id a server reports for the source it is synchronised to
//! (RFC 5905 §7.3): the IPv4 address itself, or for IPv6 the first four octets
//! of the MD5 hash of the address.
//!
//! Peers compare reference ids to detect timing loops, so the IPv6 form has to
//! be the MD5 one every other implementation computes. MD5 over one 16-byte
//! message is all that is needed, so it is written out here rather than pulled
//! in as a dependency; it is used for identification only, never for security.

use std::net::IpAddr;

/// The reference id for a source at `addr`.
pub fn reference_id(addr: IpAddr) -> [u8; 4] {
    match addr {
        IpAddr::V4(v4) => v4.octets(),
        IpAddr::V6(v6) => {
            let d = md5_16(&v6.octets());
            [d[0], d[1], d[2], d[3]]
        }
    }
}

/// MD5 (RFC 1321) of exactly 16 bytes: a single padded block.
fn md5_16(message: &[u8; 16]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    // The one block: the message, the 0x80 pad byte, zeros, and the bit
    // length (128) as a little-endian u64 in the last eight bytes.
    let mut block = [0u8; 64];
    block[..16].copy_from_slice(message);
    block[16] = 0x80;
    block[56] = 128;
    let mut m = [0u32; 16];
    for (i, word) in m.iter_mut().enumerate() {
        *word = u32::from_le_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }

    let (a0, b0, c0, d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            2 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let rotated = a
            .wrapping_add(f)
            .wrapping_add(K[i])
            .wrapping_add(m[g])
            .rotate_left(S[i]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(rotated);
    }

    let mut out = [0u8; 16];
    for (i, word) in [
        a0.wrapping_add(a),
        b0.wrapping_add(b),
        c0.wrapping_add(c),
        d0.wrapping_add(d),
    ]
    .iter()
    .enumerate()
    {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_is_the_address_itself() {
        assert_eq!(
            reference_id("203.0.113.2".parse().unwrap()),
            [203, 0, 113, 2]
        );
    }

    #[test]
    fn ipv6_is_the_first_four_octets_of_its_md5() {
        // Expected values from Python's hashlib over the packed address.
        assert_eq!(
            reference_id("2001:db8::1".parse().unwrap()),
            [57, 171, 155, 55]
        );
        assert_eq!(
            reference_id("2001:db8:1::2".parse().unwrap()),
            [182, 134, 154, 143]
        );
    }
}
