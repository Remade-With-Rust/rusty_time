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
