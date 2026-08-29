> **In the wild** — [RAG Converter](https://ragconverter.com) uses `rusty_time` to put an NTP timestamp on every chunk.
> It makes personal and work files AI-readable without them leaving the machine:
> the whole conversion runs as WebAssembly in the browser tab, with nothing
> uploaded and nothing to install.

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
| **G2 convergence** | **Mixed.** S1 and S8 reach 1 ms in **5 s** against chrony's 7 s, and S4 (10% loss) reaches it at **1300 s** where chrony takes 1704 s; S6 (500 ms cold start) takes **14–16 s** against chrony's 12 s. |
| **G1 accuracy** | **Not met — level on all five.** 40 seeded worlds per scenario, paired: nothing resolved either way (S1 16/40, S6 16/40, S8 19/40, S2 24/40, S4 20/40). Ahead **per packet spent** on S1, S2 and S8 — chrony's accuracy for fewer packets. |
| G3, G5, G6 | not measured; nothing claimed. |

Steady-state p50 (last quarter), 40 seeded worlds, chrony vs rusty_time:
S1 0.6 → 0.8 µs, S6 0.7 → 0.8 µs, S8 3.3 → **4.3 µs**, S2 6.642 → **6.601 ms**,
S4 1.402 → **1.383 ms**. `0.1.9` chooses the regression window by the standard
error of the **predicted offset** rather than of the slope, which took S8 from
9.1 to 4.3 µs and S6 from 1.0 to 0.8 µs; S6 and S8 were both *resolved behind*
chrony before that and are now level.

Instruction cost, deterministic and internal (not a chrony comparison):
**237 Ir per served request**, **13,613 Ir per client discipline step** — within
1 Ir of `0.1.7` despite the multi-source rework.

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

### Two things to read before deploying this

**Poll interval matters less than it did, but still matters.** Over a simulated
day on the drifty-oscillator scenario, last-quarter mean error:

| `maxpoll` | rusty_time | chrony |
|---|---|---|
| 6 (64 s) | 17.9 µs | 4.3 µs |
| 10 (1024 s), the default | 127 µs | 10.0 µs |

`0.1.7` chooses the regression window from the data, which took the default-poll
figure from 1452 µs to 127 µs. A gap remains at long polls; `--maxpoll 6` is
still the better setting if microseconds matter.

**Multi-source selection matches chrony.** Three servers, one of them five
seconds wrong (`tools/corpus/multisource.sh`), final error after 600 simulated
seconds:

| | worlds within 1 ms | worst final error |
|---|---|---|
| `0.1.7` | 4 / 8 | 2996 ms — captured by the liar |
| `0.1.9` | **16 / 16** | **0.004 ms** |
| chrony | 16 / 16 | 0.005 ms |

The fix was structural, not a tuning knob. Every source used to run its own
copy of the discipline loop — its own frequency, drain and budget — while only
the selected one was allowed to reach the clock, which left every unselected
source having booked a correction that never happened. Seven different ways of
cleaning up after that were measured and every one was worse than leaving the
wrong books alone. There is now **one loop per clock and one register per
source**, so an unselected source never produces a plan at all and there is
nothing to clean up.

### Bounding what a source may do to your clock

```sh
rtimed sync <server> --maxchange 1000 1 2
```

chrony's `maxchange`, and **off by default** exactly as chrony's is — the right
value is a policy question about the deployment, not something a library can
guess. Beyond `start` updates, a correction larger than `offset` seconds is
refused and the clock is left running exactly as it was; after `ignore`
consecutive refusals the daemon exits rather than keep reporting a
synchronisation it is not performing. A negative `ignore` never exits.

It matters more here than on a server you own. Authentication proves who a
source is, not that it is telling the truth — and on a mesh, a capability's
expiry is decided by this clock, so anything that can move it can move the
boundary between *revoked* and *valid*.

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
