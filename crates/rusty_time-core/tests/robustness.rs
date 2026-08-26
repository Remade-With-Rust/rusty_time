//! Seeded structured-random robustness sweep: the in-tree stand-in for the
//! libFuzzer targets (which need nightly + libFuzzer and run in CI). Same
//! property: no input may panic the parsers.

use rusty_time_core::{config, ntp};

/// Tiny deterministic generator (xorshift64*) — no dependency, same sequence on
/// every platform.
struct X(u64);
impl X {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[test]
fn ntp_parse_never_panics() {
    let mut rng = X(0xDEAD_BEEF);
    let mut buf = [0u8; 256];
    for iteration in 0..200_000u32 {
        let len = (rng.next() as usize) % buf.len();
        for b in buf[..len].iter_mut() {
            *b = rng.next() as u8;
        }
        // Bias half the cases toward nearly-valid packets: correct version bits
        // exercise the deep paths instead of the version gate.
        if iteration % 2 == 0 && len > 0 {
            buf[0] = (rng.next() as u8 & 0b1100_0111) | (4 << 3);
        }
        if let Ok(p) = ntp::NtpPacket::parse(&buf[..len]) {
            let _ = p.to_bytes();
        }
        for _ in ntp::extension_fields(&buf[..len]).take(64) {}
    }
}

#[test]
fn config_parse_never_panics() {
    let mut rng = X(0xC0FF_EE00);
    let words = [
        "server",
        "pool",
        "makestep",
        "maxslewrate",
        "allow",
        "deny",
        "iburst",
        "minpoll",
        "maxpoll",
        "0.5",
        "-3",
        "abc",
        "#",
        "%",
        "\u{FFFD}",
        "",
        "999999999999999999",
    ];
    for _ in 0..50_000u32 {
        let n = (rng.next() as usize) % 8;
        let mut line = String::new();
        for _ in 0..n {
            line.push_str(words[(rng.next() as usize) % words.len()]);
            line.push(if rng.next().is_multiple_of(4) {
                '\n'
            } else {
                ' '
            });
        }
        let _ = config::parse(&line);
    }
}
