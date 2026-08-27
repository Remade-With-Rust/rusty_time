# rusty_time-wasm

A disciplined virtual clock for wasm targets.

A browser page has no UDP socket, so it cannot speak NTP the ordinary way. This crate runs
**the same codec and the same sample filter as the native client** and carries real NTPv4
packets over an HTTP gateway, rather than inventing a JSON time API that would drift from
the protocol everything else uses.

The result is a `VirtualClock`: a monotonic base plus a measured offset and frequency,
disciplined from real NTP exchanges, so a page can hold time that is better than
`Date.now()` and can say how well it knows it.

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/main/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
