# rusty_time — Mission Plan

> **Mission:** `rusty_time` is chrony, remade with Rust. A pure-Rust, memory-safe NTP/NTS
> client + server that matches or beats chrony on convergence, steady-state accuracy, and
> server throughput — and reaches one platform chrony never can: **wasm**, so every app on
> the MATA mesh has disciplined time, including edge functions and the browser.
>
> Reference target: [chrony](https://gitlab.com/chrony/chrony) (C, GPLv2).
> Part of the [Remade-With-Rust](https://github.com/remade-with-rust) family.

Status: **planning** · Owner: tim.almond@thehouseinc.xyz · Created: 2026-08-26

---

## 1. Why this exists

- **chrony is the best-in-class C implementation** — faster convergence and better accuracy
  than ntpd, excellent behavior on intermittent/congested networks, NTS support, hardware
  timestamping. It is also ~90k lines of C parsing untrusted UDP off the open internet as
  root-adjacent code. That is exactly the profile this org exists to delete.
- **The mesh needs time.** CRDT ordering hints, capability expiry, signed-grant validity
  windows, settlement metering — every MATA seam degrades quietly when clocks drift. A
  house time daemon is infrastructure for everything else we ship.
- **wasm is the differentiator.** chrony, ntpd, and ntpsec cannot follow us into the
  browser or a disco edge function. A `rusty_time` virtual clock (estimate offset/skew,
  serve corrected `now()` without touching the OS clock) is a capability nobody else has.

### Prior art — stated honestly

| Project | What it is | Our position |
|---|---|---|
| **chrony** (C) | The reference. Best convergence + accuracy in class. | The baseline every corpus number is measured against. |
| **ntpd-rs** (Rust, Prossimo/memorysafety.org) | Memory-safe NTP daemon with NTS, client+server. | The §1 ladder says check memorysafety.org first — we did. ntpd-rs is ntpd-class on discipline (kalman-style filter, no refclocks, no hardware timestamping, no wasm, no chrony-grade intermittent-network behavior). It is prior art and a **secondary corpus baseline**, not the target. If the corpus ever shows ntpd-rs beating us, that is a stop-and-explain finding. |
| **ntpsec** (C) | Hardened ntpd fork. | Optional tertiary baseline; not a design input. |

We must beat chrony on the corpus, not merely match ntpd-rs — otherwise the honest answer
to "why not ntpd-rs?" is "no reason," and this repo has no mission.

---

## 2. The one-line test, applied

> Could this ship as-is to a user who assumes their data is theirs alone, onto a machine
> you do not own, with no C toolchain anywhere in the build?

| Concern | Answer |
|---|---|
| C toolchain | None. TLS is **rustls**; AEAD is **RustCrypto** (`aes-siv` for NTS, RFC 8915); no `*-sys` crates that compile C. `libc`/`windows-sys` are binding-only crates (no C compiled) and are unavoidable for syscalls — stated here, out loud, once. |
| Data | Drift history, NTS cookies, server keys, client telemetry persist **per-entry, encrypted (AES-256-GCM), under compound keys** via `spacedb-sdk` — never a mutable blob like chrony's driftfile/dumpdir. |
| Identity | Local control socket authorizes by peer uid; **remote management requires an mID token** verified locally with `mid-verify`. No passwords, no chrony `keyfile` symmetric-key legacy (NTS replaces it). |
| Machine you don't own | The daemon is an OS service (needs clock privileges by nature), but the **wasm client and the time-gateway op deploy to the mesh** via `disco`. |
| Test rigs are exempt | chrony itself, clknetsim, and ntpd-rs live only on the Linux CI/bench rig as corpus baselines. Nothing C ships in any artifact. |

---

## 3. Scope — the chrony parity map

### v1.0 MUST (client + server core)

- NTPv4 client and server, RFC 5905, with extension fields (RFC 7822)
- **NTS** client and server: NTS-KE over TLS 1.3 (rustls), AES-SIV-CMAC-256 cookies (RFC 8915)
- chrony-grade sample filtering: per-source (offset, delay) register, **linear-regression
  frequency/offset estimation**, outlier rejection, delay-based weighting
- Source selection: falseticker detection (interval intersection), combining, weighting
- Clock discipline: slew via OS adjustment APIs, `makestep`-equivalent policy, `maxslewrate`,
  adaptive poll (`minpoll`/`maxpoll`), KoD RATE handling
- Intermittent-network behavior (chrony's heritage): offline/online sources, burst on
  reconnect, no panic on long gaps
- Leap second: apply (step/slew) on client; **leap smear** on server
- Server hardening: token-bucket rate limiting, `allow`/`deny` ACLs, interleaved mode, MRU/client log
- Ops-first control plane (`tracking`, `sources`, `sourcestats`, `serverstats`, `makestep`,
  `burst`, `online`/`offline`, …) — every one an API op before it is a CLI verb (§5)
- Config: chrony.conf-compatible directive subset (documented), so migration is an apt-get swap

### v1.x SHOULD

- Hardware timestamping (Linux `SO_TIMESTAMPING` + NIC `SIOCSHWTSTAMP`), PHC (`/dev/ptp*`)
- Refclocks: PPS (RFC 2783), SHM + SOCK (gpsd interop)
- RTC tracking (Linux `/dev/rtc`), temperature compensation (`tempcomp`)
- Windows software socket timestamps (`SIO_TIMESTAMPING`)
- Seccomp filter (Linux), capability drop to `CAP_SYS_TIME`-only

### NEVER

- NTPv3 symmetric-key MAC auth as a headline feature (NTS is the auth story; legacy MD5/SHA1
  keyfiles are a non-goal), broadcast/manycast modes, `cmdmon` UDP command port
  (control is local socket / named pipe + mID-authed remote op only)

---

## 4. Architecture — the scaffold

Per the house scaffold (skill §2). Names final unless the org objects:

```
rusty_time/
├── Cargo.toml                  # workspace; pins live here once; unsafe_code = "deny"
├── docs/plans/time_mission.md  # this file
├── crates/
│   ├── rusty_time-core/        # LIBRARY. Protocol + math. no_std-friendly, wasm-clean.
│   │                           # NTPv4 parse/format, filtering, selection, discipline
│   │                           # planning, rate limiting, leap logic. No I/O, no clock,
│   │                           # no allocator, no product types. A developer who has
│   │                           # never heard of MATA can use this crate.
│   ├── rusty_time-nts/         # LIBRARY. NTS-KE (rustls) + extension-field crypto (aes-siv).
│   ├── rusty_time-clock/       # LIBRARY. The platform seam: ClockRead / ClockDiscipline /
│   │                           # TimestampedUdp traits + linux/macos/windows/wasm impls (§6).
│   ├── rusty_time-api/         # LIBRARY. Typed ops over a Transport seam. JSON public,
│   │                           # oxicode on the internal control wire. mID auth for remote.
│   ├── rusty_time-daemon/      # DELIVERABLE `rtimed`. Allocator declared HERE (seam).
│   ├── rusty_time-ctl/         # DELIVERABLE `rtimec`. chronyc-equivalent over the api crate.
│   ├── rusty_time-wasm/        # DELIVERABLE. Browser/edge virtual clock (§6.4), npm + disco.
│   └── rusty_time-alloc/       # The rusty_alloc seam — one crate, one pin.
├── corpus/                     # §7: scenario files, seeds, runner, ledger
└── tests/interop/              # live-network + reference interop gates
```

Rules the layout enforces: `#[global_allocator]` only in deliverables; core knows bytes and
timestamps, never products; **every capability is an op** callable by CLI, test, and agent
before any UI exists; every persisted type per-entry encrypted in SpaceDB.

Persistence detail (the one place the doctrine needs care): the daemon must produce a
usable clock **before** storage is warm, so boot order is: read OS clock → start discipline
with defaults → load drift/cookies from the local SpaceDB replica when it opens (tens of
ms) → retro-apply frequency. Never block time-serving on storage.

---

## 5. Ops before buttons — the control plane

Every chronyc verb becomes a typed op in `rusty_time-api` first:

`status.tracking` · `status.sources` · `status.sourcestats` · `status.serverstats` ·
`status.ntsdata` · `ctl.makestep` · `ctl.burst` · `ctl.online` / `ctl.offline` ·
`ctl.add_source` / `ctl.del_source` · `ctl.reload` · `srv.allow` / `srv.deny` ·
`srv.smear` · `debug.selectdata` · `debug.clients`

- Local transport: Unix domain socket (Linux/macOS), named pipe (Windows) — authorized by
  peer credentials (uid / SID).
- Remote transport: the same ops over the SpaceDB `Transport` seam, **mID-token
  authenticated**, capability-scoped read-only vs control (`mata-cap`).
- Wire: JSON on anything public; oxicode internally. `rtimec` is a thin consumer; a test
  and an agent are consumers #2 and #3.

---

## 6. The build-function inventory — every seam, every platform

This is the complete list of platform functions we must implement to be deployable on
**Linux, macOS, Windows ("pc"), and wasm**. Three traits in `rusty_time-clock` carry all of
it; everything above them is portable core.

### 6.0 Portable core (all targets, including wasm)

| Function | Notes |
|---|---|
| `ntp::parse_packet` / `ntp::format_packet` | RFC 5905 + RFC 7822 extension fields; zero-alloc on the hot path |
| `nts::ke_client` / `nts::ke_server` | NTS-KE over rustls (TLS 1.3, ALPN `ntske/1`) |
| `nts::protect` / `nts::verify` | AES-SIV-CMAC-256 extension fields, cookie mint/rotate/redeem |
| `filter::SampleRegister::push / regress` | per-source (offset, delay, dispersion) history; linear-regression freq+offset estimate; outlier ejection |
| `select::select_sources` | falseticker interval intersection, weighting, combining |
| `discipline::plan` | offset/freq correction plan: slew rate, step decision (`makestep` policy), poll adaptation |
| `leap::schedule / smear` | client apply; server smear curve |
| `server::respond` | stateless response builder, interleaved mode |
| `server::RateLimiter` | token bucket + KoD RATE emission; MRU table |
| `config::parse` | chrony.conf-compatible subset → typed config |
| `virtual_clock::Estimator` | offset/skew model over any monotonic counter — shared by wasm and by holdover on all OSes |

### 6.1 Linux

| Seam function | Implementation |
|---|---|
| `clock.read_raw()` | `clock_gettime(CLOCK_REALTIME)` + `CLOCK_MONOTONIC_RAW` pairing |
| `clock.slew(freq_ppm, offset)` | `clock_adjtime(ADJ_FREQUENCY \| ADJ_OFFSET)`; precise step via `ADJ_SETOFFSET` |
| `clock.step(delta)` | `clock_settime` (only under makestep policy) |
| `clock.leap_arm(ins/del)` | `ADJ_STATUS` `STA_INS`/`STA_DEL`; `ADJ_TAI` for TAI offset |
| `udp.rx_timestamp()` | `SO_TIMESTAMPING` (SW + HW), `SCM_TIMESTAMPING` cmsg |
| `udp.tx_timestamp()` | `MSG_ERRQUEUE` drain for TX timestamps |
| `udp.hw_enable(ifname)` | `SIOCSHWTSTAMP`; PHC association |
| `udp.batch_recv()` | `recvmmsg` — the server-throughput lever chrony does not pull |
| `phc.read_offset()` | `/dev/ptp*`, `PTP_SYS_OFFSET_PRECISE` ioctl |
| `pps.fetch()` | RFC 2783 `time_pps_*` on `/dev/pps*` |
| `refclock.shm / sock` | gpsd SysV SHM segment (key `0x4e545030+u`) + chrony SOCK datagram protocol |
| `rtc.read / trim()` | `/dev/rtc` ioctls, drift file → SpaceDB entry |
| `priv.drop()` | keep `CAP_SYS_TIME` only; setuid to service user; **seccomp allowlist** |
| `svc.integrate()` | systemd unit, `sd_notify` readiness, socket activation for the control socket |
| packaging | `.deb` + `.rpm` + static **musl** tarball (x86_64, aarch64) |

### 6.2 macOS

| Seam function | Implementation |
|---|---|
| `clock.read_raw()` | `clock_gettime` + `mach_continuous_time` |
| `clock.slew(...)` | `ntp_adjtime` where honored; fallback: re-armed `adjtime(2)` micro-slews (chrony's macOS driver strategy) — **the estimator owns frequency, the OS only executes offsets** |
| `clock.step(delta)` | `clock_settime` (root; document SIP interaction) |
| `udp.rx_timestamp()` | `SO_TIMESTAMP` (µs, software RX only — no HW path exists; corpus must not pretend otherwise) |
| `priv.drop()` | start as root for clock ops, drop everything else; sandbox profile |
| `svc.integrate()` | `launchd` plist (KeepAlive), `launchctl` install op in `rtimec` |
| packaging | universal2 binaries (x86_64 + aarch64), codesigned + notarized `.pkg` |

### 6.3 Windows

| Seam function | Implementation |
|---|---|
| `clock.read_raw()` | `GetSystemTimePreciseAsFileTime` + `QueryPerformanceCounter` |
| `clock.slew(...)` | `SetSystemTimeAdjustmentPrecise` (100 ns units; Win10 1809+); legacy `SetSystemTimeAdjustment` fallback |
| `clock.step(delta)` | `SetSystemTime`; acquire `SeSystemtimePrivilege` explicitly |
| `udp.rx_timestamp()` | `SIO_TIMESTAMPING` ioctl (Win10 2004+); fallback QPC-at-recv with measured stack latency |
| `coexist.w32time()` | detect Windows Time service, refuse to fight it, offer a disable op |
| `svc.integrate()` | SCM service (pure-Rust `windows-service`), Event Log source, named-pipe control transport with SID auth |
| packaging | MSI (WiX) with service registration; `winget` manifest |

### 6.4 wasm32 — the virtual clock (no OS clock to touch)

| Seam function | Implementation |
|---|---|
| `clock.read_raw()` | `performance.now()` (browser) / WASI monotonic — feeds the shared `virtual_clock::Estimator` |
| `transport.exchange()` | **no raw UDP in a browser.** Two paths: (a) **WebTransport/QUIC datagrams** to a `rtimed` gateway op (preferred — real RTT symmetry), (b) authenticated HTTPS time endpoint (coarse fallback) |
| `vclock.now()` | corrected `SystemTime` for the app — offset+skew applied, never steps backward |
| `vclock.confidence()` | current error bound — mesh apps gate CRDT/capability decisions on it |
| `daemon.gateway_op` | the server-side half: `time.exchange` op on `rtimed`, NTS-protected, deployable to disco edge |
| packaging | `wasm32-unknown-unknown` npm package (`@remade-with-rust/rusty-time`) + `wasm32-wasip2` build for edge functions; `disco sites deploy` for the demo/status page |

### 6.5 Cross-cutting build gates (every PR, per workflow doctrine)

```sh
cargo check --target x86_64-unknown-linux-gnu \
            --target x86_64-unknown-linux-musl \
            --target aarch64-unknown-linux-musl \
            --target x86_64-pc-windows-msvc \
            --target aarch64-pc-windows-msvc \
            --target x86_64-apple-darwin \
            --target aarch64-apple-darwin \
            --target wasm32-unknown-unknown
```

- `unsafe_code = "deny"` workspace-wide; lifted per-crate only in `rusty_time-clock`
  (syscall boundary), each block with a `// SAFETY:` invariant
- Deputy owns the lockfile from commit one; no new dependency without the §1 ladder walk
- No `unwrap()` on any path a packet can reach — every parse is a parse-constructor
  returning `Result`; fuzz the packet and NTS parsers (`cargo-fuzz`) from M1
- `use-protection-please` audit before v1.0

---

## 7. The corpus — performance vs chrony

**TIMECORP v1.** The corpus is the mission's referee: chrony is the primary baseline,
ntpd-rs secondary. No performance claim leaves this repo — README included — unless it is
in `corpus/LEDGER.md` with the run that produced it (the rusty_zstd refusal discipline).

### 7.1 Method

- **Simulation first: [clknetsim](https://gitlab.com/chrony/clknetsim)** — chrony's own
  deterministic clock+network simulator, the tool its published ntpd comparisons use.
  Deterministic runs mean **counts and offsets, not durations** — the strongest kind of
  number, and immune to rig noise. Fairness note: clknetsim drives real daemon binaries
  through intercepted libc calls; the runner must confirm `rtimed`'s Rust std paths are
  fully intercepted (gettime/settime/adjtimex/send/recv) **before any number is admissible
  — this is M2's first deliverable, and if interception proves unreliable we build the
  equivalent simulator harness against the `rusty_time-clock` seam instead (same scenarios,
  same metrics).**
- **Hardware second:** a Linux lab box with GPS+PPS discipline as ground truth,
  cross-checking that simulation rankings survive contact with reality; plus per-OS smoke
  rigs (macOS, Windows) where the only baseline is that platform's chrony/W32Time behavior.
- **The measurement bar (house `performance.md` §2) applies in full:** both arms identical
  work and identical scenario seeds; chrony run with **documented, tuned defaults**
  (`iburst`, sane `makestep`, same poll bounds — not a strawman config); N ≥ 31 seeds per
  scenario, report median **and** worst-case; a **null arm** (chrony-vs-chrony across seed
  sets) establishes the noise floor any claimed win must clear; hardware runs pinned +
  ABBA-interleaved.
- **Correctness gates before any speed number:** live interop (sync from chrony, pool
  servers; serve to chronyd/ntpd-rs clients), NTS interop (against chrony NTS and
  time.cloudflare.com), RFC 5905 packet conformance suite, fuzzers clean.

### 7.2 Scenarios

| # | Scenario | Config sketch | What it stresses |
|---|---|---|---|
| S1 | LAN symmetric | 0.2 ms RTT, σ=10 µs jitter | best-case convergence + floor accuracy |
| S2 | WAN asymmetric | 40 ms RTT, 2:1 path asymmetry | asymmetry bias handling |
| S3 | Wi-Fi heavy tail | Pareto jitter, occasional 50 ms spikes | filter/outlier quality |
| S4 | Congested/lossy | 5–20 % loss, bursty delay | robustness, poll adaptation |
| S5 | Intermittent | online 5 min / offline 55 min cycles | chrony's home turf — must not lose here |
| S6 | Cold start, small step | 500 ms initial offset | initial convergence, iburst |
| S7 | Cold start, big step | 120 s initial offset | makestep policy, step-vs-slew |
| S8 | Drifty oscillator | +100 ppm bias, random-walk wander | frequency tracking |
| S9 | Temperature ramp | freq wander correlated ±30 ppm over hours | tracking under correlated drift |
| S10 | Holdover | 24 h upstream loss after 1 h sync | drift prediction quality |
| S11 | Leap second | insertion, client + smeared server | leap correctness |
| S12 | Server load | 1k → 100k clients, Poisson arrivals | throughput, rate limiter, MRU |
| S13 | NTS steady state | S1/S2 with NTS on | crypto overhead on accuracy + CPU |
| S14 | NTS-KE storm | 1k concurrent handshakes | rustls handshake cost, cookies/sec |
| HW1 | GPS/PPS lab box | 48 h vs PPS ground truth | reality check on S1/S8/S10 rankings |

### 7.3 Metrics (per scenario, per arm)

- **Convergence:** time (and packets spent — network cost) to |offset| < 10 ms, < 1 ms, < 100 µs
- **Steady state:** residual |offset| median / p95 / max; frequency error (ppm); Allan
  deviation at τ = 8 s … 4096 s
- **Holdover:** |offset| at +1 h / +6 h / +24 h after loss
- **Server:** sustained responses/sec at < 10 µs added latency; p50/p99 response latency;
  memory per 10k tracked clients
- **Footprint:** RSS steady-state, CPU-seconds per simulated day, binary size, syscalls/poll
  (`strace -c` — a deterministic count)

### 7.4 Release gates (v1.0 ships when TIMECORP says)

| Gate | Bar |
|---|---|
| G1 accuracy | steady-state p95 offset ≤ chrony's on ≥ 12 of 14 sim scenarios, **beat** on ≥ 7; no scenario > 1.25× chrony |
| G2 convergence | S6 time-to-1 ms ≤ chrony; S7 respects the same makestep policy chrony is configured with |
| G3 holdover | S10 24 h drift ≤ chrony ± noise floor |
| G4 server | S12 responses/sec ≥ chrony on the same rig (recvmmsg is our lever); p99 ≤ chrony |
| G5 footprint | RSS and CPU/day ≤ 1.5× chrony (report honestly; chrony is very lean — parity is a fine v1 story, per the rusty_alloc precedent) |
| G6 vs ntpd-rs | ≥ chrony-level margin over ntpd-rs on S1–S10 — the "why not ntpd-rs" receipt |
| G7 correctness | all §7.1 interop + conformance + fuzz gates green — a speed win that fails interop is a bug with good timing |

`corpus/LEDGER.md` records every run: date, commit, scenario set, seeds, numbers, verdict.
Wins may be cited; anything not in the ledger does not exist.

---

## 8. Milestones — one brick at a time

| M | Deliverable | Exit test |
|---|---|---|
| M0 | Workspace scaffold, alloc seam, Deputy, CI check-matrix (§6.5), this plan merged | `cargo check` green on all 8 targets |
| M1 | `rusty_time-core` packet + filter + selection; fuzzers; SNTP one-shot client (Linux) | syncs a test box against pool.ntp.org; fuzz corpus clean |
| M2 | clknetsim interception validated for Rust binaries (§7.1 fairness note); discipline loop + Linux clock driver; **first TIMECORP run** vs chrony | S1/S6/S8 numbers in the ledger (losing is fine — the ledger starts) |
| M3 | NTS client + server; SpaceDB persistence (drift, cookies, keys) | NTS interop vs chrony + time.cloudflare.com |
| M4 | Server mode + rate limiting + interleaved; `recvmmsg` batching; ops/`rtimec` control plane | S12 in ledger; chronyd/ntpd-rs clients sync from us |
| M5 | macOS + Windows clock drivers, service integration, packaging | per-OS smoke rigs green; installers produced in CI |
| M6 | wasm virtual clock + WebTransport gateway op; npm + disco deploy of the status page | browser demo holds < 10 ms vs gateway on S2-like network |
| M7 | HW timestamping, PHC, PPS/SHM/SOCK refclocks; HW1 lab corpus | HW1 confirms sim rankings |
| v1.0 | Gates G1–G7 green; `use-protection-please` audit; README claims ⊆ ledger | ship |

---

## 9. Open decisions

1. **Binary names** — `rtimed` / `rtimec` (short, chrony-shaped) vs `rusty_timed`. Default:
   `rtimed`/`rtimec` unless the org has a naming convention ruling.
2. **License** — chrony is GPLv2; we are clean-room from RFCs + published algorithm
   descriptions, so house default MIT/Apache-2.0 dual applies. Confirm no chrony source is
   ever pasted into this repo (corpus configs excepted — they're data).
3. **chrony.conf compatibility depth** — full directive parser vs documented subset +
   migration tool op. Default: subset + `config.migrate` op that reports what it dropped.
4. **wasm transport** — WebTransport gateway is the accuracy play; is an HTTPS-only
   fallback worth shipping in v1 or does it dilute the accuracy story? Default: ship both,
   label confidence honestly via `vclock.confidence()`.
