> **In the wild** — [RAG Converter](https://ragconverter.com) uses `rusty_time-core` to put an NTP timestamp on every chunk.
> It makes personal and work files AI-readable without them leaving the machine:
> the whole conversion runs as WebAssembly in the browser tab, with nothing
> uploaded and nothing to install.

# rusty_time-core

Portable NTPv4 protocol and clock-discipline algorithms.

This crate knows bytes, timestamps and estimates. It performs **no I/O, reads no OS clock,
and holds no product types** — a developer who has never heard of the rest of the workspace
can drive it, and it compiles for `wasm32` unchanged.

- **`ntp`** — RFC 5905 packet codec, extension fields (RFC 7822), timestamp arithmetic.
- **`filter`** — the sample register and a weighted linear regression that measures
  frequency *directly*, rather than inferring it through a PLL time constant.
- **`select`** — falseticker rejection by interval intersection.
- **`discipline`** — turns estimates into clock commands (step, slew, drain budget).
- **`client`** — `SyncController`, the loop the daemon and the simulator both run.
- **`server`** — client table, token-bucket rate limiting, Kiss-o'-Death, interleaved mode.

Sign convention throughout: an offset is **the number of seconds to add to the local
clock** to match the source (RFC 5905 theta). Positive offset means the local clock is behind.

## The `no_std` leaf

`ntp` builds on every target with **no `std` and no `alloc`** — the NTPv4 packet
codec, RFC 7822 extension iteration, and the RFC 5905 §8 offset/delay
arithmetic. It reads no clock, opens no socket and owns no buffer, so an SNTP
client on a Cortex-M4F or RV32 part needs nothing else:

```toml
rusty_time-core = { version = "0.2", default-features = false }
```

Everything above the wire — `filter`, `select`, `discipline`, `client`,
`server`, `config`, `refclock`, `vclock` — needs `Vec`/`String` and stays behind
the default-on `std` feature.

```rust
use rusty_time_core::ntp::{NtpPacket, NtpTimestamp, offset_delay};

// The nonce SHOULD be unpredictable rather than the real clock: it is echoed
// back as origin_ts and is the only spoofing defence an unauthenticated
// client has.
let req = NtpPacket::client_request(4, NtpTimestamp(nonce));
let wire: [u8; 48] = req.to_bytes();     // no allocation

// ...send `wire`, receive 48 bytes back...
let resp = NtpPacket::parse(&reply)?;     // never panics on untrusted bytes
let (offset, delay) = offset_delay(t1, t2, t3, t4);
```

### Embedded platforms

| platform | status |
|---|---|
| `thumbv7em-none-eabihf` (Cortex-M4F) | **compile-gated in CI**, every push |
| `riscv32imac-unknown-none-elf` (RV32) | **compile-gated in CI**, every push |
| `xtensa-esp32s3-none-elf` (ESP32-S3) | **run on the part** — 30/30 checks |

Three separate claims, deliberately. The CI rungs prove the leaf **builds**, so
the `no-std` category on this crate is measured rather than a label. The leaf's
own test suite runs on the host against the same `no_std` code path
(`cargo test -p rusty_time-core --no-default-features`) to prove it is
**right**. And a hand-run firmware proves it **runs on silicon** — an ESP32-S3
building a request, parsing a response off a hand-written wire image and
computing the RFC 5905 offset in soft-float, with no heap linked at all:

```text
checks passed 30 / 30
RESULT: PASS -- the rusty_time-core leaf ran on the board
```

The firmware, the full board output and what it does *not* claim are in
[bare-metal/esp32s3/](https://github.com/remade-with-rust/rusty_time/tree/main/bare-metal/esp32s3).
It is hand-run because it needs Espressif's Rust fork, which no CI runner has.

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/main/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
