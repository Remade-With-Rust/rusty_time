# TIMECORP Ledger

The referee's book (mission plan §7). Every performance number this project cites —
README included — must appear here with the run that produced it. Anything not in
this file does not exist.

Rules of admission:

- Runs are appended by `timecorp run`; each block names the arm, the seed count,
  and the scenarios. Deterministic sim metrics are counts and offsets, immune to
  rig noise; the per-run split-half noise floor is printed so deltas have a bar
  to clear.
- The sim-harness arm measures rusty_time against **itself over time** (commit vs
  commit). Cross-implementation claims (vs chrony, ntpd-rs) require the Linux rig
  baselines from `.github/workflows/corpus.yml` — until those land, every block
  below says so.
- A run that regresses a gate metric is still recorded. Losing is fine; the
  ledger starting is the point.

Scenario status: S1, S6, S8 implemented in the deterministic harness;
S2 asserted for the wasm client (M6); S12 implemented as deterministic
server-load counts (M4). S3–S5, S7, S9–S11, S13–S14 pending.
**HW1 pending hardware** — it needs a GPS/PPS lab box this machine does not
have, so the M7 exit test is not met and is not claimed.

## Run 1787766275 (unix) — arm: rusty_time (sim harness v1) — 31 seeds/scenario

| scenario | conv@1ms | t→1ms (med) | t→100µs (med) | steady p50 | steady p95 | steady max | freq resid (ppm, med) |
|---|---|---|---|---|---|---|---|
| S1 | 31/31 | 231.00 s | 599.00 s | 218.8 us | 261.7 us | 262.5 us | 0.042 |
| S6 | 4/31 | 2718.00 s | n/a | 2.54 ms | 3.01 ms | 3.02 ms | 0.164 |
| S8 | 31/31 | 405.00 s | 754.00 s | 4.70 ms | 5.86 ms | 5.98 ms | 0.391 |

Baseline chrony: **PENDING** — needs the Linux rig (`.github/workflows/corpus.yml`); the sim-harness arm above measures rusty_time only and is comparable across commits, not across implementations.

## Rig validation — clknetsim interception for Rust binaries (M2 open item): CLOSED

Two-node clknetsim world (chronyd `local stratum 1` serving; rtimed one-shot
querying; node 2 configured 50 ms ahead at +20 ppm): rtimed measured offset
-0.0500 s on 6/6 exchanges and regression frequency -20.4 ppm — configured
values recovered inside fully simulated time. Reproduce:
`tools/corpus/wsl_interception_probe.sh`.

Two findings, both required:
1. clknetsim's socket() hook exact-matches the socket type; Rust std passes
   SOCK_DGRAM|SOCK_CLOEXEC and got EINVAL. Rig-only one-line mask:
   `tools/corpus/patch_clknetsim_rust_sockets.sh`.
2. clknetsim advances simulated time while a client blocks in poll/select; a
   plain blocking recv returns EWOULDBLOCK forever. rtimed now readiness-polls
   before every receive (`rusty_time_clock::net::wait_readable`) — the shape
   the M3 event loop needs regardless.

Consequence: the cross-implementation baseline arm (rusty_time vs chrony vs
ntpd-rs under identical simulated worlds) is viable. chrony's own 001-defaults
simulation test PASSes on the same rig.

## M3 gate — NTS interop: PASS (both directions)

Reproduce: `tools/corpus/nts_interop_chrony.sh` (needs chrony built with +NTS).

| arm | result |
|---|---|
| rusty_time client -> **time.cloudflare.com** | 4/4 exchanges authenticated, 0 rejected, cookies replenished 8 -> 9 |
| rusty_time client -> **chronyd** NTS server | 4/4 authenticated, 0 rejected, 9 cookies held |
| **chronyd** client -> rusty_time NTS server | chronyc authdata: `NTS 1 15 256 ... 0 NAK, 8 Cook, CLen 100`; chronyc sources: `^* localhost` — chrony SELECTED our server as its synchronisation source |
| rusty_time client -> rusty_time server (loopback) | 3/3 authenticated over real TLS 1.3 |

Direction B is the load-bearing one: chrony is a strict independent
implementation, so its acceptance means we are not merely agreeing with
ourselves. TLS is rustls with `oxitls-rustcrypto-provider` (pure Rust);
rustls's default provider compiles C and is refused by the house rule.

### Persistence (SpaceDB), verified end to end

Client resumed a saved session across three runs, skipping NTS-KE each time
(9 cookies carried); server restarted and restored its master key, and
**cookies minted by the previous server process still authenticated** — the
property master-key persistence exists to provide.

### Three defects this gate found

1. **blake3 compiles C on aarch64.** spacedb-sdk pulls blake3, whose default
   aarch64 path builds NEON assembly and needs a `cc` toolchain — a direct
   violation of the no-C-toolchain rule. All three aarch64 legs of the check
   matrix failed with "failed to find tool cc" while x86_64 passed. Fixed by
   declaring blake3 with its `pure` feature so feature unification disables the
   assembly path graph-wide.
2. **yrs 0.25.0 divide-by-zero.** `find_pivot` (block_store.rs:51) seeds an
   interpolation search with `clock / end` and does not guard `end == 0`;
   reached through spacedb-sdk's `import`, it panicked on the third
   open/flush cycle. Root-caused and avoided by not using CRDT
   `export`/`import` as the local durability format — that is what mesh sync
   is for — and persisting a per-entry record log instead, which is also a
   closer fit to the per-entry house law. `import_guarded` keeps the panic
   contained on the mesh path, where import is genuinely required.
3. **openssl `req -x509` defaults to CA:TRUE**, and rustls correctly refuses a
   CA certificate as an end-entity (`CaUsedAsEndEntity`). The rig now sets
   `basicConstraints=critical,CA:FALSE`.

No performance claim is made here; this block records correctness and interop
only. chrony BD-style performance comparison remains PENDING the Linux rig.

## M4 — server hardening, interleaved mode, control plane

### TIMECORP S12 — server load (deterministic counts, `timecorp serverload`)

Poisson arrivals driving the real admission policy. Counts, not durations: no
pinning, no noise floor, exactly reproducible per seed.

| scen | requests | answered | dropped | kissed | evicted | tracked | reply |
|---|---|---|---|---|---|---|---|
| S12a 1k clients @ 1k/s | 59635 | 14891 | 33936 | 10808 | 0 | 1000 | 43.1% |
| S12b 100k clients @ 50k/s | 2999484 | 1239999 | 1319614 | 439871 | 1021672 | 16384 | 56.0% |
| S12c 1 flooder + 1k clients @ 5k/s | 300279 | 14562 | 214683 | 71034 | 0 | 1000 | 28.5% |

Client-table state: **104 bytes/client**, capacity 16384 => **1.6 MiB** worst case.
Reply ratio is always < 100%: the server answers less than it is asked, which
is the property that stops it being a reflector. Response size is also always
<= request size (asserted in tests), so it is never an amplifier.

### M4 exit gate — chronyd syncs from rusty_time: PASS

`tools/corpus/m4_interop_chrony.sh`, both plain and interleaved:

| mode | chrony verdict | offset | peer delay |
|---|---|---|---|
| plain NTP | `^*` selected, NTP tests 111 111 1111 | -4.8 us | 116 us |
| interleaved (`xleave`) | `^*` selected, `Interleaved: Yes` | **-66 ns** | **79.6 us** |

Interleaved measures better than basic, which is the entire point of the mode:
its transmit timestamp is read after the packet is actually sent.

ntpd-rs as a second client implementation: **CLOSED at M7** — see the interop
closeout at the end of this file (ntpd-rs measured +0.056 ms against us).

### Four defects this milestone found

1. **The rate limiter was keyed on (address, port).** The source port is chosen
   by the sender, so anyone could refill their bucket with a new ephemeral
   port. Caught live: 12 requests from 3 short-lived client processes produced
   0 drops against a burst of 8, and the client table showed 3 clients for one
   host. Now keyed on address only, as chrony does.
2. **Per-client limiting is defeated by table churn.** S12b (100k addresses,
   16k table) showed the reply ratio climbing back to **100%** — every request
   arrives from a forgotten address with a fresh bucket. Fixed with a global
   token-bucket ceiling; S12b now sits at 56%.
3. **Eviction was an O(capacity) scan**, so a server facing more addresses than
   its table holds evicted on nearly every packet. S12b did not finish. Now an
   indexed true-LRU (BTreeSet), O(log n) and deterministic — a 200k-client
   churn test runs in 0.7 s.
4. **Interleaved mode had two silent field errors.** The reply must echo the
   request's *receive* field as origin (not transmit), and must report the
   *current* receive timestamp (not the previous one). Each wrong field still
   produced a well-formed, accepted packet: chrony reported a plausible
   `Interleaved: Yes` while computing an offset of **+362 ms** and a peer delay
   of **4.009 s** — exactly one poll interval, the signature of timestamps
   paired across different exchanges.

### And one defect in the gate itself

The first version of the M4 interop gate **passed** while chrony was measuring
that 362 ms error, because it only asserted `Interleaved: Yes` and `^*`. A time
server that answers in the right shape but the wrong time has failed. The gate
now asserts |offset| < 10 ms and delay < 10 ms in both modes, which is what
caught the fix working.

Also fixed: `ControlResponse::Clients(Vec<..>)` could not serialize at all —
serde's internally-tagged enums reject a newtype variant wrapping a sequence,
so the op worked in-process and returned an empty reply over the socket. Now a
struct variant, with serialization asserted in the test.

## M5 — platform drivers, service integration, packaging

### Per-OS smoke rigs (`tools/smoke/smoke.sh`)

One script, run natively on each platform. It asserts the built binaries work
end to end **without touching the system clock**, so anyone can run it.

| platform | result | measured offset | notes |
|---|---|---|---|
| Linux (x86_64, WSL2 kernel 6.6) | **PASS** | 26 us | discipline available (root); recvmmsg batching; monotonic granularity 37 ns |
| Windows 11 (x86_64) | **PASS** | 11 us | discipline correctly reported NOT available (unprivileged); monotonic granularity 100 ns; slew step 0.1 ppm |
| macOS (universal2) | **CI only** | - | this project has no Mac; the rig runs in `release.yml`, and the macOS slew arithmetic is unit-tested on every host (see below) |

The rig checks five things in order of how badly each would hurt: clock
readable and requirements stated; server binds and answers; **the measured
offset is sane** (a server that answers wrongly has failed); the control plane
responds and its counters match the traffic sent; the service definition is
emitted.

### Clock drivers

`rtimec doctor` now *probes* rather than assumes. On Windows the probe is
`PrivilegeCheck` against `SeSystemtimePrivilege` — chosen because the
alternative, attempting an adjustment to see whether it works, would move the
very clock being asked about. A test asserts the probe does not disturb the
clock.

macOS is the one platform this project cannot run locally, so its slew
arithmetic was moved out of the `#[cfg(target_os = "macos")]` module into
`rusty_time_clock::slew`, where **every `cargo test` on every host exercises
it** — including the bounded-horizon case that stops a tiny drain rate
projecting a frequency term into an absurd one-shot correction.

### Service integration and packaging

- **Linux:** systemd `Type=notify` readiness (implemented directly — the
  protocol is a datagram, not worth a C dependency) and socket activation, so
  systemd can bind port 123 as root and hand the daemon an already-open socket.
  The unit grants `CAP_SYS_TIME` and nothing else, and `Conflicts=` the other
  time daemons. A test keeps the packaged unit and `rtimed service show` in
  agreement, because two copies of one policy drift silently.
- **Windows:** MSI (WiX) registering an auto-start LocalSystem service, plus a
  standalone installer script. Neither disables `w32time` — two daemons on one
  clock fight, so that is left an explicit operator choice, and both installers
  say so.
- **macOS:** launchd plist and a `pkgbuild` component package of a `lipo`-fused
  universal2 binary.
- `release.yml` builds all of it and **runs the smoke rig against the artifact
  it just built**, on all three platforms.

### Three defects this milestone found

1. **`--control <path>` silently did nothing on Windows.** A filesystem path is
   not a bind address, so the control plane never came up; the Windows smoke
   rig failed on every control-plane check. Now one `control_endpoint()`
   resolves a path to a Unix socket on Unix and to a deterministic loopback
   port on Windows, so the same command line works on both — and the daemon
   prints what it resolved to rather than leaving it a mystery.
2. **The daemon and `rtimec` computed different default control names**
   (`\\.\pipe\rusty_time` vs `rusty_time`). Once names are hashed to ports,
   two different strings mean `rtimec` reports "is rtimed running?" about a
   daemon that is running perfectly. Both now call one
   `default_control_spec()`.
3. **Socket activation was written in the wrong crate.** Adopting fd 3 needs
   `unsafe`, which is denied workspace-wide and lifted only in the platform
   seam. It compiled on Windows and failed on Linux — caught by the check
   matrix, not by local builds.

### One transient, recorded rather than shrugged off

A single Windows smoke run failed while three subsequent runs passed. The most
likely cause is contention: the Windows and Linux rigs were started
concurrently, and WSL2 forwards loopback. The rig now prints the server's
stdout/stderr on failure and names the port-in-use case, so a repeat is
diagnosable instead of mysterious.

## M6 â€” the wasm virtual clock and the browser gateway

The capability chrony cannot follow us into: a browser has no OS clock to set
and no UDP socket to use, so the client *estimates* the offset and serves a
corrected `now()` with an error bound attached.

### M6 exit gate â€” S2-like network within 10 ms: PASS

TIMECORP S2 conditions (40 ms round trip, 2:1 path asymmetry, Pareto jitter,
40 exchanges at an 8 s cadence), asserted in
`crates/rusty_time-wasm/src/tests.rs`:

| measure | value |
|---|---|
| worst corrected-clock error | **6.854 ms** (bound: 10 ms) |
| unavoidable asymmetry bias | 6.667 ms |
| **error our estimator adds** | **~0.19 ms** |

Asymmetry is the hard part and NTP cannot observe it: a 2:1 split on a 40 ms
path is a systematic 6.67 ms bias no algorithm can remove. The gate is
therefore really asking whether the estimator adds much on top of it, and the
answer is ~0.19 ms. The test also asserts the error is *above* 1 ms, so a
simulation that silently lost its asymmetry would fail rather than look
excellent.

### Real wasm against a real gateway (not a simulation)

`tools/smoke/gateway_node_test.mjs` loads the compiled 31 KB wasm module,
builds genuine NTPv4 packets inside it, posts them to a live `rtimed` gateway
over HTTP, and feeds the replies back in â€” the browser path minus the browser.

| check | result |
|---|---|
| exchanges accepted | 6/6, 0 rejected |
| measured offset | +3.9 ms |
| reported error bound | Â±6.0 ms |
| forged reply (origin field flipped) | refused, and counted |

The offset is milliseconds rather than the native client's microseconds because
`Date.now()` has millisecond granularity â€” a browser limitation, not a defect,
and the error bound says so rather than claiming precision the environment
cannot provide.

### Design decision worth recording

The gateway speaks **real NTPv4 packets over HTTP**, not a JSON time API
invented for browsers. A browser-specific protocol would mean a second parser,
a second set of tests, and a browser path free to drift from the one everything
else uses. Instead the wasm client runs the same codec and the same regression
filter as the native client â€” both already fuzzed â€” and the gateway routes
through the same `build_reply` that serves UDP, so rate limiting, interleaved
mode and NTS apply to browser clients automatically.

### Delivered

- `@remade-with-rust/rusty-time` npm package (31 KB wasm), built by
  `tools/wasm/build-npm.sh`.
- `rtimed serve --gateway ADDR [--gateway-assets DIR]`: NTP-over-HTTP plus a
  self-contained status page that measures and displays the live offset,
  error bound and sample history.
- The smoke rig gained a wasm section; it runs where node and a wasm build are
  present and **skips explicitly** otherwise, rather than passing silently.

### Pending, stated plainly

- **WebTransport/QUIC datagrams** â€” the mission plan's preferred transport, for
  true RTT symmetry. The HTTP path shipped here is the plan's stated fallback
  (Â§6.4b). The wasm client is transport-agnostic by construction â€” JavaScript
  owns the transport â€” so WebTransport slots in without touching the time math.
- **`disco sites deploy` of the status page** â€” disco hosting is Preview and
  not reachable from this machine; the page is served by the gateway itself in
  the meantime.
- **NTS over the gateway** â€” the plumbing routes through `build_reply`, so an
  NTS-protected browser exchange needs cookie transport to the page, which is
  M7 work.


## M7 â€” reference clocks and hardware timestamping

The mission plan's M7 exit test is "HW1 confirms sim rankings", which needs the
GPS/PPS lab box. **This machine does not have one, so HW1 is PENDING and is not
claimed.** What follows is what could be verified here, and what could not,
separated deliberately.

### What this machine can actually reach

`tools/corpus/probe_hw_capabilities.sh` reports the honest inventory:

| capability | here | evidence |
|---|---|---|
| PTP hardware clock | `/dev/ptp0` (Hyper-V) | **real** |
| Kernel software RX timestamps | eth0 software-receive | **real** |
| gpsd SHM refclock | userspace, no device needed | **real** |
| chrony SOCK refclock | userspace, no device needed | **real** |
| PPS (`/dev/pps*`) | absent | PENDING hardware |
| NIC hardware timestamping | eth0 reports none | PENDING hardware |

### Verified (`tools/corpus/refclock_probe.sh`)

| transport | result |
|---|---|
| **PHC** `/dev/ptp0` | read and validated; offset âˆ’3.6 Âµs to +12.6 Âµs vs the system clock across runs, dispersion ~4 Âµs |
| **SOCK** (chrony protocol) | 3/3 synthetic-producer samples received; offset decoded as **exactly** +0.000123000 s |
| **SHM** (gpsd protocol) | segment read; offset **exactly** the +0.001499891 s the producer published |
| **Kernel RX timestamps** | `SO_TIMESTAMPING` enabled, stamp returned through `recvmmsg` control data and asserted to fall inside the observed send/receive window |

The SHM reader implements the protocol's count-guard: a producer writes the
struct without a lock, so the reader checks `count` before and after and retries
on a mismatch. A sample torn across that write would be a plausible-looking
wrong time.

### A precision defect found by demanding exactness

The first SOCK round-trip returned **âˆ’0.0014998913 s** for an offset sent as
âˆ’0.0015 â€” an error of ~110 ns. Cause: `RefclockSample` stored a *reference
timestamp* and recovered the offset by subtracting the local time. An f64
holding a Unix-epoch value (~1.76e9) has only ~1e-7 s of resolution left below
the decimal point, so `local + offset âˆ’ local` does not return `offset`.

That silently caps **every** reference clock at roughly 100 ns however good the
hardware is â€” irrelevant for a network source, fatal for the PPS and PHC
sources this milestone exists to support. The sample now stores the offset as
the authoritative field, and SHM differences the integer seconds and
nanoseconds separately rather than converting both times to f64 first. The
test asserts equality, not closeness; "close enough" is what hid it.

### Correctness closeout (G7)

| gate | result |
|---|---|
| **Fuzzing** (`tools/corpus/run_fuzzers.sh`) | **PASS** â€” 45 s per target, **~30.0 M executions total**, zero crashes: `ntp_parse` 27,133,553 (590 k/s), `nts_records` 2,382,463, `config_parse` 446,799 |
| Live NTS interop, both directions | PASS (M3) â€” chronyd and time.cloudflare.com |
| chronyd syncs from us, plain + interleaved | PASS (M4) â€” `^*` selected, offsets âˆ’4.8 Âµs and âˆ’66 ns |
| wasm client vs live gateway | PASS (M6) |
| Per-OS smoke rigs | PASS (M5) â€” Linux and Windows native, macOS in CI |
| 8-target check matrix | green |
| Workspace tests | 165 passing, 0 failing |

### Still PENDING, stated rather than implied

- **HW1 lab corpus** â€” needs GPS + PPS ground truth. The M7 exit test is not met.
- **PPS refclock** â€” implemented against RFC 2783, never executed: no `/dev/pps*`.
- **NIC hardware timestamping** â€” the request path is wired and the kernel is
  asked for hardware stamps, but this NIC reports none, so only the software
  path has ever produced a timestamp here.
- **`PTP_SYS_OFFSET_PRECISE`** â€” the PHC sampler uses the portable
  read-system/read-PHC/read-system midpoint instead, which works everywhere;
  the precise ioctl is a lab-box improvement.


## Interop closeout — ntpd-rs (the M4 pending item): PASS

`tools/corpus/ntpd_rs_interop.sh`. ntpd-rs is the other memory-safe NTP daemon
and an entirely separate codebase, so its agreement is independent evidence
that we implement the protocol rather than a self-consistent dialect.

| measure | value |
|---|---|
| ntpd-rs verdict | source accepted, Kalman filter converging |
| offset it measured against us | **+0.056 ms** |
| what rtimed logged | 2 requests, 2 responses, 0 dropped, 0 refused |

Three independent implementations now synchronise from a rusty_time server:
**chronyd** (plain, NTS, and interleaved), **ntpd-rs**, and our own wasm client
over the HTTP gateway. In the other direction we authenticate against chronyd
and time.cloudflare.com.

With this, every correctness gate that does not require hardware or a
performance baseline is green.


## Cross-implementation performance: rusty_time vs chrony (G1-G6 arm)

`tools/corpus/bench_vs_chrony.sh`. The comparison the mission plan has owed
since M2, and the first time this project has published a number against
chrony rather than against itself.

### The rig

**clknetsim** — chrony's own deterministic clock-and-network simulator, so the
baseline is measured on the tooling its authors trust. Both arms run in the
*same simulated world*: one config file per scenario, used unchanged by each
arm, and node 1 is a chronyd stratum-1 server in both cases so the server is
never a variable. chrony is configured, not strawmanned: `iburst`, and the
same `minpoll 4 / maxpoll 6 / makestep 1.0 3` policy given to rtimed.

Error is read from clknetsim's own offset log, which records each node's
**true** error once per second. Neither implementation's self-report is
trusted — that distinction matters here, because the first defect below was
one where rtimed's self-report and the truth disagreed by 8 ms.

11 repetitions per arm per scenario, 1800 s simulated each. clknetsim redraws
its random streams per run, so a single run is one sample of a distribution;
the table reports the **median run** for typical behaviour and the **worst run**
for the tail.

### Steady-state accuracy (last quarter of each run)

| scenario | arm | p50 (median run) | p95 (median run) | max (worst run) |
|---|---|---|---|---|
| S1 LAN symmetric | chrony | 1.4 us | 1.8 us | 2.5 us |
| S1 | **rusty_time** | **1.1 us** | **1.6 us** | 3.4 us |
| S6 cold start 500 ms | chrony | 1.2 us | 1.6 us | 4.3 us |
| S6 | rusty_time | 2.4 us | 3.4 us | 4.8 us |
| S8 drifty +100 ppm | chrony | 5.5 us | 8.0 us | 20.4 us |
| S8 | rusty_time | 6.2 us | 8.4 us | 23.3 us |
| S2 WAN 2:1 asymmetric | chrony | 6.630 ms | 6.813 ms | 7.028 ms |
| S2 | rusty_time | 7.153 ms | 7.310 ms | 8.191 ms |
| S4 congested, 10% loss | chrony | 2.024 ms | 2.742 ms | 8.005 ms |
| S4 | **rusty_time** | **1.902 ms** | 2.892 ms | **7.043 ms** |

S2 is a check on the rig as much as on the implementations: a 40 ms path split
26.7/13.3 ms puts an irreducible 6.7 ms error in any NTP client, because NTP
cannot observe path asymmetry. Both land there. A result meaningfully below
6.7 ms would have meant the harness was measuring something other than true
error.

### Convergence (median across runs)

| scenario | chrony to <1 ms | rusty_time to <1 ms | chrony to <100 us | rusty_time to <100 us |
|---|---|---|---|---|
| S1 | 7 s | 41 s | 7 s | 132 s |
| S6 | 12 s | 190 s | 12 s | 281 s |
| S8 | 7 s | 39 s | 7 s | 130 s |

### Gate verdicts, stated plainly

| gate | verdict |
|---|---|
| **G1 accuracy** | **UNRESOLVED at microsecond scale; not met overall.** See the second run below — the us-scale scenarios are not separated by this rig, and the earlier per-scenario win/loss readings did not survive it. What holds: 5 of 14 scenarios have an arm at all, and **S6 is the one real signal**, sitting above the 1.25x cap in both runs. |
| **G2 convergence** | **FAIL.** S6 time-to-1 ms is 190 s against chrony's 12 s. Not close, and not a measurement artefact — see the cause below. |
| G3 holdover, G4 server throughput, G5 footprint, G6 vs ntpd-rs | **NOT MEASURED.** No number is claimed. |

### Why convergence lags, since the number alone does not say

The offset drain removes `offset / (corrtimeratio * poll)` per plan, with
`corrtimeratio = 3`, and the loop re-plans every poll. Each poll therefore
removes about a third of what remains, so the offset decays as `(2/3)^n` —
about 5.7 polls to fall by 10x. After the initial burst the poll jumps
straight to `minpoll` (16 s), which sets the pace from then on. chrony
finishes most of the correction inside the burst instead.

This is a tuning decision in the drain policy, not a defect: the loop is
stable, converges monotonically, and its steady-state accuracy is at parity.
It is deliberately **not** being retuned as part of this benchmark. Changing
`corrtimeratio` moves every S1-S14 number in this ledger, and retuning a
constant at the end of a benchmark run in order to turn a gate green is how a
corpus stops being evidence. It is the next piece of work, with its own
validation pass.

### Two real defects this comparison found

Both were invisible to the in-house simulator, because the simulator modelled
an idealised driver. This is the argument for the cross-implementation arm
existing at all.

**1. The Linux driver silently delivered a fraction of the commanded slew.**
`ADJ_FREQUENCY` is clamped by the kernel at **500 ppm** (`MAXFREQ` in
`kernel/time/ntp.c`); the driver advertised 32767 ppm, reading the limit off
the width of the scaled field rather than the kernel's behaviour. Asked for
1716 ppm, the clock moved at 500. The controller then subtracted a drain that
had not happened from its sample history, the regression read the shortfall as
a frequency error, wound up to the 500 ppm frequency clamp, and **overshot a
10 ms start to -8.8 ms**. Fixed by splitting the correction across `ADJ_TICK`
(coarse, +/-10% of nominal) and `ADJ_FREQUENCY` (fine), which is what chrony
does and the only way to get a usable slew range on Linux; `capabilities()`
now advertises a rate the driver can actually deliver, and `rtimed sync`
configures the discipline from it so the loop cannot command the impossible.
Guarded by `linux_tick_and_freq` tests asserting commanded == delivered.

**2. A cold start wasted 16 seconds before its first usable sample.**
A server that has just started answers with the unsynchronised leap indicator,
which a client must refuse. On that refusal the retry backed off to the full
poll interval (16 s) instead of the burst spacing (2 s). First usable sample
moved from t=15.995 s to t=1.995 s. Guarded by two tests in
`rusty_time-core::client`: retries are fast while burst budget remains, and
back off to the poll interval once it is spent.

Neither fix changed the S1/S6/S8 simulator numbers recorded above — verified by
re-running `timecorp run --seeds 31` against a freshly built binary and getting
byte-identical output.

| measure | value |
|---|---|
| Workspace tests | **179 passing, 0 failing** |
| clippy `-D warnings` | clean |
| Scenarios with a cross-implementation arm | 5 of 14 |


## Server hot path: 3053.6 -> 352.1 instructions per request (-88.47%)

`tools/perf/ir.sh`, `crates/rusty_time-core/benches/hot_path.rs`.

### The instrument, and why it is a counter

Every win below is individually well under 1% of any wall clock. At that size
the clock cannot be promoted to the verdict however many pairs are run — the
box's own drift is larger than the effect, so a timing A/B either discards real
work-removal or banks a regression. So the primary evidence is a **deterministic
instruction count**: callgrind Ir, exact and attributable per function.

The harness is a fixed 200k-request workload with a fixed client population, an
LCG picking clients from a fixed seed, and a simulated monotonic clock. The
client table's hash seed is pinned via `RUSTY_TIME_HASH_SEED` so probe counts do
not move between runs. **Verified exactly reproducible: three consecutive runs
each read 70,428,459 Ir.** (Unpinned, the OS-random seed makes it reproducible
only to ~0.002% — still four orders of magnitude below the effect, but an exact
instrument is worth more than a nearly-exact one.)

The correctness gate is byte-identity. This is an integer/exact path, so the
harness folds every byte of every reply into a checksum and the harness refuses
to report a win if it moves. **It did not move across any of the ten changes:
`0x7546d2258584b400` throughout.**

### Where the instructions actually were

Profiled before touching anything. The result overturned the assumption that an
NTP server spends its time on NTP:

| cost | share of per-request instructions |
|---|---|
| SipHash hashing the client address | ~38% |
| `BTreeSet` recency index (`search_tree`) | ~23% |
| hashbrown `get_mut` | ~6% |
| **client-table bookkeeping, total** | **~67%** |

The cause was structural: the table was hashed **seven times per request** —
three in `admit` (`contains_key`, `touch`'s `get_mut`, then a final `get_mut`),
two in `response_mode`, one each in `note_response` and `note_transmit` — plus
two O(log n) tree walks and two key clones to maintain an eviction index that is
only read when the table is full.

### The ten wins

Measured cumulatively; each was landed and gated separately.

| # | change | where |
|---|---|---|
| 1 | Precompute the token refill rate — a `powi` per request for a value derived only from config | core |
| 2 | `response_mode`: `get` + `get_mut` on the same key -> one `get_mut` | core |
| 3 | `admit`: three lookups -> one | core |
| 4 | `BTreeSet<(seq, K)>` recency index -> intrusive O(1) doubly-linked list over stable slots; removes 2 tree walks + 2 key clones per request | core |
| 5 | `ClientHandle`: hash once in `admit`, address the three follow-ups by index — four hashes per request -> one | core |
| 6 | Seeded multiply-mix hasher replacing SipHash for the address key | core |
| 7 | Four mutex acquisitions per request -> two (`admit`/`response_mode`/`stratum`/`note_response` folded under one guard) | daemon |
| 8 | Plain NTP replies no longer allocate: `ReplyBytes::Plain([u8; 48])` instead of `to_vec()` on a fixed-size array | daemon |
| 9 | `kiss_of_death` returns `[u8; 48]` instead of `Vec<u8>` | daemon |
| 10 | `client_key` computed once instead of twice; `RUSTY_TIME_DEBUG_XLEAVE` read once per process instead of per interleaved request | daemon |

Cumulative: **610,723,644 -> 70,428,459 Ir** over 200k requests, i.e.
**3053.6 -> 352.1 instructions per request, -88.47%**, checksum unchanged.

Wins 7-10 are daemon-side and outside the core harness, so they are recorded as
the deterministic counts they are — locks per request, allocations per reply,
calls per request — verified in the code rather than inferred.

### On the security of win 6

SipHash-1-3 is std's default because it resists collision floods, and that
property is **not** optional when the key is a client-chosen source address.
The replacement keeps it and drops the price: the seed is drawn from the OS once
per process, exactly as SipHash's keys are, so an attacker cannot compute a
colliding set without a secret they never see. What is given up is SipHash's
proof against an adversary who already knows the seed. What remains alongside is
a table bounded to a fixed capacity with LRU eviction, so no chain can grow
without bound even if that ever failed.

### One candidate investigated and rejected

Guarding the extension-field walk on `request.len() > HEADER_LEN`, on the theory
that plain 48-byte requests should not pay for it. **Rejected: `ef::fields`
already clamps its start to the packet length and yields nothing for a bare
header**, so the guard bought a check that already happens and added a branch.
Reverted rather than counted.

### One instrument corrected

The corpus previously reported **104 bytes/client, 1.6 MiB worst case**. That
figure was `size_of::<ClientRecord>()` alone and omitted the `BTreeSet` index
entirely, so it under-counted the old structure too. It now asks the type
(`ClientTable::bytes_per_client()`), which counts the record, its slot links and
the index entry: **150 bytes/client, 2.3 MiB at capacity 16384**. The two
numbers are not comparable and **no memory win is claimed** — the new one is
simply the first complete measurement.

### Gates

| gate | result |
|---|---|
| Reply byte-identity (harness checksum) | unchanged through all ten changes |
| S12a / S12b / S12c server-load counts | **identical to the recorded ledger rows**, including 1,021,672 evictions in S12b — the new LRU behaves identically through a million evictions |
| S1 / S6 / S8 discipline scenarios | unchanged |
| Workspace tests | 179 passing, 0 failing, **test files unchanged** |
| clippy `-D warnings`, `cargo fmt` | clean |
| 8-target check matrix | green |

### What is NOT claimed

An 88% instruction reduction is not an 88% throughput gain. Instructions are not
time: what was removed (SipHash rounds, B-tree pointer chases) has different IPC
and cache behaviour from what remains, and a real server additionally spends the
syscalls this harness excludes. The single-arm absolute figure, pinned and
best-of-3 CPU time over 20M requests, is **~30M requests/s of core policy +
codec work on one core** — G4 (throughput vs chrony on the same rig) remains
**unmeasured**, and this does not settle it.


## Round two: the paths the first round never instrumented

The first round fixed the plain-NTP client table and stopped there, because
that was the only path with an instrument. Three more were built. Each is a
deterministic Ir workload with a byte-identity gate, and each found work worth
removing.

| harness | measures | before | after |
|---|---|---|---|
| `rusty_time-clock/benches/recv_setup.rs` | `recv_batch` per-call setup | 10,892.5 Ir/call | **1,264.5** (−88.4%) |
| `rusty_time-nts/benches/nts_reply.rs` | the server's NTS reply | 30,275 Ir/reply | **19,240** (−36.4%) |
| `rusty_time-core/benches/mru_report.rs` | the MRU status report | 2,236,139 Ir/report | **2,959** (−99.87%, 756x) |

### The ten changes

Six carry an exact Ir A/B; four are deterministic counts of work removed,
verified in the code the way the first round's daemon changes were.

| # | change | evidence |
|---|---|---|
| 1 | `recv_batch` takes a caller-owned `BatchScratch` instead of building four arrays per call — two of them `vec![zeroed; 32]`, ~8 KiB of memset, to hand the kernel space it immediately overwrites | **measured** 10,892.5 -> 1,264.5 Ir/call |
| 2 | NTS reply and cookie-plaintext buffers pre-sized instead of grown from empty | **measured** 30,275 -> 28,804 |
| 3 | `ef::write_authenticator` appends in place; the old form built the body in one `Vec`, copied it into a second to add the header, and left the caller to copy that into the reply | **measured** 28,804 -> 28,377 |
| 4 | `cookie::mint_fields_into` — **one** AES key schedule for all of a reply's cookies instead of one each, and no per-cookie allocation | **measured** 28,377 -> 19,654 (−30.7%) |
| 5 | One `getrandom` draw for every cookie nonce, not one per cookie | **counted** up to 8 syscalls per reply -> 1 |
| 6 | `Sealer::seal_in_place` — one scratch buffer for all the cookies rather than a fresh `Vec` per cookie, copied out and dropped | **measured** 19,654 -> 19,240 |
| 7 | `most_recent` walks the recency list, which is already in that order, instead of cloning all 16384 records and sorting them to keep ten | **measured** 2,236,139 -> 2,959 |
| 8 | `ef::write_zero_field` for cookie placeholders — the client allocated a zero `Vec` to copy zeros out of (also ships in wasm) | **counted** 1 allocation per protected request -> 0 |
| 9 | The NTS server path walks the request's extension fields **once**; it used to walk them again inside the authenticator verifier to re-find a field it had just seen | **counted** 2 walks per NTS request -> 1 |
| 10 | The gateway reuses one `String` across header lines instead of allocating a fresh one per line | **counted** ~1 allocation per header -> 1 per request |

Win 4 is the one that mattered, and it was not obvious from reading: the cost
was not the cryptography but the *setup* for it. A reply mints up to eight
cookies under one master key and was expanding a fresh AES key schedule for
each.

### The gate

Every arm's checksum is unchanged: `0xc0452ab605328800` (plain), and
`0xef68436181093b01` across all four NTS changes. The `mru_report` arms agree
exactly (`0x9dc2477057bf9530`), which is the proof that the recency list and a
sort by `last_seen` are the same ordering.

The decisive one is not a checksum. These changes rewrite how cookies are
minted and how authenticators are written, so **live NTS interop against
chrony was re-run and passes in both directions**: rusty_time authenticates
against chronyd's NTS server (4/4 exchanges, offset −32 us), and chronyd
completes NTS against ours, holding 8 cookies of length 100 and selecting us
`^*` at +628 ns. A foreign implementation redeeming our cookies is a stronger
statement about the wire format than any local assertion.

S1/S6/S8 unchanged; S12a/b/c counts identical; 179 tests, clippy clean, 8
targets green.

### Three notes against self-flattery

**The measuring tap was 20% of the measurement.** The plain-path harness folded
each reply into a checksum byte by byte — 48 iterations per request, which
callgrind priced at 68 Ir/request, about a fifth of everything the workload
did once the client table was fixed. Every share derived from that harness was
distorted by it. The fold is now word-wise, coverage unchanged. The plain-path
baseline is restated at **237.1 Ir/request** with the honest tap.

**One change measured exactly zero and was reverted.** `admit_handle` indexed
the same slot twice, once for the generation and once for the record; merging
them changed nothing, because LLVM had already merged them. Kept out of the
count, and the code was put back rather than left carrying a comment claiming
a saving that does not exist. It also refutes the obvious next move: the
`slice/index.rs` share is not redundant indexing waiting to be restructured.

**A harness silently measured nothing.** The first `recv_setup` run reported
38.8 Ir/call for *both* arms — a flat toggle that would have read as "reusing
the scratch does not help". It was missing `harness = false`, so cargo linked
libtest's `main` and the binary ran zero tests while exiting cleanly. A flat
arm-toggle is not evidence until the arm is known to be wired; the harness now
prints its own work count (`calls 20000`) so this cannot recur silently.


## G4 server throughput: MET

`tools/corpus/bench_server_vs_chrony.sh` (new), `tools/perf/server_ir.sh` (new).
G4 had no arm at all before this; it now has two, and passes.

### Against chrony, same rig, same generator

200000 requests per round, concurrency 64, 6 rounds, server and generator
pinned to different cores, arm order rotated.

| arm | answered | replies/s | cpu us/reply |
|---|---|---|---|
| **rusty_time** | 200000 | **138664** | **2.950** |
| chrony | 200000 | 105668 | 4.100 |
| chrony_null | 200000 | 105960 | 4.050 |

Work parity exact. **replies/s: null-arm floor 292, arm gap 32996 — RESOLVED,
rusty_time ahead** (113x the floor). **cpu_us/reply: floor 0.05, gap 1.15 —
RESOLVED** (23x the floor). That is **1.31x chrony's throughput at 0.72x its
CPU per reply**.

p99 is **not** claimed: at concurrency 64 against a saturated server it is
queueing latency in the generator, and all three arms sit within 3% of each
other. G4's p99 half needs a sub-saturation rate arm that does not exist yet.

### The deterministic instrument

Wall throughput could not resolve a single one of the changes below — the
control arm's spread across rounds exceeded its own median. `server_ir.sh` runs
the server under callgrind and counts instructions retired per reply:
reproducible to 0.02% (878.4 / 878.5 / 878.6 across runs), and immune to
whatever else the box is doing.

| # | change | Ir/reply |
|---|---|---|
| — | before | **962.0** |
| 1 | `sendmmsg`: one send syscall per batch instead of one per reply | 878.5 (-8.7%) |
| 2 | `ClockRead::wall_parts` — the OS already has (secs, nanos); composing them into an `i128` and dividing them back apart cost a *software* 128-bit division, 5% of the server | 799.3 (-9.0%) |
| 3 | `send_batch_by` reads the caller's replies directly, dropping a per-batch temporary vector | 784.0 (-1.9%) |
| 4 | One state lock for the whole batch instead of one per reply (up to 32 -> 1) | 741.0 (-5.5%) |
| 5 | No control buffers on the server socket: it never enables or reads kernel receive timestamps, so it was asking the kernel to prepare data for nobody | 724.7 (-2.2%) |
| — | **after** | **725.2 (-24.6%)** |

Interleaved mode is not worse for batching, it is better: the packets now
genuinely all leave in one syscall, so the single timestamp taken immediately
after describes all of them, where the old loop stamped each reply after its
own `sendto` and charged the last client in a batch the accumulated cost of
every send before it.


## G2 convergence: substantially improved, still short on S6

One change, gated three ways. The offset drain aims to finish in
`CORR_TIME_RATIO` poll intervals but the loop re-plans every poll, so only a
third of it ever runs: the offset decays by a third per poll, a time constant
three times longer than the ratio suggests. Correct in steady state — it is
what keeps sample noise out of the clock — and wrong during acquisition.

| scenario (clknetsim) | before | after | chrony |
|---|---|---|---|
| S1 to <1 ms | 41 s | **5 s** | 7 s |
| S6 to <1 ms | 190 s | **28 s** | 12 s |
| S8 to <1 ms | 39 s | **5 s** | 7 s |

S1 and S8 now beat chrony. **S6 is 6.8x better but still 2.3x chrony's 12 s, so
G2 is not met.** The in-house corpus improves too: S1 231 s -> 89 s, S8 405 s ->
127 s, steady-state unchanged on both.

### Why it is gated, and what the gates cost

The first version keyed only on "the offset is far outside the noise". On
clknetsim it was a clean win everywhere, S6 included at 12 s — G2 met. On the
in-house corpus, whose S6 models a path with 0.74 ms of jitter, it took S6's
steady error from **2.54 ms to 10.83 ms**. Two rigs, opposite verdicts, and the
low-noise one showed only the good news.

Raising the noise threshold did nothing: S6 sat at 10.83 ms at 10x, 30x, 100x
and 300x, because the estimator there reports a *small* `offset_sd` beside a
large error. A loop that is confidently wrong cannot be filtered out by asking
it how confident it is.

What worked was bounding the correction by what the clock can absorb: the fast
path applies only while acquiring (first 8 updates), and only when the rate it
needs is at most a quarter of the slew ceiling. A correction that consumes the
whole budget pins the clock at maximum rate for the interval, and the frequency
estimator then infers drift from samples taken while the clock was being
hauled — badly enough to leave a permanently worse steady state.

| variant | clknetsim S6 | in-house S6 steady |
|---|---|---|
| ungated | 12 s (G2 met) | 2.54 -> **10.83 ms** |
| **gated (shipped)** | 28 s (G2 not met) | 2.54 -> **2.93 ms** |

The gated version ships. Turning a convergence gate green by accepting a 3.4x
steady-state regression is the same move this ledger refused earlier when it
declined to retune `corrtimeratio` to make G2 pass — and it would have been
invisible to the rig the gate is scored on.

## Why S6 was twice chrony's convergence, and the gate that fixed it

The 28 s was not mysterious once the trajectory was read next to chrony's. It
was one step.

```
t= 1.995  offset -0.500  freq -83333   poll=2     <- burst, at the slew ceiling
t= 4.177  offset -0.318  freq -53042   poll=2
t= 6.290  offset -0.206  freq -34378   poll=2
t= 8.361  offset -0.135  freq -67545   poll=2
t=10.506  offset +0.0098 freq   +609   poll=16    <- burst over, 9.8 ms left
t=26.497  offset +0.0000                          <- 16 s spent on 9.8 ms
```

The offset drain is sized to finish **within one poll interval**. The
acquisition burst ended after a fixed four samples, and the poll then jumped
straight from 2 s to `min_poll`. So the burst hauled 500 ms down to 9.8 ms in
ten seconds — and then handed the last 9.8 ms a **16 second deadline instead of
a 2 second one**. chrony, which is not bound by its poll interval, had the same
cold start gone by t=12 s.

Everything else was a distraction: the frequency estimate was not the problem
(suppressing frequency updates across the haul changed the outcome by nothing
at all), and the slew ceiling was not the problem (the burst was already at it).

### The gate

**End the acquisition burst when the offset reaches the convergence target, not
when a counter runs out.** Capped at `MAX_ACQUIRE_BURST` polls, because a
client that cannot converge must back off rather than keep asking a stranger's
server every two seconds.

A first version keyed the extension on `offset > 10 x noise` and failed at
precisely the wrong moment: at 9.8 ms remaining, `10 x noise` evaluated to
about 10 ms, the test went false, and the poll jumped anyway. Tying it to what
"converged" actually means — 1 ms, floored at twice the noise — is what worked.

| clknetsim, to <1 ms | before | after | chrony |
|---|---|---|---|
| S1 | 41 s | **5 s** | 7 s |
| S6 | 190 s -> 28 s | **18 s** | 12 s |
| S8 | 39 s | **5 s** | 7 s |

S6 is now **1.5x chrony, down from 2.3x**; S1 and S8 beat it. S1 accuracy is
also now RESOLVED in our favour (1.0 us vs 1.8 us, null-arm floor 0.27 us).

### And the regression it retired

The earlier acquisition change cost the in-house corpus's S6 steady error
(2.54 -> 2.93 ms) and that was recorded as the price of the convergence win.
With the burst gate in place the price is gone — the noisy rig improves on
**every** metric:

| in-house corpus | baseline | now |
|---|---|---|
| S1 conv / steady | 231 s / 218.8 us | **73 s / 199.7 us** |
| S6 steady (seeds converged) | 2.54 ms (4/31) | **1.51 ms (10/31)** |
| S8 conv / steady | 405 s / 4.70 ms | **74 s / 4.43 ms** |

That is the tell that this gate addresses the cause rather than trading one
scenario against another: the two rigs, which disagreed sharply on the previous
attempt, now agree.

### What still separates us from chrony on S6

`ClockCommand::Slew` carries a `drain_offset` — a budget — and **no driver
honours it**; every platform folds the drain into a constant frequency that
runs until the next plan. That is why the correction time has to equal the poll
interval at all: a rate that finishes early would keep going and overshoot.
chrony's driver accumulates an offset and stops when it is gone, which is how
it slews near its ceiling without being bound to its polling schedule.

Honouring that budget in the drivers (Linux `ADJ_OFFSET` is exactly this
primitive) would remove the remaining 1.5x. It is an architectural change to
the clock seam, not a tuning constant, and it is the next thing to do for G2.

Guarded by three tests in `discipline::acquisition_tests`: the burst continues
while a correction is outstanding, ends once the offset is small, and is
bounded when convergence never arrives.

## The architectural fix: drains carry a budget

`ClockCommand::Slew` has always had three fields — a frequency, a
`drain_offset`, and a `drain_rate_ppm`. The middle one says how big the
correction is. **No driver honoured it.** Every platform folded the drain into
a constant frequency that ran until the next plan replaced it, so a drain was
not a correction of known size, it was a rate with no end.

That is why the correction time had to equal the poll interval: a rate that
would finish early does not stop when the offset is gone, it sails past it. So
"how fast may the clock move" and "when does the next packet arrive" were the
same question, and a 500 ms cold start's last 9.8 ms was handed a 16 s deadline
because that is when the next packet happened to be due.

### What was built

* `SyncController` tracks `drain_remaining_s` — the budget — and exposes
  `drain_completes_at()` and `poll_drain()`.
* The daemon **wakes for the end of a drain** rather than only for the next
  poll, and applies the frequency-only command that retires it.
* The simulator stops its plant integration at the same event, so the two agree
  — the property that makes a corpus number mean anything.

### The bug it produced, which is the bug it exists to prevent

First implementation booked **the budget** when a drain retired. But the budget
says when the drain *should* stop; the driver stops when it is told to, which
is when the caller next looks. Waking 11 ms after a 19577 ppm drain expired
delivered **215 us** the loop never recorded. One unbooked correction, once, in
a fifteen-minute run, left a permanent ~180 us bias: **S6 measured 137 us
steady against chrony's 2.5 us**, and the frequency estimate settled 1 ppm off
true because the regression read the missing correction as drift.

This is the same failure as a driver silently clamping a slew (M2, the Linux
`ADJ_FREQUENCY` 500 ppm ceiling): **the loop's arithmetic must describe what the
clock did, not what it was asked to do.** Fixed by booking the delivered
correction, and pinned by two tests in `client::tests`.

### What it bought, measured honestly

**No measured speed.** At the shipped configuration the budget is inert: the
same binary with drain retirement disabled (`RUSTY_TIME_NO_DRAIN_STOP=1`)
measures the same — S1 5 s, S6 14 s, S8 5 s either way. The discipline never
asks for a rate that would finish before the next poll, so no drain ever
retires early enough to matter.

Three attempts to use it were tried and all measured worse on at least one rig:

| attempt | clknetsim S6 | in-house S6 steady |
|---|---|---|
| clear in a fixed 2 s, ceiling-capped | 11 s | 9.27 ms |
| slew at the ceiling throughout | 107 s | 4.93 ms |
| fixed 2 s, gated on confidence | 16 s | 1.50 ms |
| **poll-scaled, confidence-capped (shipped)** | **14 s** | **1.50 ms** |

It is kept as correctness infrastructure, not as a speed win: `drain_offset`
now means what it says, an over-fast rate can no longer overshoot, and a late
wake-up is accounted rather than lost. Anyone raising the rate in future needs
all three, and none of them existed before.

## What actually moved S6: confidence

The measured improvement came from a different gate. How fast the clock may be
hauled should depend on how well the offset is known — and the two rigs differ
by two orders of magnitude in exactly that quantity. A 500 ms offset on
clknetsim's 10 us-jitter path is known to five decimal places; the same 500 ms
on the in-house S6's 0.74 ms-jitter path is a much rougher number, and
committing to it at full speed writes the roughness into the clock.

So the acquisition rate keeps scaling with the poll interval, and only its
*ceiling* depends on confidence: the full slew budget once the offset exceeds
the noise by 10000x, a quarter of it otherwise. No single constant satisfied
both rigs; this satisfies both.

| clknetsim, to <1 ms | start of session | now | chrony |
|---|---|---|---|
| S1 | 41 s | **5 s** | 7 s |
| S6 | 190 s | **14 s** | 12 s |
| S8 | 39 s | **5 s** | 7 s |

| in-house corpus | baseline | now |
|---|---|---|
| S1 conv / steady | 231 s / 218.8 us | **73 s / 199.7 us** |
| S6 steady | 2.54 ms | **1.50 ms** |
| S8 conv / steady | 405 s / 4.70 ms | **74 s / 4.43 ms** |

**S6 is 14-16 s against chrony's 12 s, from 2.3x at the start of this work and
15.8x before any of it.** G2's bar is "<= chrony", so it is still not met.
Convergence is highly reproducible -- chrony 7/12/7 s with zero spread across
nine rounds, rusty_time 5/14-16/5 s -- so those numbers are trustworthy.

**The accuracy claims are not, and are withdrawn.** See below.

Gates: 184 tests, clippy clean, 8 targets green, every in-house scenario better
than baseline on every metric.

## Correction: the accuracy verdicts were over-confident

The resolution verdict compared the two chrony arms' **medians** and treated
that difference as the rig's floor. That is a single draw from a distribution,
and it routinely came out absurdly small -- 0.04 us on S6, beside within-arm
spreads of 3 us. The verdict was therefore over-confident, and it flipped: S8
read **RESOLVED, rusty_time ahead** (2.3 us vs 5.0 us) in one run and
**RESOLVED, chrony ahead** (10.7 us vs 2.0 us) in the next, on identical code,
because that scenario's p50 ranges 0.7-25.3 us across reps.

Replaced with the paired sign test the measurement discipline actually asks
for. Every arm runs in the same round against the same box, so rounds are the
pairing and per-round comparison cancels the drift that defeats medians:
`z = (wins - n/2) / (0.5*sqrt(n))`, resolved at |z| > 2.

At N = 9:

| scenario | paired | verdict |
|---|---|---|
| S1 | 3/9, z = -1.00 | NOT RESOLVED |
| S6 | 1/9, z = **-2.33** | **RESOLVED, chrony ahead** |
| S8 | 6/9, z = +1.00 | NOT RESOLVED |

**Withdrawn:** every per-scenario accuracy claim in either direction on S1 and
S8, including "S8 RESOLVED, rusty_time ahead" recorded above. The one accuracy
result that survives a proper test is S6, and chrony wins it.

What still stands, because it was measured on instruments that resolve:
convergence (reproducible to the second), the in-house corpus scenarios
(deterministic, 31 seeds), the server Ir counts (exact), and the G4 throughput
comparison (null-arm floor 292 replies/s against a 33000 gap).

The rig now also prints each arm's **convergence** spread, not just its
accuracy spread. This session moved S6 through 28/18/14/16 s on medians whose
reproducibility was never shown; the spread column exists so that cannot happen
silently again.

## A bug the benchmark could not see: multi-source drain retirement

Retiring a spent drain applied that source's frequency to the clock **for every
source**, while the sample path correctly applies only the selected source's
plan. With more than one server configured, a source the selector had rejected
— a falseticker, or simply the worse of two — would impose its frequency the
moment its drain happened to expire.

Every scenario in this corpus uses a single source, so no measurement here
could ever have caught it; it was found by reading the two paths side by side
and asking why they disagreed. Fixed: drains are still retired in each
controller's own books, but only the selected source's command reaches the
clock.

**Still unaudited in multi-source mode:** a non-selected source's controller
continues to assume its plans drove the clock, so its `slew_samples` bookkeeping
is wrong for as long as it is not selected. That predates this change and is
not fixed here.

## The rig, and what it was hiding

The client comparison now runs a **null arm** — a second chrony against the
first — prints each arm's p50 spread across reps, and states whether a gap is
resolvable. It exists because the earlier "rusty_time beats chrony on S1"
reading did not survive a second run: chrony's own unchanged S1 p50 moved
1.4 -> 0.4 us between runs, a swing larger than the gap being reported. The
rig now says so itself:

```
S1 rusty_time  ... -> null-arm floor 1.06 us | arm gap 1.62 us : NOT RESOLVED
S8 rusty_time  ... -> null-arm floor 1.52 us | arm gap 3.36 us : RESOLVED, rusty_time ahead
```

**Only S8 is resolved, and it is ours** (2.6 us vs 5.9 us). Every earlier
per-scenario win or loss at microsecond scale is withdrawn as unresolvable.

The G4 rig had the same disease and the null arm caught it: the two identical
chrony arms differed by **31%**, because the round order was
`A B C` / `C B A` — which looks alternating and leaves B permanently in the
middle. Rotating properly and pinning server and generator to separate cores
took the floor from 38468 replies/s to **292**, a 130x improvement in
resolution, and that is what made both G4 verdicts resolvable.

### Measured and rejected

* **Skipping the `poll` before a receive when the last batch came back full.**
  0.4% *worse*: `poll` is cheaper than the extra empty `recvmmsg` when the
  guess is wrong, and at a mean batch depth of 26.4 the condition rarely fired.
  Reverted.
* **The ungated acquisition ratio**, above.

Gates: 179 tests, clippy clean, 8 targets green, **NTS interop PASS both
directions** after the server restructure (chronyd selects us `^*` at +1583 ns),
S12a/b/c counts identical.

---

## Steady-state accuracy — the rig had to be fixed first

**Goal:** beat chrony on steady-state accuracy, which a distributed cloud needs
more than it needs fast convergence.

### The rig was not deterministic, and a comment said it was

`bench_vs_chrony.sh` opened with:

> *clknetsim is deterministic for a given config, so each number is exactly
> reproducible rather than a sample from a noisy rig.*

That was false, and further down the same file knew it — *"clknetsim redraws its
random streams each run, so a single run is one sample"*. Two runs of
**identical code** produced steady-state biases 2.4 us apart:

```
seeded   run 1: 99946d9ad56c9dfac72a39f06259aa42
seeded   run 2: 99946d9ad56c9dfac72a39f06259aa42   <- bit-identical
unseeded run 1: 09956fcbb67177f97a6da30ac40560fc
unseeded run 2: 30a506ba1974e68fd904547fc87ea4b9   <- different world
```

clknetsim honours `CLKNETSIM_RANDOM_SEED` in **both** halves — `server.cc:233`
for the network delay streams and `client.c:417` for the node's own `random()`.
Setting it makes a run bit-reproducible.

The rig now derives rep N's seed from `SEED_BASE + N`, so **rep N of every arm
faces the same world, packet for packet**. That is a stronger fairness claim
than "the same distribution", and it turns a paired comparison from a
statistical exercise into a deterministic one.

The false claim was not harmless: it is what licensed `REPS=1`, and the first
three-arm experiment run under it produced three different answers that were
entirely the draw.

### T1 placement: a real hypothesis this rig cannot test

The steady-state bias was negative (clock ends up ahead), which fits a T1
stamped *before* `send()` returns: the packet leaves after we stamped it, so
`(t2 - t1)` is too large, the offset reads too positive, and the loop runs the
clock fast. T1 is local bookkeeping — the client's transmit timestamp goes on
the wire as a nonce, never as a time — so it can legitimately move.

Three arms (stamp before `send`, after `send`, midpoint), five seeds:

```
seed   arm        signed(us)  mean|e|(us)
101    before/after/mid  -1.35         1.35
202    before/after/mid  +0.91         0.91
303    before/after/mid  -0.18         0.36
404    before/after/mid  -1.68         1.68
505    before/after/mid  -2.03         2.03
```

**Bit-identical across all three arms, in every seed.** clknetsim's virtual
clock advances only at poll boundaries, so `send()` takes exactly zero
simulated time and both clock reads return the same value. The arm is wired;
the simulator cannot express the effect.

Two conclusions, and the second matters more than the first:

1. T1 placement is **unmeasurable on this rig**. Parked for hardware, not
   refuted — and not shipped, because an unmeasured change is not a win.
2. Therefore send latency **cannot be** the cause of the bias measured here.
   The sim has none. The earlier "bias/noise = 1.00, ours is negative"
   reading was real but its explanation was wrong.

What the seeded data shows instead: the bias is **per-world, not per-code** —
it changes sign with the seed (-1.35, +0.91, -0.18, -1.68, -2.03 us). That is
sampling error in the delay draws, not a standing defect in the loop.

The arithmetic agrees. S1 draws 10 us exponential jitter each way, so a single
sample's offset error is about (d1 - d2)/2 with an SD near 7 us. Averaged over
the ~30-40 samples the estimator effectively uses, that predicts ~1.1-1.3 us —
which is what both implementations measure. **We are at the statistical floor
for the number of samples used**, so the lever is the estimator's effective
sample count, not the controller's gain.

### The integral trim: measured and rejected

Twenty seeds per arm, paired seed by seed, S1 and S8:

```
=== S1 - paired against chrony (20 seeds) ===
  chrony             median |e|   1.52 us   worst   5.53 us
  rusty_time gain=0.0  median |e| 1.47 us    9/20 wins  z=-0.45  NOT RESOLVED
  rusty_time gain=0.1  median |e| 1.98 us   10/20 wins  z=+0.00  NOT RESOLVED

=== S8 - paired against chrony (20 seeds) ===
  chrony             median |e|   5.24 us   worst  12.20 us
  rusty_time gain=0.0  median |e| 4.78 us    8/20 wins  z=-0.89  NOT RESOLVED
  rusty_time gain=0.1  median |e| 6.20 us    5/20 wins  z=-2.24  RESOLVED, chrony ahead
```

Turning the trim on is **the only resolved result in the sweep, and it is a
regression**. Default returned to 0.

The earlier single unpaired run had read the same change as an improvement on
*both* scenarios (S6 1.50 -> 1.28 ms, S8 4.43 -> 4.12 ms). That reading was the
draw. This is precisely the failure the seeded rig now prevents, and it is
worth being blunt about: the theory was right (a proportional controller cannot
zero a constant drift), the implementation did what the theory said, and the
measurement still says no — because the standing offset here is not a constant
drift. It changes sign with the seed. It is sampling error, and integrating
sampling error is how you put noise into a frequency estimate.

### Where the accuracy gap actually is: packets, not the estimator

clknetsim counts packets itself. Reading its statistics rather than either
implementation's opinion, same worlds, same configured poll bounds
(`--minpoll 4 --maxpoll 6` for both):

```
             mean poll   median |e| S1   median |e| S8
chrony          33.9 s         1.52 us         5.24 us
rusty_time      40.1 s         1.47 us         4.78 us
```

chrony buys **18% more samples than we do**. Offset error falls as `1/sqrt(N)`,
so that is worth `sqrt(1.18)` = **1.087x** to chrony before its estimator does
any work at all — against a measured raw gap of 1.12x on S1. Almost the whole
gap is the packet budget.

Dividing it out asks the question the raw win rate cannot — *which estimator is
better at equal cost?*

```
        raw (x)   per packet spent      poll
S1        1.12    x1.05   9/20  z=-0.45   40.1 s vs 33.9 s
S8        1.05    x0.96  11/20  z=+0.45   41.2 s vs 33.9 s
```

**At equal cost we are at parity on S1 and slightly ahead on S8.** The
estimator was never the deficit. Two months of this project's accuracy work
would have gone into the filter; the counter says the filter is fine and the
poll adaptation is what is leaving accuracy on the table.

**First proposed mechanism, and its refutation.** The obvious suspect was the
dead band: an offset below `2 * noise` counts as stable and doubles the poll
after three such samples, while the interval only comes back down above
`10 * noise`. A band that wide should pin the client at maxpoll.

Sweeping `poll_down_noise_ratio` over 10, 6, 4, 3 — 200 cells, twenty seeds:

```
S1  ratio 10 / 6 / 4 / 3   ->  poll 40.1 s in EVERY arm, median |e| 1.47 us in every arm
S8  ratio 10 / 6           ->  poll 41.2 s      ratio 4 / 3 -> poll 40.9 s
```

Identical. The step-DOWN branch is reached only when `|offset| >= 2 * noise`,
and a converged loop is almost never there — it is classified stable on nearly
every sample, so the descent path is dead code in steady state and its
threshold cannot matter. The band is not the mechanism; the **climb** is. The
loop reaches maxpoll after three samples and never has cause to leave.

Recorded rather than quietly replaced, because the refuted version is the one
that sounds right, and a plausible mechanism nobody measured is exactly how the
integral trim got written.

**The instrument lesson, again:** the honest comparison of two time daemons
states how many packets each one spent. A pure accuracy number ranks whoever
polls hardest, and every table in this ledger before this one omitted it.

### Buying accuracy with packets: it works on S1, and it is not a win

`poll_up_streak` — consecutive stable samples before the interval doubles — is
the term with real authority over the packet budget. Twenty seeds, four values:

```
                    poll   median |e|   raw    win     per packet
S1  chrony         33.9 s     1.52 us     -      -          -
S1  streak  3      40.1 s     1.47 us  x1.12   9/20      x1.05
S1  streak  6      35.7 s     1.16 us  x1.04   8/20      x1.01
S1  streak 10      31.0 s     1.72 us  x1.22   8/20      x1.25
S1  streak 16      25.5 s     1.33 us  x0.86  12/20      x1.01

S8  chrony         33.9 s     5.24 us     -      -          -
S8  streak  3      41.2 s     4.78 us  x1.05   8/20      x0.96
S8  streak  6      36.3 s     5.08 us  x0.98  11/20      x0.96
S8  streak 10      31.4 s     5.72 us  x1.22   3/20      x1.25   RESOLVED, chrony ahead
S8  streak 16      26.5 s     4.55 us  x1.08   8/20      x1.20
```

Two things to take from this, and one to refuse to take.

**The tension is real and it is structural.** S1 improves as the poll shortens
and S8 does not. The register holds a fixed 64 samples, so polling faster buys
sample COUNT by spending register SPAN — and a frequency slope is estimated
from span, not count. S1's oscillator is a constant +20 ppm, so its slope is
easy and the extra samples are pure gain. S8's wanders, so its slope is the
whole problem and shortening the baseline costs more than the samples return.
A single fixed climb rate cannot serve both.

**Per packet, nothing improved.** Every arm that got more accurate did it by
polling harder, at x1.01 to x1.25 efficiency — the estimator did not get
better, it was handed more data. That is not a win against chrony, it is
matching chrony's bill. The default stays at streak 3.

**What to refuse:** `streak 16` posts the best raw S1 number in the table
(x0.86, 12/20) and it would be easy to ship that line. It is z=+0.89 — not
resolved — its per-packet figure is x1.01, and the same setting is x1.20 on S8.
Picking the best cell out of eight comparisons is picking noise; at |z|>2 with
eight arms, roughly one false winner per sweep is the expected yield.

### The weight floor: a confirmed win that was not one

The regression weights a sample by `(floor / (excess_delay + floor))^2`, with
`floor = min_delay * 0.125`. That floor is tied to the PATH LENGTH, while the
quantity it stands for — the error scale of a zero-excess sample — is set by
timestamp resolution. Narrowing it should sharpen the weighting toward true
inverse-variance.

Twenty seeds, S1 and S8, four values. `0.03125` looked excellent: S1 median
1.11 us against chrony's 1.52, per packet **x0.76**, at an unchanged poll — an
estimator gain, not one bought with packets.

Forty FRESH seeds confirmed it:

```
S1  0.03125   28/40 wins  z=+2.53   RESOLVED, rusty_time ahead   (0.97 us vs 1.50)
```

Then the same change was run across the whole corpus, paired against our own
default, forty seeds per scenario:

```
        wins/40      z        verdict
S1        31      +3.48   RESOLVED better
S2         5      -4.74   RESOLVED worse
S4        15      -1.58   worse (per packet -2.21, RESOLVED worse)
S6         9      -3.48   RESOLVED worse
S8        16      -1.26   worse
```

**Rejected.** It buys S1 and sells every other path in the corpus.

The lesson is not "we got unlucky" — it is that the confirmation run was the
wrong experiment. Forty fresh seeds cleared |z| > 2 honestly, and every one of
them was on S1 or S8, the two scenarios the value had been chosen on. Varying
the SEED is not varying the axis that can flip the answer. The rig was made
deterministic specifically so that small effects could be resolved, and it
resolved this one correctly and pointed the wrong way, because the question put
to it was too narrow.

**The unifying mechanism, which now has three independent confirmations.**
Every knob that improves the offset estimate by concentrating information onto
the best samples degrades the frequency estimate by shortening the baseline it
is fitted over:

| knob | concentrates by | S1 (constant 20 ppm) | drifting paths |
|---|---|---|---|
| `poll_up_streak` up | more samples, less span | better | S8 worse (RESOLVED) |
| `weight_floor_ratio` down | fewer samples dominate | better | S2/S4/S6 worse (RESOLVED) |
| `freq_integral_gain` up | offset fed into frequency | worse | S8 worse (RESOLVED) |

S1's oscillator is a constant +20 ppm, so its slope is free and every scrap of
concentration is pure gain. Every other scenario pays for it. A single scalar
cannot serve both, which is why all three sweeps produced the same shape of
answer. The next thing worth building is a fit that weights the INTERCEPT and
the SLOPE differently — sharp weights for where the clock is, broad weights for
how fast it is running — rather than a fourth scalar that trades one against
the other.

### Standing against chrony, steady state, as measured

Forty seeded worlds per scenario, paired, shipping defaults both sides, with
the packet count each arm spent:

```
      chrony |e|   rusty_time |e|   wins/40     z      per packet   verdict
S1       1.09 us         1.55 us     17/40   -0.95      x0.95      not resolved
S2    6689 us         7286 us         3/40   -5.38      x0.99      RESOLVED chrony ahead
S4    1902 us         1865 us        23/40   +0.95      x0.61      not resolved
S6       0.98 us         3.04 us      8/40   -3.79      x3.19      RESOLVED chrony ahead
S8       3.24 us         4.21 us     14/40   -1.90      x1.17      not resolved
      poll 33.9 s      poll ~41 s
```

**The goal — better than chrony in steady state — is not met.** Level on S1,
S4 and S8; resolved behind on S2 and S6.

*(Superseded below: the split-weighting change that came out of this diagnosis
did earn its way in, and moves S2 and S6. The standing table is restated at the
end of this section.)*

Two concrete targets came out of it, both resolved and both diagnosable:

* **S6, x3.4 behind (3.04 us vs 0.98 us).** The largest relative gap in the
  corpus, and it is steady state, not the cold start S6 was built to measure.
* **S2, 596 us of avoidable error.** The path is 26.7 ms one way and 13.3 ms
  the other, so NTP cannot do better than `(26.7 - 13.3) / 2` = 6700 us.
  chrony measures 6689 us — it is *at* the theoretical floor. We measure 7286.
  The unavoidable part is not the interesting part; the 596 us is ours.

### SHIPPED: the offset and the slope get their own weights

Three sweeps all produced the same shape of answer — better on S1, worse on
everything with a frequency to work for — because one weight set was answering
two questions that want opposite things:

* **Where is the clock?** Best told by the few samples that queued least.
  Concentrate.
* **How fast is it running?** A slope, and a slope wants a long baseline.
  Spread out.

So the fit now uses both. The slope keeps the broad weights it always had; the
offset is re-seated on sharp ones, holding that slope fixed. With `b` already
decided, the sharply-weighted intercept is just the weighted mean offset taken
about the weighted mean time — the `b * (t - t0)` terms cancel there — so it is
one extra pass, no second solve, and it cannot perturb the frequency estimate.

Paired against the previous single-weight fit, fifty fresh seeded worlds per
scenario, S8 re-run at a hundred to settle a near-miss:

```
       median |e|  ->  median |e|     wins       z      verdict
S1        1.22 us       1.25 us      28/50    +0.85    better, not resolved
S2      7164 us       6759 us        42/50    +4.81    RESOLVED better
S4      2575 us       2696 us        26/50    +0.28    neutral
S6         2.74 us       1.49 us     41/50    +4.53    RESOLVED better
S8         3.71 us       3.64 us    47/100    -0.60    neutral
```

**Two resolved improvements, no resolved regression**, replicated across two
independent seed sets (the discovery run read +2.19 and +2.92 on thirty). S2
gains 405 us; S6 gains 45%. Convergence is untouched — S1 5 s, S6 14-16 s,
S8 5 s in both arms — and p50/max improve or hold on all three.

**The S8 near-miss is why the hundred-seed run exists.** At fifty seeds it read
`18/50, z=-1.98` — close enough to the bar to be reported as a regression by
anyone rounding. At a hundred it is `47/100, z=-0.60`, with the candidate's
median very slightly *ahead*. A z just under the threshold is not a small
effect, it is an unresolved one, and the fix is more seeds rather than a verdict.

#### Why it works, which the diagnosis found before the fix

S6's error was never noise. Sampled through a run it was positive at almost
every point, and across forty worlds its signed bias was **+2.74 us against
chrony's +0.05** — while our *variance* was lower than chrony's. That is a DC
bias, and DC bias means bookkeeping, not jitter.

The cause is in the corpus definition. Delay jitter is drawn `exponential`, and
real queueing is shaped the same way: a packet can be delayed a great deal and
cannot arrive early. Averaging a skewed distribution broadly does not just add
noise, it adds a standing bias, because the entire tail lies on one side.
Inverse-variance weighting on the offset rejects that tail. The slope never
cared, because a constant bias does not tilt a line.

That also predicts the shape of the result, and did so before it was measured:
the largest gains land on the most skewed and asymmetric paths — S2 (2:1
asymmetry) and S6 — and little changes on S1 (little jitter) or S8 (dominated by
oscillator wander rather than delay).

#### Standing against chrony after the change

Fifty seeded worlds per scenario, paired, shipping defaults both sides:

```
      chrony |e|   rusty_time |e|   wins/50     z     per packet   verdict
S1       1.35 us         1.25 us     27/50   +0.57     x0.85      level
S2    6792 us         6759 us        28/50   +0.85     x0.91      level raw;
                                     47/50   +6.22                RESOLVED ahead per packet
S4    2007 us         2696 us        18/50   -1.98     x1.40      not resolved
S6       1.12 us         1.49 us     15/50   -2.83     x1.24      RESOLVED chrony ahead
S8       3.44 us         4.03 us     15/50   -2.83     x1.06      RESOLVED chrony ahead
```

**Still not "better than chrony" overall**, and the honest headline stays that.
What changed is the size of the gap: S6 went from **x2.37 to x1.35**, and S2
from resolved-behind to level (and resolved *ahead* per packet spent, 47/50,
z=+6.22 — we reach chrony's accuracy on that path while sending ~16% fewer
packets). S4 is now the largest untouched gap and has never been investigated.

---

## Steady state, settled: parity with chrony, and what stopped it going further

### The definitive standing

One hundred seeded worlds per scenario, paired, shipping 0.1.1 both sides, with
the packets each arm spent:

```
      chrony |e|   rusty_time |e|   wins/100    z     per packet   verdict
S1       1.38 us         1.23 us      52/100  +0.40     x0.84      level
S2    6717 us         6722 us         54/100  +0.80     x0.91      level raw;
                                      92/100  +8.40                RESOLVED ahead per packet
S4    2022 us         2781 us         42/100  -1.60     x1.19      not resolved
S6       1.35 us         1.77 us      40/100  -2.00     x1.04      not resolved
S8       4.02 us         4.21 us      40/100  -2.00     x0.99      not resolved
      poll 33.9 s      poll ~40 s
```

**Nothing is resolved against us anywhere.** At the start of this work S2 and S6
were resolved behind at z = -5.38 and -3.79; both are now inside the noise, and
on S2 we deliver chrony's accuracy on **16% fewer packets** (92/100, z = +8.40).

**And nothing is resolved for us on raw accuracy either.** That is parity, not
"better than chrony", and the goal asked for better. Said plainly so no later
reader has to infer it.

### Four more knobs, four more times the corpus refused to agree

Every lever tried after the offset/slope split produced the same shape of
answer, and it is worth putting them in one table because the shape IS the
finding:

| knob | helps | hurts |
|---|---|---|
| `poll_up_streak` (more packets) | S1 | S8 (RESOLVED) |
| `weight_floor_ratio` down (single weight) | S1 | S2, S4, S6 (RESOLVED) |
| `freq_integral_gain` up | — | S8 (RESOLVED) |
| `offset_age_halflife_s` | S1, S8 (RESOLVED) | S2, S6 (RESOLVED) |
| `offset_weight_dispersion_k` = 0.15 | S2 (RESOLVED), S4 | S1, S6, S8 (mild) |

Five scalars, five splits. The corpus is not being awkward — the scenarios
genuinely want opposite things, and a single constant cannot serve a steady
oscillator and a wandering one, or a path whose jitter is a tenth of its length
and one whose jitter is four times it.

### The adaptive gate that should have fixed that, and why it did not

If a constant cannot serve both, gate it on which case you are in. Decay is
right exactly when old samples are STALE, and stale is a property of the
oscillator — so the register fitted its older and newer halves separately and
compared the slopes against the standard error of their difference.

Measured across the corpus, that statistic does not discriminate:

```
       p50    p75    p90    max    fires at K=1.5
S1    0.49   0.80   1.19   1.98        2.5%
S2    1.35   1.65   1.91   2.40       38.3%     <- steady frequency, decay HURTS
S4    1.03   1.78   2.28   3.55       40.9%
S6    0.78   1.03   1.29   1.98        7.4%
S8    0.79   1.26   2.26   2.77       20.0%     <- wandering frequency, decay HELPS
```

It fires nearly **twice as often on S2, whose frequency is constant, as on S8,
whose frequency is the random walk the test exists to find** — S8's median
separation sits *below* S2's. No threshold separates them, because on a
high-jitter path two half-window slopes disagree from measurement noise long
before any oscillator moves. Removed rather than shipped: a knob named for
drift detection that actually measures jitter is worse than no knob.

Separating wander from noise needs a statistic that discriminates by **LAG** —
an Allan variance over successive frequency estimates — not a single window
split. That is the next real piece of work, and it is a campaign, not a knob.

### The instrument reported a confident verdict on a null arm

`paired_verdict.py` scored ties as losses. Two bit-identical arms therefore read

```
rusty_time150   0/40 wins  z=-6.32  RESOLVED, rusty_time0 ahead
```

— a resolved verdict, with a large z, on code that was byte-for-byte the same
on both sides. It was caught only because the medians and the signed bias
matched to the last digit, which is what a null arm looks like.

Ties are now discarded (the textbook sign test) and an all-tied comparison
reports `IDENTICAL`, not a verdict. Every earlier number in this ledger was a
comparison between arms that genuinely differed, so none of them are affected —
but the defect would have corrupted the first adaptive result that half-fired,
which is exactly the case it was introduced for.

### The S6 standing bias: both obvious mechanisms excluded

S6 and S4 carry standing biases far larger than chrony's (+1.23 us vs +0.05;
+1149 us vs +314). A DC bias is the most attractive target left, because unlike
every knob above it cannot trade — removing a bias costs no variance.

Two mechanisms were proposed, implemented and measured. Both are wrong.

**1. The extrapolation arm.** The offset is `mean_offset + b * (now - t0)`, and
`t0` sits mid-register, hundreds of seconds back. A 6 ppb slope error over a
192 s correction time is 1.2 us — exactly the bias. Shortening the arm by
age-decaying the offset weights should remove it.

It made S6 *worse*, and the bias went UP (+0.88 -> +1.34 us).

**2. The iburst cluster as a lever.** With the extended acquisition burst, a
cold start puts up to twenty samples in the first forty seconds and about
twenty-five across the next twenty minutes — nearly half the register inside
three percent of its span, all at one end of the time axis. That is textbook
high leverage, taken while the clock was slewing hard. Weighting the slope by
the time each sample represents should defuse it.

It made S6 *much* worse — median 1.59 -> 4.01 us, bias +0.77 -> -3.72 — and S1
worse with it:

```
         S1        S2        S4        S6        S8
      -4.11     +2.53     +0.00     -2.85     +1.58
```

The cluster is **load-bearing**, not parasitic. Those are the only samples taken
while the 500 ms transient was being removed, so they carry most of what is
known about the frequency. The leverage that makes them dangerous in theory is
the same leverage that makes them informative.

Both obvious explanations are now excluded by measurement rather than by
argument. That is worth as much as a fix: the next person does not spend a day
on either.

### Stopping, and why

Seven levers now: poll rate, weight floor, integral trim, offset/slope split
(shipped), offset age decay, an adaptive gate on that decay, a dispersion-scaled
floor, and slope density weighting. One shipped. The rest either trade one
scenario against another or are refuted outright.

The measurement is stable and it says **parity**: at one hundred seeds per
scenario nothing is resolved in either direction on raw accuracy, and we deliver
chrony's S2 accuracy on 16% fewer packets. The goal asked for *better*, and
better is not supported.

Continuing to sweep scalars is now the wrong move, and it is worth naming why:
with five scenarios and two verdict flavours, each sweep runs ten comparisons,
so a |z| > 2 result turns up by luck roughly once per sweep. Seven sweeps in,
the risk is no longer failing to find an effect — it is *finding* one that is
not there. The corpus has said the same thing seven times: these scenarios want
opposite things from any single constant.

What would actually move it, stated so it is not re-derived:

* **An estimator that adapts on a statistic that works.** The half-window slope
  test failed because it cannot separate measurement noise from oscillator
  wander. Separating those needs discrimination by LAG — an Allan variance over
  successive frequency estimates. That is the one direction with a real
  mechanism behind it, and it is a campaign.
* **The S6/S4 standing bias**, now with two mechanisms excluded.
* **S4's tail**: worst case 13887 us against chrony's 6337 in the same worlds,
  the largest single discrepancy left anywhere in the corpus.

### Why more packets never helped: the time constant is tied to the poll

The steady-state drain rate is `offset / (CORR_TIME_RATIO * poll_interval)`.
That makes the loop's aggressiveness a function of how often it looks: polling
twice as fast does not average twice as much, it halves the time constant and
writes twice as much sample noise into the clock. Every attempt to buy accuracy
with packets failed for this reason — the packets bought twitchiness.

Re-testing the poll rate after the offset/slope split (a refutation expires when
its baseline moves) confirmed it. Matched to chrony's packet rate:

```
        S1      S2      S4      S6      S8     poll
k8   +0.95   +2.53   +0.95   -2.21   -4.43    32.7 s   (chrony 33.9 s)
```

S1, S2 and S4 improve; S6 and S8 become RESOLVED losses. Same trade, one level
down.

So the two were combined — an absolute correction time AND chrony's packet
rate, the one pairing that could convert per-packet parity into raw advantage,
and which neither knob alone can show:

```
                S1      S2      S4      S6      S8     poll
base         -0.63   +0.63   -1.90   -2.21   -1.26    ~40 s
t=200        -0.95   +0.63   -0.63   -3.48   -1.26    ~38 s
t=120,k8     +0.32   +1.90   -0.32   -2.21   -1.90    ~32 s
t=200,k8     +0.32   +0.63   -0.32   -2.53   -2.85    ~31 s
```

**Nothing resolves ahead anywhere, in any arm.** S6 stays resolved behind in all
four. The absolute constant additionally destabilises the poll adaptation — S2
fell to a 21 s poll, a third more packets for no gain — because the stability
test that raises the interval is calibrated against a correction time that now
does not move with it. Rejected.

### Concluded: parity, and the goal is not met

Nine levers: poll rate (twice, before and after the split), weight floor,
integral trim, offset/slope split, offset age decay, an adaptive gate on it, a
dispersion-scaled floor, slope density weighting, and an absolute correction
time. **One shipped.** Everything else trades one scenario against another or is
refuted outright.

The answer to "can we be better than chrony in steady state" is **no, not by any
of these routes**. At one hundred seeds per scenario the shipped loop is level:
nothing resolved in either direction on raw accuracy, and chrony's S2 accuracy
delivered on 16% fewer packets.

That is a real improvement on where this started — S2 and S6 were resolved
behind at z = -5.38 and -3.79 — and it is not what was asked for. Recorded
plainly so nobody has to infer it from a table.

**The structural reason, which is the actual finding.** Every knob in this loop
is one number serving five paths that want different things: a steady oscillator
and a random-walk one, a path whose jitter is a tenth of its length and one whose
jitter is four times it, a cold start and a settled clock. Nine sweeps produced
nine versions of the same answer. The next real gain is not another constant, it
is an estimator that measures which case it is in — and the one attempt at that
failed because a half-window slope test cannot separate measurement noise from
oscillator wander. Doing it needs discrimination by LAG: an Allan variance over
successive frequency estimates. That is the whole next campaign, and it is the
only route left with a mechanism behind it rather than a hope.

---

## 0.1.2 — the correction time was carrying the standing bias

### Diagnosis first, and that is why this one worked

Nine levers had failed by trying to shrink the residual FREQUENCY error. The
tenth asked a different question: is the estimator even wrong?

The rig has ground truth and the daemon has `--verbose`, so the two can simply
be compared. Steady state, S6:

```
seed=401   loop believed -1.499 us   truth -1.209 us
seed=402   loop believed -0.151 us   truth +0.512 us
seed=403   loop believed -1.230 us   truth +1.192 us
```

**The estimator was right.** The loop could see the error and was not removing
it — which makes the standing bias a property of the CONTROLLER, not the filter.

A proportional loop settles where its drain balances the drift re-creating the
offset: `offset = F_residual * corr_time`. Nine sweeps had been attacking
`F_residual`, which trades against everything. `corr_time` is a free parameter,
and the standing offset is LINEAR in it. That is a quantitative prediction:
shorten the correction time, shrink the bias in proportion.

It held. **S6's standing bias went from +1.27 us to +0.24 us, against chrony's
+0.26.** The corroboration was already in the data — the earlier `t=200` arm, a
LONGER correction time, had made S6 resolved-worse (z = -3.48). Same
relationship, opposite direction.

### Shipped: CORR_TIME_RATIO 3.0 -> 1.0

Paired against the old ratio, sixty fresh seeded worlds per scenario:

```
         S1        S2        S4        S6        S8
      +0.77     +4.65     +0.77     +1.29     +3.36
```

Two resolved improvements, **no resolved regression, every scenario trending
better**, convergence untouched (S1 5 s, S6 16 s, S8 5 s in both arms).

It stays a RATIO. An absolute 40 s constant measured slightly better and is
unsafe to ship: the corpus runs `maxpoll 6` (64 s) while the production default
is `maxpoll 10` (1024 s), where a fixed 40 s would drain each estimate
twenty-five times faster than the loop can see it. That distinction is the
difference between a corpus number and a shippable one.

### Standing against chrony after 0.1.2

Sixty seeded worlds per scenario, paired, shipping defaults both sides:

```
      chrony |e|   rusty_time |e|   wins/60    z     per packet          verdict
S1       1.44 us         1.42 us     33/60  +0.77   38/60 z=+2.07   RESOLVED ahead/packet
S2    6585 us         6448 us        32/60  +0.52   60/60 z=+7.75   RESOLVED ahead/packet
S4    2038 us         2683 us        26/60  -1.03      x1.12        not resolved
S6       1.48 us         1.64 us     33/60  +0.77   34/60 z=+1.03   not resolved
S8       3.41 us         3.45 us     32/60  +0.52   40/60 z=+2.58   RESOLVED ahead/packet
```

**Raw accuracy is still parity** — nothing resolved in either direction, and the
goal asked for better. What changed is that the last resolved loss is gone (S8
was z = -2.21 before this), four of five scenarios now favour us on the win
count, and we are **resolved ahead per packet spent on three of five**.

Said plainly: we match chrony's accuracy while sending fewer packets, on most of
the corpus. We do not beat its accuracy at any spend, on any scenario, at
|z| > 2. That remains true after ten levers.

### At full power: one resolved win, one resolved loss

The 60- and 100-seed runs all read "nothing resolved either way". That was a
POWER statement, not a result: the win rates sat near 54%, and resolving 54%
needs about 150 seeds. So the shipped 0.1.2 was run against chrony at 150 seeds
per scenario, pre-registered, all five scenarios, reporting whatever came out.

```
      chrony |e|   rusty_time |e|   wins/150     z      per packet        verdict
S1       1.37 us         1.30 us      67/150   -1.31      x1.06        level
S2    6679 us         6586 us         92/150   +2.78      x0.85        RESOLVED, ours
S4    2331 us         2274 us         84/150   +1.47      x0.77        level
S6       1.34 us         1.65 us      59/150   -2.61      x1.30        RESOLVED, chrony
S8       3.40 us         3.01 us      77/150   +0.33   92/150 +2.78    level raw,
                                                                       RESOLVED ours/packet
```

**S2 is the first resolved raw-accuracy win over chrony in this project.** On a
WAN path with 2:1 delay asymmetry we are ahead at z = +2.78 — and ahead per
packet at z = +11.76, on 147 of 150 worlds.

**S6 is a resolved loss**, and it is no longer a bias: 0.1.2 removed that
(signed bias +0.06 us against chrony's -0.07). What remains is variance in the
middle of the distribution, and it has an odd shape — chrony's median is better
(1.34 vs 1.65) while OUR worst case is better (4.68 vs 5.84). We are steadier at
the tail and looser in the body. For a distributed cloud that trade is arguably
the right way round, but it is a trade, not a win.

**This supersedes the earlier "parity everywhere" reading**, which was taken at
100 seeds where S6 sat at z = -2.00, exactly on the threshold. More seeds moved
it to -2.61 and moved S2 to +2.78. Two verdicts that were called unresolved were
unresolved only for want of samples — which is what "NOT RESOLVED" always meant
and is worth restating, because it is easy to read as "no difference".

### The answer to the question that started this

*Can we improve steady-state accuracy to better than chrony?*

**Partly, and now precisely: better on one of five scenarios, worse on one,
level on three, and ahead per packet spent on three.** Not "better than chrony"
as a flat claim, and the corpus is now sharp enough to say exactly where the
line falls rather than hedging.

Where it started: S2 and S6 both resolved AGAINST us at z = -5.38 and -3.79.
S2 has crossed over. S6 has not, and its remaining deficit is a different
quantity from the one that was fixed.

---

## The S6 client path: ten measured instruction wins, −47.4%

The server had an instruction-count harness (`tools/perf/ir.sh`); the client —
the loop that runs on every node, and the only one that runs on most of them —
had none. `benches/client_path.rs` is that harness, shaped like S6: a 500 ms
cold start, so it exercises the burst, the makestep, the drain retirement and
then long steady-state operation, rather than only the cheapest state.

The gate is a checksum over every plan the controller emits, folded by exact
f64 BITS. This is instruction-count work at FIXED behaviour, so bit-identity is
the bar; anything meant to change the numbers belongs in the corpus harness
behind a paired test.

```
                                                       Ir            delta
baseline                                        410,038,646
 1  median by SELECTION, not a full sort         328,498,788   -81,539,858  -19.89%
 2  each sample's weight computed ONCE           297,355,056   -31,143,732   -9.48%
 3  `times` built only when it is read           293,353,678    -4,001,378   -1.35%
 4  residual computed once, branch-free loops    285,296,800    -8,056,878   -2.75%
 5  median keyed on the raw bit pattern          269,911,287   -15,385,513   -5.39%
 6  sign-run test stops when the answer is known 267,183,063    -2,728,224   -1.01%
 7  window span from its ends, not a scan        249,813,175   -17,369,888   -6.50%
 8  min_delay maintained by `push`               243,791,090    -6,022,085   -2.41%
 9  flat rows instead of (&Sample, weight)       242,748,302    -1,042,788   -0.43%
10  row buffers owned by the register            215,512,750   -27,235,556  -11.22%
                                                              ------------
                                                              -194,525,896  -47.44%
```

25,627 Ir per discipline step down to 13,469. Behaviour is bit-identical: five
corpus cells across S1, S2, S4, S6 and S8 produce the same figures to four
decimal places as the commit before this one.

**The two biggest were not micro-optimisation.** A full stable sort was being
run to answer one median question (23% of the path), and the regression's
scratch buffers were allocated and freed on every single estimate forever at a
size the register already knew (11%). Neither is a clever trick; both are work
that did not need doing.

**Three attempts measured WORSE and were reverted**, which is the reason each
line above has a number beside it:

* A reusable `&mut Vec` scratch for the residual pass: **+3.4M Ir**. The
  indirection cost more than the allocation it removed.
* A fixed `[f64; 64]` stack buffer for the same: **+0.3M**. It memsets 512
  bytes on every call to hold twenty-odd values — the hidden-memset trap.
* Fusing the two residual walks into one loop with an `if i < half` inside:
  **+1.8M**. Removing duplicated arithmetic is not a win if it costs the
  vectoriser a branch-free loop; the version that finally paid computes the
  residuals once *and* keeps every loop branch-free.

**A near-miss worth recording.** The corpus check appeared to fail — S6 seed 401
read -2.2677 against an expected -1.2089 — and two changes were bisected out
chasing it. The expected value was stale: -1.2089 was the *pre-0.1.2* number,
from before `CORR_TIME_RATIO` 3.0 -> 1.0 deliberately moved it. Stashing the work
and measuring HEAD settled it in one run. A reference value carried forward by
hand is a baseline that expires silently, and the fix is to measure the
baseline rather than remember it.

---

## 0.1.3 — where performance stands, all arms

One place with every current number and its provenance, because they were
scattered across a dozen sections and two of them were being quoted from
memory.

### Server: measured, ours

```
arm             answered   replies/s   cpu us/reply
rusty_time        200000      138664          2.950
chrony            200000      105668          4.100
chrony_null       200000      105960          4.050
```

**1.31x chrony's throughput at 0.72x its CPU per reply.** Both resolved far
above the null-arm floor — 113x for throughput, 23x for CPU. Measured before
0.1.1 and still current: every server-path file is unchanged since, all work
after that point being client-side.

**p99 is not claimed.** At concurrency 64 against a saturated server all three
arms sit within 3% of each other, which is generator queueing rather than the
server. That half of G4 needs a sub-saturation rate arm that does not exist.

### Steady-state accuracy: one win, one loss, three level

150 seeded worlds per scenario, paired, shipping defaults both sides. Mean
absolute error over the last quarter of the run.

```
      chrony      rusty_time    wins/150      z      per packet
S1    1.37 us      1.30 us       67/150    -1.31    +0.80 ours
S2 6679 us      6586 us          92/150    +2.78   +11.76 ours   RESOLVED ours
S4 2331 us      2274 us          84/150    +1.47    +1.96
S6    1.34 us      1.65 us       59/150    -2.61    -2.29        RESOLVED chrony
S8    3.40 us      3.01 us       77/150    +0.33    +2.78 ours
```

### Whole-run accuracy and convergence

Five reps, seeded, median across reps. This measures the WHOLE run, so it
includes the convergence transient — which is why S4 and S6 read differently
here than in the steady-state table above. The two answer different questions.

```
      p50 chrony -> ours     max chrony -> ours     to <1 ms
S1     1.4 ->  0.8 us          2.1 ->  3.9 us        7s ->  5s
S2  6.662 -> 6.729 ms        7.12 -> 7.14 ms        neither
S4  3.927 -> 1.082 ms        11.3 ->  6.8 ms        never -> 1138s
S6     1.4 ->  0.9 us          2.1 ->  5.4 us       12s -> 16s
S8     5.1 ->  3.5 us         16.9 -> 10.0 us        7s ->  5s
```

**chrony and chrony_null came out bit-identical in every row.** The rig is
seeded, so two identical arms produce identical output and the resolution floor
is exactly zero — every gap in that table is code, not noise. It is worth
saying plainly how unusual that is: the same rig had a 38,468 replies/s floor
before it was fixed.

S4 is the strongest whole-run result anywhere in the corpus: 3.6x better p50, a
better tail, and it reaches 1 ms inside the run where chrony does not.

### Instruction cost, deterministic

```
server   237.1 Ir/request      (from 3053.6 — -92%)
client 13469   Ir/discipline step (from 25627 today — -47.4%)
```

### What is NOT measured, stated so nobody assumes it is

* **Client CPU against chronyd.** The 47% client win is against our own
  baseline. There is no cross-implementation CPU figure for the client path the
  way there is for the server — and on a mesh, the client is what runs on every
  node.
* **Server p99**, above.
* **Sub-microsecond anything.** The GPS/PPS path has never run on real hardware.

---

## The S6 client path, round two: ten more wins, −23.3%

Same harness, same gate — a checksum over every emitted plan folded by exact
f64 bits. Starting from the 0.1.3 baseline of 215,512,762.

```
                                                       Ir            delta
baseline (0.1.3)                                215,512,762
11  pass 2 skips when nothing can be trimmed     211,724,501    -3,788,261  -1.76%
12  residual sd computed once, on the survivor   202,793,243    -8,931,258  -4.22%
13  head offset, amortised compaction            195,158,296    -7,634,947  -3.76%
14  slew specialised on the live term            186,494,362    -8,663,934  -4.44%
15  decay test hoisted out of the sharp walk     183,644,656    -2,849,706  -1.53%
16  density test hoisted out of the row build    173,018,307   -10,626,349  -5.79%
17  the MAD inlined into its two call sites      171,321,950    -1,696,357  -0.98%
18  per-sample weight cache                      170,533,579      -788,371  -0.46%
19  dispersion folded into the offset walk       169,941,754      -591,825  -0.35%
20  spike test answered without a median         165,241,442    -4,700,312  -2.77%
                                                              ------------
                                                               -50,271,320 -23.33%
```

**13,469 Ir per discipline step down to 10,328.** Across both rounds:
**25,627 -> 10,328, a 2.48x reduction**, behaviour bit-identical throughout —
five corpus cells across S1, S2, S4, S6 and S8 match the published 0.1.3 to four
decimal places.

### The one worth reading

Win 20. `select_nth_unstable_by_key` was **17% of the entire client path** —
18,493 partitions for 16,000 estimates — and every one of them existed to answer
`worst > 3 * 1.4826 * mad`, a question whose answer is almost always no.

The median is not needed to answer it. `v -> 3 * 1.4826 * v` is monotone
non-decreasing over the non-negative reals, so sorting by `v` also sorts by
`f(v)`, which makes `f(mad)` the `mid`-th smallest `f`-value. Therefore
`worst > f(mad)` holds exactly when at least `mid + 1` values satisfy
`f(v) < worst` — a counting pass with no partition. The `.max(1e-9)` arm
separates cleanly, since `worst > max(f(mad), 1e-9)` is `worst > f(mad)` and
`worst > 1e-9`. The median is still selected on the rare path that will use it,
so the threshold handed back is bit-identical.

Not an approximation, not a fast path with a tolerance: the same answer, without
computing the quantity it appeared to need.

### Six measured and reverted

Each carries a number because each was tried and rejected on evidence:

* **`VecDeque` for the sliding window: +1.9M.** O(1) eviction, but its iterator
  handles a wrap and `regress` walks every sample on every estimate. Paying on
  the frequent path to save on the rare one loses. A head offset with amortised
  compaction (win 13) gets the same saving and keeps the window a plain slice.
* **A reusable `&mut Vec` for the residual pass: +1.1M, a THIRD failure.** It had
  already lost as a caller local (+3.4M) and as a register field (+2.4M); the
  retry was fair because win 17 made the callee inline, which moved the
  baseline. It still lost. Settled.
* **A `want_gap` flag so pass 2 could skip the half-means: +0.9M.** The branch
  costs more than the two sums it guards.
* **`fold(0.0, f64::max)` in place of an explicit `>` loop: +2.9M.** `f64::max`
  carries IEEE NaN semantics that a comparison does not.
* **`#[inline(always)]` on `wls_fit`: +0.9M.** Six call sites; duplicating three
  loops at each costs more than the calls. The opposite verdict to the MAD,
  which has two — inlining is not a direction, it is a measurement.
* **Caching the offset weights like the base weights: +27k.** The weight must be
  carried in the row, because trimming leaves a row unable to find its own index
  again, and the wider row costs what the saved divisions gain.

---

## 0.1.4 — a failure audit, and two real defects

Deliberately hunting for what breaks in production rather than what is slow.
Two defects found and fixed, two fuzz targets added over code that had none,
and one gap documented rather than fixed.

### Defect 1: a refused clock command left the books poisoned

`SyncController::on_sample` books a plan's effects the moment it produces it —
the frequency change tilts every stored sample, a step shifts them, the drain
budget starts counting. The daemon *then* hands the command to the driver,
which can refuse it. The old code logged the error and carried on.

So after a refusal the register held a correction that never happened. The
regression read that history as truth, the frequency estimate absorbed it, and
the daemon went on reporting itself synchronised while the clock ran free.
**Nothing looks wrong in that state**, which is the worst property a failure can
have in a time daemon.

It is reachable: `clock_adjtime` returns `EPERM` the moment `CAP_SYS_TIME` goes
away, and a seccomp policy or a container with a read-only clock refuses it too.
The startup capability probe does not help — capabilities can be dropped after
it passes.

Fixed with `revert_last_plan`, the exact inverse of what a plan applied, plus
`confirm_last_plan` on the success path. The daemon now reverts a refused
command and gives up after ten consecutive refusals rather than continuing to
report a synchronisation that is not happening. Two tests, including one that
proves a reverted controller does NOT behave like one whose command landed.

This is the third appearance of the same law in this project: **the loop's
arithmetic has to describe what the clock actually did.** First the 500 ppm
`ADJ_FREQUENCY` clamp, then booking a drain's budget instead of its delivery,
now booking a command the driver rejected.

### Defect 2: timestamps quantised to 238 ns, doubling in February 2038

Wall time was carried as f64 *seconds since 1970*. Unix time is ~1.79e9, where
an f64 has a **238 ns gap between representable values** — so every timestamp
was rounded to 238 ns before any arithmetic, and a difference of two could be
off by 477 ns. The kernel's `SO_TIMESTAMPING` path was worst: an exact
`timespec` folded into f64 seconds the instant it arrived.

The wire carries 2^-32 s = **0.233 ns**. Three orders of magnitude were being
discarded, against a measured steady-state error of about 1300 ns.

And it degrades on a schedule. **When Unix time crosses 2^31 in February 2038**
the exponent steps and the gap doubles to 477 ns, making the error in a
difference ~954 ns — comparable to the entire steady-state error. Nothing would
break loudly; the daemon would simply become less accurate, on a date.

Fixed by keeping integer nanoseconds end to end (`kernel_rx_ns: Option<i64>`,
exact until the year 2262) and taking all four timestamps RELATIVE to T1, with
the difference computed in the 32.32 fixed-point domain where the subtraction is
exact. Three tests pin it, with a tolerance of one NTP tick — the wire's own
resolution, because a tighter tolerance would be testing the test.

That change also **deleted the era guess**. Picking an NTP era by proximity to
the local clock is only as good as that clock; a difference under ±68 years is
unambiguous by RFC 5905's arithmetic, and every difference here is milliseconds.
`ntp_to_unix_near` is gone.

**No corpus regression**: forty seeded worlds per scenario, paired against
chrony, nothing resolved in either direction (S1 18/40, S2 20/40, S4 17/40,
S6 18/40, S8 18/40), with S2 still resolved ahead per packet at 40/40, z=+6.32.
The improvement itself is **not** corpus-visible — 238 ns sits far below the
rig's 10 us of jitter — which is exactly why it needed a deterministic test
rather than a rig run.

### New fuzz coverage over code that had none

The three existing targets all fuzz parsers. Two more now cover the stateful
code behind them:

```
discipline_loop     644,779 runs   clean
client_table      2,933,043 runs   clean
```

`discipline_loop` asserts the property that matters at the boundary: **a clock
command is never nonsense.** A NaN frequency does not merely compute a poor
answer — it reaches `clock_adjtime` through an `as i64` conversion that
saturates rather than trapping, so a NaN becomes 0 and an infinity becomes
`i64::MAX`. Samples are constrained to what `exchange()` actually admits, since
fuzzing values the daemon refuses would test unreachable code.

`client_table` hunts one specific hazard: `admit_handle` returns a
`ClientHandle { slot, generation }` that outlives what it points at. Between
taking a handle and using it the client can be evicted and its slot recycled, so
a wrong generation check would write one client's timestamps into another's
record — cross-client state confusion reachable from unauthenticated packets.
The target holds a handle across deliberate eviction churn and asserts no other
key's record ever moves. It did not.

### Documented, then built in 0.1.5: a bound on how far a source can move the clock

There is no `maxchange` equivalent. A single source can step the clock
arbitrarily on first sync, and afterwards drag it at up to `max_slew_ppm`
indefinitely. **chrony's default applies no limit either**, so this matches the
reference rather than falling short of it — but on a mesh where the node is
hardware you do not own, and where a capability's expiry is decided by this
clock, it is worth having. Recorded as an open item rather than invented here.

---

## 0.1.5 — `maxchange`, because a clock is believed

The audit left one gap open: nothing bounded how far a source could move the
clock. A single source could step it arbitrarily on first sync and afterwards
drag it at up to `max_slew_ppm` indefinitely. chrony applies no limit by
default either, so this matched the reference — but matching the reference is
not the bar when the node is hardware you do not own and a capability's expiry
is decided by this clock. Authentication proves who a server is, not that it is
telling the truth.

```
--maxchange <offset> <start> <ignore>
```

chrony's three-argument shape, and off by default for the same reason chrony's
is: the right value is a policy question about the deployment, not something a
library can guess. A machine with a dead RTC legitimately needs to move its
clock by years on first sync; a mesh node that has been up for a week does not,
and a source asking it to should be refused rather than obeyed.

* **`offset`** — the largest correction this daemon will make, in seconds.
* **`start`** — updates to allow through first, so a cold start can make the one
  large correction it genuinely needs.
* **`ignore`** — refusals to tolerate before giving up. Negative never gives up.

The guard runs **before** the step decision. A step is the largest and fastest
way to move a clock, so a guard placed after it would be guarding everything
except the dangerous case.

A refusal leaves the clock running exactly as it was — the commanded frequency
held, nothing drained — because that is the only honest response to an estimate
the daemon has decided not to trust. Refusals are counted **consecutively**, and
an accepted update clears the run, so one outlier among good samples cannot
accumulate toward a shutdown.

**Giving up is the point.** A daemon that refuses corrections forever and says
nothing is a machine whose clock is quietly wrong; the operator needs to find
out, and an exit is how a service says so. `ignore` negative is there for the
operator who would rather have a stuck clock than a stopped daemon — their call,
made explicitly.

Demonstrated end to end on the rig, S6's 500 ms cold start with a 1 ms limit:

```
rtimed sync: 192.168.123.1 asked for a -0.458 s correction, beyond the 0.001 s limit — refused (1x)
rtimed sync: 192.168.123.1 asked for a -0.458 s correction, beyond the 0.001 s limit — refused (2x)
rtimed sync: 192.168.123.1 still asking for a -0.458 s correction beyond the 0.001 s limit
             after 2 refusals — giving up rather than running a clock this daemon has
             decided it cannot steer
```

and the clock was left uncorrected at ~460 ms, drifting at the oscillator's own
20 ppm, which is exactly what refusing means.

Six tests pin it, including the two that matter most: that it is **off by
default**, and that the first correction of a cold start still goes through. A
safety limit that breaks legitimate cold starts would be turned off by every
operator who hit it, which is worse than not shipping it.

### What the guard costs, measured

The same deterministic harness the optimisation rounds used, so the number is
comparable to every other figure here. `client_path` gained an arm selected once
per invocation — reading the environment inside the loop would measure `getenv`,
not the guard — with a limit far above anything the workload produces, so the
guard is exercised on its real hot path: **the check that passes**, not the
refusal a deployment almost never takes.

```
                                      Ir      Ir/step     vs no guard
before the guard existed      165,241,442     10,328.2         —
guard present, OFF (default)  165,432,868     10,339.6     +0.116%   ~11 Ir/step
guard present, ON             165,545,157     10,346.6     +0.184%   ~18 Ir/step
```

**The checksum is identical in all three.** The guard is transparent when it
does not fire — enabling it changes the instruction count and nothing else about
the plans emitted, which is the property a safety limit has to have before
anyone will leave it on.

Eighteen instructions per discipline step, against 10,328. A discipline step
happens once a poll — roughly once every forty seconds — so in wall-clock terms
this is unmeasurable. It is stated anyway, because "negligible" is a claim and
this project only makes claims it has numbers for.

### The NaN case, which the obvious spelling gets wrong

Written naturally the test is `offset.abs() > limit`. That **accepts** a NaN
estimate, because every comparison against NaN is false — so the one value that
is certainly not a time would sail past the guard whose entire job is to refuse
a correction it cannot vouch for. Nothing downstream re-checks: the command
reaches `clock_adjtime` through an `as i64` conversion that saturates rather
than trapping, so a NaN silently becomes zero.

The test is therefore "refuse unless the offset is DEFINITELY within the limit",
spelled through `partial_cmp` so the third case is visible in the code rather
than implied by a negation. `NaN`, `+inf` and `-inf` are all refused, and a test
covers each. It costs 1 Ir/step when the guard is on and nothing when it is off.

Nine tests in total, and the two that matter most are still the negative ones:
that the guard is **off by default**, and that **the first correction of a cold
start goes through**. A safety limit that breaks legitimate cold starts is a
limit every operator switches off.

### A harness defect the release surfaced

Publishing 0.1.6 stopped half-done — three crates up, six still on 0.1.5 —
reporting a hard failure. The registry had returned 429, which the script is
built to wait out; it did not recognise it. The check was:

```sh
grep -q "429 Too Many Requests"
```

and cargo had said `failed to get a 200 OK response, got 429`. Matching one
PHRASING of a status rather than the status turned a routine wait into an
abandoned release. Now matched by code, and re-running walked through the
remaining six, waiting out two more limits on the way.

Worth stating because the state it left behind was the dangerous kind: not
broken, just inconsistent. `rusty_time-core 0.1.6` was live while `daemon`
was still 0.1.5, and 0.1.5 remained wholly installable, so nothing failed —
which is exactly why nobody would have noticed.

Also a reminder about the OTHER limit: three releases in quick succession spend
the burst allowance for new versions, after which it is one a minute. That is a
reason to batch changes into a release, not to publish each fix as it lands.

---

## The production-readiness audit: four gaps, two closed, two exposed

### 1. Leap seconds — CLOSED

The daemon read the leap indicator and used it only to reject `Unsynchronized`.
An announced leap was ignored, so the second arrived as an ordinary offset and
was slewed away over about twelve seconds.

Worse, and this is why it mattered more after 0.1.5 than before: with
`--maxchange` set below one second the guard REFUSED it, and since every node in
a fleet sees the same leap at the same instant, every node would exhaust its
allowance and exit **together**. A safety limit turning into a synchronised
outage, on a date known years in advance.

`--leapsecmode slew|step|ignore` now, defaulting to `slew`. An announced leap is
exempt from the change guard — but only up to two seconds, so the announcement
cannot be used to smuggle an arbitrary correction past it. Seven tests, of which
the load-bearing ones are: an announced leap is not refused, **the same offset
unannounced still is**, and an announcement does not excuse an hour.

### 2. Multi-source — EXPOSED, NOT FIXED

Every scenario in this corpus had exactly one server. `tools/corpus/multisource.sh`
adds three, one of them five seconds wrong, and asks the only question that
matters: does the client converge to the TRUTH?

```
seed   rusty_time                    chrony
701      0.428 ms  PASS               0.002 ms  PASS
702   2996.520 ms  FAIL               0.004 ms  PASS
703   2319.610 ms  FAIL              -0.002 ms  PASS
704    295.950 ms  FAIL               0.002 ms  PASS
705      0.028 ms  PASS              -0.005 ms  PASS
706      1.961 ms  FAIL              -0.003 ms  PASS
707      0.164 ms  PASS              -0.000 ms  PASS
708     -3.875 ms  FAIL               0.001 ms  PASS
```

**Four of eight seeds fail, by as much as three seconds. chrony passes eight of
eight at five microseconds.**

The mechanism, from the per-source logs: each source runs its own controller,
and only the SELECTED one's commands reach the clock — but every controller
books its plan the moment it makes it. The unselected ones therefore accumulate
corrections that never happened, and their estimates drift. Measured at t≈141 s,
two servers that were both keeping perfect time reported offsets **70 ms apart**
with root distances of 0.1 ms. Nothing overlapped, the intersection found no
majority, and with no source selected the daemon simply held its last commanded
rate and walked the clock away at 5100 ppm.

**A fix was attempted and reverted.** Three shapes were tried — reverting
unselected plans, propagating the applied adjustment to every register, and
splitting measurement from control so only the selected source plans. Each
measured WORSE than the baseline (up to 36 s of error), the last because
separating the two halves broke the order the drain must be accounted in. The
tree is back at the 0.1.6 behaviour, and the harness stays as the reproduction.

Shipping a half-finished refactor of the clock loop would be worse than shipping
a failing test that says exactly what is wrong.

### 3. Reference clocks — CLOSED as far as it can be without hardware

`tools/corpus/refclock_loopback.sh` drives the SHM and SOCK inputs from an
independent producer writing the byte layouts gpsd and chrony define. Both are
recovered exactly.

It found a real bug — in the harness. The obvious producer writes twelve
consecutive `int`s; the actual `struct shmTime` has a 64-bit `time_t`, which puts
`clockTimeStampSec` at offset 8 as an i64 with four bytes of padding after the
microseconds field. The consumer correctly reported no valid sample. **That is
the argument for a foreign producer**: a misread segment still yields a number,
and a test that writes its own layout will agree with itself forever.

Stated plainly: this closes *"the code path has never been executed by anything
but itself"*. It does not close *"runs on hardware"*. There is no PPS edge, no
NMEA, no receiver.

### 4. Soak — EXPOSED A MAJOR DEFECT

`tools/corpus/soak.sh` runs a simulated day and compares quarters, with chrony
as the control. A long run without a control cannot tell "this daemon drifts"
from "this scenario drifts".

**The corpus has always run `maxpoll 6` (64 s). The shipped default is
`maxpoll 10` (1024 s).** Over a simulated day on S8:

```
maxpoll        rusty_time (last quarter)      chrony
  6                        22.8 us             4.3 us
 10  (the default)       1452.5 us            10.0 us
```

Our error scales with the poll interval; chrony's does not. That is the
signature of the proportional loop this ledger has documented from the start:
the standing offset is `F_residual * correction_time`, and the correction time
is `CORR_TIME_RATIO * poll`. At a 64 s poll it is microseconds. At 1024 s it is
milliseconds.

**Every accuracy figure in this ledger was measured at a poll ceiling the
daemon does not use by default.** The parity result against chrony holds at
`maxpoll 6` and nowhere else. That is not a small correction to the record; it
is the record being measured in the wrong configuration, and only a long run at
the real default exposed it.

Nothing here is a regression — the code has always behaved this way. What
changed is that it is now measured.

### Which of these run in CI, and why the other two do not

`refclock_loopback.sh` runs in CI, in the `refclocks` job. It passes, it is
fast, and it guards an ABI contract with somebody else's program — exactly what
a standing check is for.

`multisource.sh` and `soak.sh` are **manual**, and deliberately so. Both
currently FAIL, and a permanently red build teaches a team to ignore the colour.
They are the reproductions for two open defects, to be run when someone works on
them and to be added to CI on the commit that fixes them:

```sh
tools/corpus/multisource.sh 600          # 3 servers, one lying
MAXPOLL=10 ARM=rusty_time tools/corpus/soak.sh 86400
MAXPOLL=10 ARM=chrony     tools/corpus/soak.sh 86400   # the control
```

An untended harness rots, so this is a debt, not a plan. It is written down
because the alternative — a test nobody runs and nobody remembers — is how the
maxpoll gap survived eleven releases.
