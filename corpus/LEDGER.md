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

ntpd-rs as a second client implementation: **not yet run** (pending).

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
