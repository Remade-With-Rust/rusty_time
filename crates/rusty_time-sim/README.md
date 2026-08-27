# rusty_time-sim (`timecorp`)

The deterministic corpus runner for rusty_time.

```sh
timecorp list                    # scenarios
timecorp run --seeds 31          # convergence and accuracy
timecorp serverload              # S12 admission-policy counts
timecorp load --target host:123  # NTP load generator
```

Every number is a count or an offset from a simulated clock and network — reproducible per
seed, immune to rig noise, and comparable across commits. The runner drives the **real**
`SyncController` and the **real** `ClientTable`, not a copy: benchmarking a simulator-only
reimplementation would measure code that never ships.

It also prints a split-half noise floor, so a future delta has an honest bar to clear.

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/main/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
