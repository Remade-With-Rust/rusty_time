//! NTPv4 packet codec (RFC 5905) with extension-field iteration (RFC 7822).
//!
//! Every entry point is a parse-constructor returning `Result`; nothing here can
//! panic on untrusted bytes (the fuzz targets in `fuzz/` hold this line).

use core::fmt;

/// Length of the fixed NTPv4 header.
pub const HEADER_LEN: usize = 48;

/// Seconds between the NTP era-0 epoch (1900-01-01) and the Unix epoch (1970-01-01).
pub const UNIX_EPOCH_OFFSET: u64 = 2_208_988_800;

const FRAC: f64 = 4_294_967_296.0; // 2^32

/// 64-bit NTP timestamp: 32.32 fixed-point seconds since the era epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NtpTimestamp(pub u64);

impl NtpTimestamp {
    pub const ZERO: NtpTimestamp = NtpTimestamp(0);

    pub fn from_parts(seconds: u32, fraction: u32) -> Self {
        NtpTimestamp(((seconds as u64) << 32) | fraction as u64)
    }

    pub fn seconds(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub fn fraction(self) -> u32 {
        self.0 as u32
    }

    /// Build from Unix wall time. Era wrap (2036) is handled by truncation to the
    /// low 32 bits of the NTP second count, per RFC 5905 era arithmetic.
    pub fn from_unix(secs: i64, nanos: u32) -> Self {
        let ntp_secs = (secs.wrapping_add(UNIX_EPOCH_OFFSET as i64)) as u64;
        let frac = ((nanos as u64) << 32) / 1_000_000_000;
        NtpTimestamp(((ntp_secs & 0xFFFF_FFFF) << 32) | (frac & 0xFFFF_FFFF))
    }

    /// Signed seconds from `earlier` to `self`, correct across era wrap for spans
    /// under ±68 years (the same guarantee RFC 5905 gives).
    pub fn seconds_since(self, earlier: NtpTimestamp) -> f64 {
        let diff = self.0.wrapping_sub(earlier.0) as i64;
        diff as f64 / FRAC
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
}

/// 32-bit NTP short format: 16.16 fixed-point seconds (root delay / dispersion).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct NtpShort(pub u32);

impl NtpShort {
    pub fn to_seconds(self) -> f64 {
        self.0 as f64 / 65_536.0
    }

    pub fn from_seconds(s: f64) -> Self {
        let clamped = s.clamp(0.0, 65_535.999);
        NtpShort((clamped * 65_536.0) as u32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeapIndicator {
    NoWarning,
    LastMinute61,
    LastMinute59,
    Unsynchronized,
}

impl LeapIndicator {
    fn from_bits(b: u8) -> Self {
        match b & 0b11 {
            0 => LeapIndicator::NoWarning,
            1 => LeapIndicator::LastMinute61,
            2 => LeapIndicator::LastMinute59,
            _ => LeapIndicator::Unsynchronized,
        }
    }

    fn bits(self) -> u8 {
        match self {
            LeapIndicator::NoWarning => 0,
            LeapIndicator::LastMinute61 => 1,
            LeapIndicator::LastMinute59 => 2,
            LeapIndicator::Unsynchronized => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Reserved,
    SymmetricActive,
    SymmetricPassive,
    Client,
    Server,
    Broadcast,
    Control,
    Private,
}

impl Mode {
    fn from_bits(b: u8) -> Self {
        match b & 0b111 {
            1 => Mode::SymmetricActive,
            2 => Mode::SymmetricPassive,
            3 => Mode::Client,
            4 => Mode::Server,
            5 => Mode::Broadcast,
            6 => Mode::Control,
            7 => Mode::Private,
            _ => Mode::Reserved,
        }
    }

    fn bits(self) -> u8 {
        match self {
            Mode::Reserved => 0,
            Mode::SymmetricActive => 1,
            Mode::SymmetricPassive => 2,
            Mode::Client => 3,
            Mode::Server => 4,
            Mode::Broadcast => 5,
            Mode::Control => 6,
            Mode::Private => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Fewer than 48 bytes.
    TooShort { len: usize },
    /// Version outside 3..=4.
    BadVersion { version: u8 },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::TooShort { len } => {
                write!(f, "packet is {len} bytes; NTP header needs {HEADER_LEN}")
            }
            ParseError::BadVersion { version } => {
                write!(f, "unsupported NTP version {version} (expected 3 or 4)")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// The fixed NTPv4 header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NtpPacket {
    pub leap: LeapIndicator,
    pub version: u8,
    pub mode: Mode,
    pub stratum: u8,
    pub poll: i8,
    pub precision: i8,
    pub root_delay: NtpShort,
    pub root_dispersion: NtpShort,
    pub reference_id: [u8; 4],
    pub reference_ts: NtpTimestamp,
    pub origin_ts: NtpTimestamp,
    pub receive_ts: NtpTimestamp,
    pub transmit_ts: NtpTimestamp,
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    if let Some(s) = buf.get(at..at + 4) {
        b.copy_from_slice(s);
    }
    u32::from_be_bytes(b)
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    if let Some(s) = buf.get(at..at + 8) {
        b.copy_from_slice(s);
    }
    u64::from_be_bytes(b)
}

impl NtpPacket {
    /// A mode-3 client request. The caller supplies `transmit_ts`, which SHOULD be
    /// an unpredictable nonce rather than the real clock (BCP: it is echoed back as
    /// `origin_ts` and is the only spoofing defence an unauthenticated client has).
    pub fn client_request(version: u8, transmit_ts: NtpTimestamp) -> Self {
        NtpPacket {
            leap: LeapIndicator::NoWarning,
            version,
            mode: Mode::Client,
            stratum: 0,
            poll: 0,
            precision: 0x20u8 as i8,
            root_delay: NtpShort(0),
            root_dispersion: NtpShort(0),
            reference_id: [0; 4],
            reference_ts: NtpTimestamp::ZERO,
            origin_ts: NtpTimestamp::ZERO,
            receive_ts: NtpTimestamp::ZERO,
            transmit_ts,
        }
    }

    /// Parse the 48-byte header. Trailing bytes (extension fields / legacy MAC) are
    /// left to [`extension_fields`].
    pub fn parse(buf: &[u8]) -> Result<NtpPacket, ParseError> {
        if buf.len() < HEADER_LEN {
            return Err(ParseError::TooShort { len: buf.len() });
        }
        let b0 = buf[0];
        let version = (b0 >> 3) & 0b111;
        if !(3..=4).contains(&version) {
            return Err(ParseError::BadVersion { version });
        }
        let mut reference_id = [0u8; 4];
        reference_id.copy_from_slice(&buf[12..16]);
        Ok(NtpPacket {
            leap: LeapIndicator::from_bits(b0 >> 6),
            version,
            mode: Mode::from_bits(b0),
            stratum: buf[1],
            poll: buf[2] as i8,
            precision: buf[3] as i8,
            root_delay: NtpShort(read_u32(buf, 4)),
            root_dispersion: NtpShort(read_u32(buf, 8)),
            reference_id,
            reference_ts: NtpTimestamp(read_u64(buf, 16)),
            origin_ts: NtpTimestamp(read_u64(buf, 24)),
            receive_ts: NtpTimestamp(read_u64(buf, 32)),
            transmit_ts: NtpTimestamp(read_u64(buf, 40)),
        })
    }

    /// Serialize the header into a 48-byte buffer.
    pub fn write(&self, buf: &mut [u8; HEADER_LEN]) {
        buf[0] = (self.leap.bits() << 6) | ((self.version & 0b111) << 3) | self.mode.bits();
        buf[1] = self.stratum;
        buf[2] = self.poll as u8;
        buf[3] = self.precision as u8;
        buf[4..8].copy_from_slice(&self.root_delay.0.to_be_bytes());
        buf[8..12].copy_from_slice(&self.root_dispersion.0.to_be_bytes());
        buf[12..16].copy_from_slice(&self.reference_id);
        buf[16..24].copy_from_slice(&self.reference_ts.0.to_be_bytes());
        // RFC 5905 field order: reference, origin, receive, transmit.
        buf[24..32].copy_from_slice(&self.origin_ts.0.to_be_bytes());
        buf[32..40].copy_from_slice(&self.receive_ts.0.to_be_bytes());
        buf[40..48].copy_from_slice(&self.transmit_ts.0.to_be_bytes());
    }

    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        self.write(&mut buf);
        buf
    }
}

/// One RFC 7822 extension field: type, and its value bytes (header excluded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtensionField<'a> {
    pub field_type: u16,
    pub value: &'a [u8],
}

/// What follows the fixed header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trailer<'a> {
    /// A well-formed RFC 7822 extension field.
    Extension(ExtensionField<'a>),
    /// Bytes that cannot be a well-formed extension field (e.g. a legacy MAC).
    /// Always the final item when present.
    Opaque(&'a [u8]),
}

/// Iterate whatever follows the 48-byte header. Malformed input can never panic:
/// anything that does not parse as an extension field is yielded once as
/// [`Trailer::Opaque`] and iteration ends.
pub fn extension_fields(packet: &[u8]) -> ExtensionIter<'_> {
    let rest = packet.get(HEADER_LEN..).unwrap_or(&[]);
    ExtensionIter { rest }
}

pub struct ExtensionIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for ExtensionIter<'a> {
    type Item = Trailer<'a>;

    fn next(&mut self) -> Option<Trailer<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        if self.rest.len() >= 4 {
            let field_type = u16::from_be_bytes([self.rest[0], self.rest[1]]);
            let len = u16::from_be_bytes([self.rest[2], self.rest[3]]) as usize;
            // RFC 7822: total length includes the 4-byte header, is a multiple of
            // 4, and is at least 16.
            if len >= 16 && len.is_multiple_of(4) && len <= self.rest.len() {
                let value = &self.rest[4..len];
                self.rest = &self.rest[len..];
                return Some(Trailer::Extension(ExtensionField { field_type, value }));
            }
        }
        let opaque = self.rest;
        self.rest = &[];
        Some(Trailer::Opaque(opaque))
    }
}

/// Offset/delay from the four client-exchange timestamps (RFC 5905 §8), all in
/// seconds on any common timescale. Returns (offset, delay); offset is seconds to
/// ADD to the local clock.
pub fn offset_delay(t1: f64, t2: f64, t3: f64, t4: f64) -> (f64, f64) {
    let offset = ((t2 - t1) + (t3 - t4)) / 2.0;
    let delay = (t4 - t1) - (t3 - t2);
    (offset, delay)
}

#[cfg(test)]
mod precision_tests {
    use super::*;

    /// Timestamps must keep the wire's resolution, not the epoch's.
    ///
    /// An NTP timestamp carries 2^-32 s — 0.233 ns. Unix time is around
    /// 1.79e9 seconds, and an f64 there has a 238 ns gap between representable
    /// values, so the moment a timestamp is expressed as seconds-since-1970 in
    /// an f64, three orders of magnitude of it are gone. Worse, it is gone on a
    /// schedule: when Unix time crosses 2^31 in **February 2038** the exponent
    /// steps and the gap doubles to 477 ns.
    ///
    /// The daemon therefore takes differences in the fixed-point domain, where
    /// the subtraction is exact, and only then converts. This test pins that
    /// property by showing the two routes disagree by far more than the
    /// quantity being measured.
    /// One tick of the NTP fraction: 2^-32 s, about 233 ps. This is the finest
    /// distinction the wire format can draw, so it is the right tolerance for
    /// any claim about timestamp arithmetic — a tighter one is testing the
    /// test, not the code.
    const TICK: f64 = 1.0 / 4_294_967_296.0;

    #[test]
    fn differences_keep_sub_nanosecond_resolution() {
        // A realistic 2026 instant, and a second one 1 ns later.
        let secs = 1_787_856_000i64;
        let a = NtpTimestamp::from_unix(secs, 0);
        let b = NtpTimestamp::from_unix(secs, 1);

        // The exact route: subtract in fixed point, then convert.
        let exact = b.seconds_since(a);
        assert!(
            exact > 0.0,
            "a 1 ns step vanished entirely in the fixed-point difference"
        );
        assert!(
            (exact - 1e-9).abs() <= TICK,
            "fixed-point difference gave {exact} s for a 1 ns step"
        );

        // The lossy route: seconds-since-1970 as f64, then subtract.
        let ulp = (secs as f64).next_up() - secs as f64;
        assert!(
            ulp > 200e-9,
            "this test assumes an f64 at the Unix epoch is coarse; ULP is {ulp} s"
        );
    }

    /// The same, at the 2038 boundary — where it gets worse rather than breaking.
    #[test]
    fn the_2038_exponent_step_does_not_reach_the_difference() {
        // 2038-01-19, just past 2^31 seconds.
        let secs = 2_147_500_000i64;
        let a = NtpTimestamp::from_unix(secs, 0);
        let b = NtpTimestamp::from_unix(secs, 100);
        let exact = b.seconds_since(a);
        assert!(
            (exact - 100e-9).abs() <= TICK,
            "a 100 ns step past 2038 measured as {exact} s"
        );

        // Meanwhile the f64-seconds representation there cannot even hold it.
        let ulp = (secs as f64).next_up() - secs as f64;
        assert!(
            ulp > 400e-9,
            "expected the post-2038 f64 gap to exceed 400 ns, got {ulp} s"
        );
    }

    /// A difference must stay correct across the 2036 era wrap, which is the
    /// other half of why the daemon no longer guesses an era from the local
    /// clock: for spans this short the arithmetic is unambiguous on its own.
    #[test]
    fn a_difference_spans_the_era_boundary() {
        // Straddle 2036-02-07, where the NTP second count wraps.
        let before = NtpTimestamp::from_unix(2_085_978_495, 0);
        let after = NtpTimestamp::from_unix(2_085_978_497, 0);
        let delta = after.seconds_since(before);
        assert!(
            (delta - 2.0).abs() <= TICK,
            "two seconds across the era wrap measured as {delta}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_header() {
        let p = NtpPacket {
            leap: LeapIndicator::NoWarning,
            version: 4,
            mode: Mode::Server,
            stratum: 2,
            poll: 6,
            precision: -20,
            root_delay: NtpShort::from_seconds(0.015),
            root_dispersion: NtpShort::from_seconds(0.002),
            reference_id: *b"GPS\0",
            reference_ts: NtpTimestamp::from_unix(1_756_200_000, 0),
            origin_ts: NtpTimestamp(0x0102_0304_0506_0708),
            receive_ts: NtpTimestamp::from_unix(1_756_200_100, 500_000_000),
            transmit_ts: NtpTimestamp::from_unix(1_756_200_100, 500_100_000),
        };
        let bytes = p.to_bytes();
        let q = NtpPacket::parse(&bytes).expect("parse back");
        assert_eq!(p, q);
    }

    #[test]
    fn field_offsets_match_rfc5905() {
        // Hand-check the byte layout against the RFC figure.
        let mut p = NtpPacket::client_request(4, NtpTimestamp(0xAABB_CCDD_EEFF_0011));
        p.origin_ts = NtpTimestamp(0x1111_1111_1111_1111);
        p.receive_ts = NtpTimestamp(0x2222_2222_2222_2222);
        p.reference_ts = NtpTimestamp(0x3333_3333_3333_3333);
        let b = p.to_bytes();
        assert_eq!(b[0], 0b00_100_011); // LI 0, VN 4, mode 3 (client)
        assert_eq!(&b[16..24], &[0x33; 8]); // reference
        assert_eq!(&b[24..32], &[0x11; 8]); // origin
        assert_eq!(&b[32..40], &[0x22; 8]); // receive
        assert_eq!(&b[40..48], &0xAABB_CCDD_EEFF_0011u64.to_be_bytes()); // transmit
    }

    #[test]
    fn short_and_bad_version_are_errors() {
        assert_eq!(
            NtpPacket::parse(&[0u8; 20]),
            Err(ParseError::TooShort { len: 20 })
        );
        let mut b = [0u8; 48];
        b[0] = 2 << 3; // version 2
        assert_eq!(
            NtpPacket::parse(&b),
            Err(ParseError::BadVersion { version: 2 })
        );
    }

    #[test]
    fn timestamp_wraparound_diff() {
        // 1 second across the era boundary.
        let before = NtpTimestamp(u64::MAX - (1u64 << 31)); // ~0.5s before wrap
        let after = NtpTimestamp(1u64 << 31); // ~0.5s after wrap
        let d = after.seconds_since(before);
        assert!((d - 1.0).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn exchange_math() {
        // Local is 0.100 s behind; RTT 0.050 s symmetric.
        let t1 = 10.000; // local send
        let t2 = 10.125; // server recv = true 10.025 + 0.100
        let t3 = 10.126;
        let t4 = 10.051; // local recv (true 10.151 - 0.100... local scale)
        let (offset, delay) = offset_delay(t1, t2, t3, t4);
        assert!((offset - 0.100).abs() < 1e-9, "offset {offset}");
        assert!((delay - 0.050).abs() < 1e-9, "delay {delay}");
    }

    #[test]
    fn extension_iteration_handles_garbage() {
        // Header + one valid EF + trailing garbage.
        let mut buf = vec![0u8; 48];
        buf[0] = (4 << 3) | 3;
        buf.extend_from_slice(&0x0104u16.to_be_bytes()); // type
        buf.extend_from_slice(&16u16.to_be_bytes()); // len 16
        buf.extend_from_slice(&[0xAB; 12]); // value
        buf.extend_from_slice(&[1, 2, 3]); // garbage tail
        let items: Vec<_> = extension_fields(&buf).collect();
        assert_eq!(items.len(), 2);
        match items[0] {
            Trailer::Extension(ef) => {
                assert_eq!(ef.field_type, 0x0104);
                assert_eq!(ef.value, &[0xAB; 12][..]);
            }
            _ => panic!("expected extension"),
        }
        assert!(matches!(items[1], Trailer::Opaque(&[1, 2, 3])));
    }
}
