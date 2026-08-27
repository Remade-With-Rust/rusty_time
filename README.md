# rusty_time

[![crates.io](https://img.shields.io/crates/v/rusty_time-core.svg)](https://crates.io/crates/rusty_time-core)
[![docs.rs](https://docs.rs/rusty_time-core/badge.svg)](https://docs.rs/rusty_time-core)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

chrony, remade with Rust. A pure-Rust, memory-safe NTP/NTS client + server targeting
Linux, macOS, Windows, and wasm.

- Mission plan: [docs/plans/time_mission.md](docs/plans/time_mission.md)
- Performance ledger: [corpus/LEDGER.md](corpus/LEDGER.md)

**Claims discipline:** this README makes no performance claim that is not in the ledger
with the run that produced it. The corpus (TIMECORP) measures rusty_time against chrony
(primary baseline) and ntpd-rs (secondary); see the mission plan §7.

## Status

Pre-1.0. Milestones **M0–M6 complete, M7 partial** (its exit test needs a
GPS/PPS lab box — see [corpus/LEDGER.md](corpus/LEDGER.md)).

What works: NTPv4 client and server, **NTS (RFC 8915)** both ends with
SpaceDB-backed persistence, **interleaved mode** (RFC 9769), per-client and
global rate limiting, `recvmmsg` batching, an ops control plane, platform clock
drivers with service integration and packaging, a **wasm client** for browsers
and edge functions, and reference clocks over gpsd SHM, chrony SOCK and PTP
hardware clocks.

Interop is verified against chrony in both directions and all three modes:
chronyd selects an `rtimed` server as its synchronisation source over plain NTP
(−4.8 µs), over NTS, and in interleaved mode (**−66 ns**). The parsers that
face untrusted bytes survive ~30 million fuzz executions with zero crashes.
TLS is rustls on a pure-Rust crypto provider; nothing in the build needs a C
toolchain, on any of the eight supported targets.

### Measured against chrony

Under **clknetsim** — chrony's own simulator, so the baseline uses the tooling
its authors trust. Both arms share one config per scenario and talk to the same
`chronyd` stratum-1 server; error is read from the simulator's ground-truth
offset log, not from either daemon's opinion of itself. A **null arm** (chrony
against itself) states the rig's own resolution, and verdicts come from a paired
sign test across rounds, not from comparing two medians.

| gate | result |
|---|---|
| **G4 server throughput** | **MET.** 138,664 replies/s vs chrony's 105,668 (**1.31×**) at **0.72×** the CPU per reply, both resolved far outside the null-arm floor. |
| **G2 convergence** | **Mixed.** S1 and S8 reach 1 ms in **5 s** against chrony's 7 s, and S4 (10% loss) reaches it at **1138 s** where chrony does not inside the run; S6 (500 ms cold start) takes **14–16 s** against chrony's 12 s. |
| **G1 accuracy** | **Not met — one win, one loss, three level.** 150 seeded worlds per scenario, paired: **S2 ours** (z=+2.78), **S6 chrony's** (z=−2.61), S1/S4/S8 unresolved. Ahead **per packet spent** on S1, S2 and S8 — chrony's accuracy for fewer packets. |
| G3, G5, G6 | not measured; nothing claimed. |

Whole-run p50, five seeded reps, chrony → rusty_time: S1 1.4 → **0.8 µs**,
S4 3.927 → **1.082 ms**, S6 1.4 → **0.9 µs**, S8 5.1 → **3.5 µs**, S2
**6.662** → 6.729 ms. That window includes the convergence transient, which is
why it reads differently from the steady-state row above; the two measure
different things and neither supersedes the other.

Instruction cost, deterministic and internal (not a chrony comparison):
**237 Ir per served request**, **13,469 Ir per client discipline step**.

Three honest notes. Earlier per-scenario accuracy wins reported here were
**withdrawn**: they came from comparing medians on a rig whose unchanged control
arm moved further than the effect. G4's p99 half is not measured — at saturation
the number is the load generator's queue depth, not the server's responsiveness.
And there is **no cross-implementation CPU figure for the client** the way there
is for the server, which on a mesh is the side that runs on every node.

The rig is now seeded, so two identical arms produce bit-identical output and
its resolution floor is exactly zero. Any gap in the tables above is code.

Every figure above, with the run that produced it and the defects the
comparison uncovered, is in [corpus/LEDGER.md](corpus/LEDGER.md).

## Install

```sh
cargo install rusty_time-daemon    # rtimed — the daemon
cargo install rusty_time-ctl       # rtimec — the control client
cargo install rusty_time-sim       # timecorp — the corpus runner
```

As a library:

```toml
[dependencies]
rusty_time-core = "0.1"    # protocol + discipline, no I/O, wasm-clean
rusty_time-nts  = "0.1"    # NTS (RFC 8915)
```

```sh
# What can this machine actually do?
rtimec doctor

# Authenticated query
rtimed query time.cloudflare.com --nts

# Run an NTS-capable server (self-signs a dev certificate if none is given)
rtimed serve --nts --state /var/lib/rusty_time/state.spacedb

# Ask it what it is doing
rtimec serverstats
rtimec clients 20

# Install as a system service (systemd / launchd / Windows SCM)
rtimed service show          # print the unit for this platform
sudo rtimed service install  # write it where the platform expects
```

## In the browser

A page has no clock it may set, so `@remade-with-rust/rusty-time` estimates the
offset instead and serves a corrected `now()` with an honest error bound. The
wire format is a real NTPv4 packet, so the browser runs the same codec and
filter as the daemon:

```sh
bash tools/wasm/build-npm.sh                        # 31 KB wasm module
rtimed serve --gateway 127.0.0.1:8199 \
             --gateway-assets crates/rusty_time-wasm/pkg
# then open http://127.0.0.1:8199/
```

```js
const client = new TimeClient();
const rnd = new Uint32Array(2); crypto.getRandomValues(rnd);
const request = client.build_request(Date.now(), rnd[0], rnd[1]);
const reply = new Uint8Array(await (await fetch('/time',
  { method: 'POST', body: request })).arrayBuffer());
client.process_response(reply, Date.now(), performance.now());

client.now_ms(Date.now(), performance.now());   // corrected, never steps back
client.confidence_ms(performance.now());        // error bound, widens if stale
```

## Verifying a build

`tools/smoke/smoke.sh` runs the built binaries end to end on the current
platform without touching the system clock, and checks that the *measured*
offset is sane rather than merely that packets were exchanged:

```sh
RTIMED=./target/release/rtimed RTIMEC=./target/release/rtimec bash tools/smoke/smoke.sh
```

## Layout

```
crates/rusty_time-core/    protocol + algorithms (portable, wasm-clean)
crates/rusty_time-nts/     NTS record layer + AEAD (RFC 8915)
crates/rusty_time-clock/   platform clock seam (linux / macos / windows / virtual)
crates/rusty_time-api/     typed op/report types (JSON public wire)
crates/rusty_time-daemon/  deliverable: rtimed
crates/rusty_time-ctl/     deliverable: rtimec
crates/rusty_time-wasm/    virtual clock for wasm targets
crates/rusty_time-alloc/   the allocator seam
crates/rusty_time-sim/     deliverable: timecorp — deterministic corpus runner
corpus/                    scenarios, results, LEDGER.md
fuzz/                      cargo-fuzz targets (packet, config, NTS records)
```

## License

MIT OR Apache-2.0. Clean-room implementation from RFCs 5905/7822/8915 and published
algorithm descriptions; no chrony (GPLv2) source is used.
