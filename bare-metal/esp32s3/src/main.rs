//! rusty_time-core's `no_std` leaf, running on an ESP32-S3.
//!
//! `build-me-bare.md` (Kairos) B2 asks for `rusty_time-core` to build `no_std`
//! on `thumbv7em-none-eabihf` and `riscv32imac-unknown-none-elf`. CI makes that
//! claim every push. This firmware makes the *other* claim -- strategy item 5's
//! "bare means a ledger row from a board" -- by running the leaf on silicon.
//!
//! It is the whole `rusty_rtos_sntp` contract and nothing else: build a mode-3
//! request, put it on the wire, parse a response off the wire, and compute the
//! RFC 5905 offset and delay. No heap, no clock, no socket -- this binary links
//! no allocator at all, which is a stronger statement than the `rusty_zstd`
//! board row next door (that one needs `alloc`).

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_println::println;

use rusty_time_core::ntp::{
    extension_fields, offset_delay, ExtensionField, LeapIndicator, Mode, NtpPacket, NtpShort,
    NtpTimestamp, ParseError, Trailer, HEADER_LEN,
};

esp_bootloader_esp_idf::esp_app_desc!();

/// `f64::abs` is a std-only inherent method, so it is unavailable here -- and a
/// host test would not have caught that, because the test harness links std and
/// the method resolves anyway. Spelled out, it works on every target.
fn diff(a: f64, b: f64) -> f64 {
    if a > b { a - b } else { b - a }
}

/// One tick of the NTP fraction: 2^-32 s, about 233 ps. The finest distinction
/// the wire format can draw, so the right tolerance for any claim about
/// timestamp arithmetic -- tighter would be testing the test.
const TICK: f64 = 1.0 / 4_294_967_296.0;

struct Checks {
    passed: u32,
    failed: u32,
}

impl Checks {
    fn check(&mut self, name: &str, ok: bool) {
        if ok {
            self.passed += 1;
            println!("  ok    {name}");
        } else {
            self.failed += 1;
            println!("  FAIL  {name}");
        }
    }
}

/// A literal stratum-2 server response, written out byte by byte rather than
/// produced by `to_bytes`. That is the point: parsing our own output would only
/// prove the codec is self-consistent. This image pins the RFC 5905 field
/// OFFSETS and big-endian byte order independently of the writer.
#[rustfmt::skip]
const SERVER_RESPONSE: [u8; HEADER_LEN] = [
    // LI = 0 (no warning), VN = 4, Mode = 4 (server)
    0x24,
    0x02,                                             // stratum 2
    0x06,                                             // poll 6 (64 s)
    0xEC,                                             // precision -20 (~1 us)
    0x00, 0x00, 0x03, 0xD7,                           // root delay      983 / 2^16
    0x00, 0x00, 0x00, 0x83,                           // root dispersion 131 / 2^16
    b'G', b'P', b'S', 0x00,                           // reference id
    0x11, 0x11, 0x11, 0x11, 0x22, 0x22, 0x22, 0x22,   // reference  ts
    0x33, 0x33, 0x33, 0x33, 0x44, 0x44, 0x44, 0x44,   // origin     ts
    0x55, 0x55, 0x55, 0x55, 0x66, 0x66, 0x66, 0x66,   // receive    ts
    0x77, 0x77, 0x77, 0x77, 0x88, 0x88, 0x88, 0x88,   // transmit   ts
];

#[esp_hal::main]
fn main() -> ! {
    let _p = esp_hal::init(esp_hal::Config::default());

    println!();
    println!("=== rusty_time-core leaf on ESP32-S3 (xtensa, no_std, NO alloc) ===");
    println!("header {HEADER_LEN} bytes, no heap linked, no clock read");
    println!();

    let mut c = Checks {
        passed: 0,
        failed: 0,
    };

    // ---- 1. The request half: what an SNTP client puts on the wire. ---------
    println!("[1] client request");
    let nonce = NtpTimestamp(0x1234_5678_9ABC_DEF0);
    let req = NtpPacket::client_request(4, nonce);
    let wire = req.to_bytes();

    c.check("request is exactly 48 bytes", wire.len() == HEADER_LEN);
    c.check("mode 3 + version 4 in byte 0", wire[0] == 0b00_100_011);
    c.check(
        "nonce lands in transmit_ts (bytes 40..48)",
        wire[40..48] == 0x1234_5678_9ABC_DEF0u64.to_be_bytes(),
    );
    let back = NtpPacket::parse(&wire).expect("our own request must parse");
    c.check("to_bytes -> parse round trip is lossless", back == req);
    c.check("transmit_ts survives the round trip", back.transmit_ts == nonce);
    println!();

    // ---- 2. The response half: parse a wire image we did not write. ---------
    println!("[2] server response (literal wire image)");
    let resp = NtpPacket::parse(&SERVER_RESPONSE).expect("stratum-2 response must parse");

    c.check("leap = NoWarning", resp.leap == LeapIndicator::NoWarning);
    c.check("version = 4", resp.version == 4);
    c.check("mode = Server", resp.mode == Mode::Server);
    c.check("stratum = 2", resp.stratum == 2);
    c.check("poll = 6", resp.poll == 6);
    c.check("precision = -20", resp.precision == -20);
    c.check("root_delay = 983/2^16", resp.root_delay == NtpShort(983));
    c.check("root_dispersion = 131/2^16", resp.root_dispersion == NtpShort(131));
    c.check("reference_id = GPS\\0", &resp.reference_id == b"GPS\0");
    c.check(
        "reference_ts at bytes 16..24",
        resp.reference_ts == NtpTimestamp(0x1111_1111_2222_2222),
    );
    c.check(
        "origin_ts at bytes 24..32",
        resp.origin_ts == NtpTimestamp(0x3333_3333_4444_4444),
    );
    c.check(
        "receive_ts at bytes 32..40",
        resp.receive_ts == NtpTimestamp(0x5555_5555_6666_6666),
    );
    c.check(
        "transmit_ts at bytes 40..48",
        resp.transmit_ts == NtpTimestamp(0x7777_7777_8888_8888),
    );
    c.check(
        "re-serializing reproduces the wire image",
        resp.to_bytes() == SERVER_RESPONSE,
    );
    println!(
        "      root delay {} s, dispersion {} s",
        resp.root_delay.to_seconds(),
        resp.root_dispersion.to_seconds()
    );
    println!();

    // ---- 3. The arithmetic: soft-float f64 on a part with no FPU for it. ----
    // Xtensa LX7 has a single-precision FPU and no f64 unit, so every division
    // below is a compiler-generated soft-float call. That is exactly why it is
    // worth running here rather than trusting the host.
    println!("[3] offset / delay (RFC 5905 section 8)");
    let t1_ts = NtpTimestamp::from_unix(1_787_856_000, 0);
    let t2_ts = NtpTimestamp::from_unix(1_787_856_000, 125_000_000);
    let t3_ts = NtpTimestamp::from_unix(1_787_856_000, 126_000_000);
    let t4_ts = NtpTimestamp::from_unix(1_787_856_000, 51_000_000);

    // Differences are taken in the fixed-point domain, where the subtraction is
    // exact, and only then converted -- an f64 holding seconds-since-1970 has a
    // 238 ns gap between representable values and would throw the resolution
    // away before the arithmetic started.
    let t1 = 0.0;
    let t2 = t2_ts.seconds_since(t1_ts);
    let t3 = t3_ts.seconds_since(t1_ts);
    let t4 = t4_ts.seconds_since(t1_ts);
    let (offset, delay) = offset_delay(t1, t2, t3, t4);

    println!("      offset {offset} s   delay {delay} s");
    c.check("offset = +0.100 s (local is behind)", diff(offset, 0.100) <= TICK);
    c.check("delay = 0.050 s round trip", diff(delay, 0.050) <= TICK);

    // A 1 ns step must survive: the wire carries 2^-32 s and the daemon's whole
    // precision claim rests on the difference being taken before the convert.
    let a = NtpTimestamp::from_unix(1_787_856_000, 0);
    let b = NtpTimestamp::from_unix(1_787_856_000, 1);
    let step = b.seconds_since(a);
    println!("      1 ns step measured as {step} s");
    c.check("a 1 ns step does not vanish", step > 0.0);
    c.check("and lands within one NTP tick", diff(step, 1e-9) <= TICK);

    // The 2036 era wrap: the NTP second count rolls over and the difference has
    // to stay correct across it. Wrapping u64 arithmetic on a 32-bit part.
    let before = NtpTimestamp::from_unix(2_085_978_495, 0);
    let after = NtpTimestamp::from_unix(2_085_978_497, 0);
    let across = after.seconds_since(before);
    println!("      2 s across the 2036 era wrap measured as {across} s");
    c.check("era wrap does not break the difference", diff(across, 2.0) <= TICK);
    println!();

    // ---- 4. Extension fields (RFC 7822) and the refusals. -------------------
    println!("[4] extension fields and rejections");
    let mut ext = [0u8; HEADER_LEN + 16 + 3];
    ext[..HEADER_LEN].copy_from_slice(&SERVER_RESPONSE);
    ext[48..50].copy_from_slice(&0x0104u16.to_be_bytes()); // NTS cookie type
    ext[50..52].copy_from_slice(&16u16.to_be_bytes()); // total length 16
    ext[52..64].copy_from_slice(&[0xAB; 12]); // value
    ext[64..67].copy_from_slice(&[1, 2, 3]); // a legacy MAC stub

    let mut it = extension_fields(&ext);
    let first = it.next();
    c.check(
        "one well-formed extension field is yielded",
        matches!(
            first,
            Some(Trailer::Extension(ExtensionField {
                field_type: 0x0104,
                ..
            }))
        ),
    );
    c.check(
        "the unparseable tail comes back as Opaque",
        matches!(it.next(), Some(Trailer::Opaque(&[1, 2, 3]))),
    );
    c.check("iteration then ends", it.next().is_none());

    // Nothing here may panic on untrusted bytes -- on a chip a panic is a
    // reboot, so the parse-constructors earning their Result matters more here
    // than it does on a host.
    c.check(
        "a 20-byte runt is refused, not panicked on",
        NtpPacket::parse(&[0u8; 20]) == Err(ParseError::TooShort { len: 20 }),
    );
    let mut v2 = SERVER_RESPONSE;
    v2[0] = 2 << 3; // version 2
    c.check(
        "NTPv2 is refused",
        NtpPacket::parse(&v2) == Err(ParseError::BadVersion { version: 2 }),
    );
    c.check("an empty slice is refused", NtpPacket::parse(&[]).is_err());
    println!();

    // ---- verdict ------------------------------------------------------------
    println!("checks passed {} / {}", c.passed, c.passed + c.failed);
    if c.failed == 0 {
        println!("RESULT: PASS -- the rusty_time-core leaf ran on the board");
    } else {
        println!("RESULT: FAIL -- {} check(s) failed", c.failed);
    }

    loop {
        core::hint::spin_loop()
    }
}
