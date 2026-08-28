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

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/main/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
