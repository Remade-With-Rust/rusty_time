# rusty_time

chrony, remade with Rust. A pure-Rust, memory-safe NTP/NTS client + server targeting
Linux, macOS, Windows, and wasm.

- Mission plan: [docs/plans/time_mission.md](docs/plans/time_mission.md)
- Performance ledger: [corpus/LEDGER.md](corpus/LEDGER.md)

**Claims discipline:** this README makes no performance claim that is not in the ledger
with the run that produced it. The corpus (TIMECORP) measures rusty_time against chrony
(primary baseline) and ntpd-rs (secondary); see the mission plan §7.

## Status

Pre-1.0, milestone **M4** of the mission plan complete: NTPv4 codec, regression
sample filter, source selection, discipline loop, the deterministic TIMECORP
harness, **NTS (RFC 8915) client and server** with SpaceDB-backed persistence,
and a hardened server — per-client and global rate limiting with Kiss-o'-Death,
**interleaved mode** (RFC 9769), `recvmmsg` batching on Linux, and an ops
control plane driven by `rtimec`.

Interop is verified against chrony in both directions and both modes
(see [corpus/LEDGER.md](corpus/LEDGER.md)): chronyd selects an `rtimed` server
as its synchronisation source over plain NTP, over NTS, and in interleaved
mode, where it measures a **−66 ns** offset. TLS is rustls on a pure-Rust
crypto provider; nothing in the build needs a C toolchain, on any of the eight
supported targets.

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
