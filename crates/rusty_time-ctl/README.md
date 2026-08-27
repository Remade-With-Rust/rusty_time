# rusty_time-ctl (`rtimec`)

The control client for `rtimed` — the chronyc analog.

```sh
rtimec tracking      # offset, frequency, root distance
rtimec sources       # per-source state and selection
rtimec clients       # who has been asking, most recent first
rtimec ntsdata       # key ring and cookie state
```

Speaks the typed ops in `rusty_time-api` over a local control socket — a Unix socket, or a
named pipe on Windows. `--json` prints the wire form unchanged, so anything a human can
read here a script can consume.

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/master/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
