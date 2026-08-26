# TIMECORP — the rusty_time performance corpus

See mission plan §7 for the full method. Quick use:

```sh
cargo run --release -p rusty_time-sim -- list
cargo run --release -p rusty_time-sim -- run --seeds 31
cargo run --release -p rusty_time-sim -- run --scenario S1 --seeds 5 --no-ledger  # smoke
```

- `LEDGER.md` — append-only record of runs; the only citable source of numbers.
- `results/` — per-scenario JSON (all seeds + aggregate), git-ignored.

## Arms

| Arm | What it measures | Where it runs |
|---|---|---|
| sim harness | rusty_time's discipline stack in the deterministic plant | anywhere (`timecorp`) |
| clknetsim | rusty_time vs chrony vs ntpd-rs, same simulated world | Linux rig (`corpus.yml`) — validation of libc interception for Rust binaries is M2's open item |
| hardware | GPS/PPS ground truth | lab box, M7 |
