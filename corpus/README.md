# TIMECORP — the rusty_time performance corpus

See mission plan §7 for the full method. Quick use:

```sh
cargo run --release -p rusty_time-sim -- list
cargo run --release -p rusty_time-sim -- run --seeds 31
cargo run --release -p rusty_time-sim -- run --scenario S1 --seeds 5 --no-ledger  # smoke
cargo run --release -p rusty_time-sim -- serverload                               # S12
```

Interop gates (need a Linux box or WSL with chrony built from source):

```sh
tools/corpus/nts_interop_chrony.sh    # M3: NTS, both directions
tools/corpus/m4_interop_chrony.sh     # M4: plain + interleaved, with offset checks
tools/corpus/xleave_probe.sh          # instrumented interleaved diagnosis
```

**A gate must check the numbers, not just the shape.** The first M4 gate passed
while chrony was computing a 362 ms offset, because it only asserted that
interleaved mode had been negotiated. Any new interop gate asserts the measured
offset and delay too.

- `LEDGER.md` — append-only record of runs; the only citable source of numbers.
- `results/` — per-scenario JSON (all seeds + aggregate), git-ignored.

## Arms

| Arm | What it measures | Where it runs |
|---|---|---|
| sim harness | rusty_time's discipline stack in the deterministic plant | anywhere (`timecorp`) |
| clknetsim | rusty_time vs chrony vs ntpd-rs, same simulated world | Linux rig (`corpus.yml`) — validation of libc interception for Rust binaries is M2's open item |
| hardware | GPS/PPS ground truth | lab box, M7 |
