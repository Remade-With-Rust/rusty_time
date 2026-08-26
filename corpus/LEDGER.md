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

Scenario status: S1, S6, S8 implemented in the deterministic harness.
S2–S5, S7, S9–S14 and HW1: pending (mission plan §7.2).

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
