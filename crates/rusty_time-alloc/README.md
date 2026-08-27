# rusty_time-alloc

The allocator seam: one crate, one pin.

A program may define exactly one `#[global_allocator]`. A *library* that declares one
forces the choice on every consumer and makes two such libraries impossible to link
together — so the declaration belongs in the deliverable, and the choice of which
allocator belongs in exactly one place.

This crate is that place. The binaries (`rtimed`, `rtimec`, `timecorp`) install it; the
libraries never mention an allocator at all.

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/main/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
