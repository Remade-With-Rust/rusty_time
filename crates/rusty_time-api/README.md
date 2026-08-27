# rusty_time-api

The typed op and report surface for rusty_time: the JSON wire between the `rtimed` daemon
and the `rtimec` control client.

Every capability is an **op** before it is a command, so it is callable by a CLI, a test
and an agent — never only from inside an event handler. The types here are the contract;
the daemon implements them and the control client consumes them.

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/master/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
