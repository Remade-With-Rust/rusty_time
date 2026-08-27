# rusty_time-clock

The platform seam: read, slew and step the system clock, and the socket calls a time
daemon needs.

| platform | read | discipline |
|---|---|---|
| Linux | `clock_gettime` | `clock_adjtime` with `ADJ_FREQUENCY` **and `ADJ_TICK`** |
| macOS | `gettimeofday` | `adjtime` |
| Windows | `GetSystemTimePreciseAsFileTime` | `SetSystemTimeAdjustmentPrecise` |

The Linux driver splits a correction across the tick and the frequency knob because the
kernel clamps `ADJ_FREQUENCY` at **500 ppm** (`MAXFREQ`) — far too slow to drain a real
startup offset, and it clamps *silently*, so a loop that trusts the rate it asked for
mis-attributes the shortfall as drift.

Also here: `recv_batch` / `send_batch` (`recvmmsg`/`sendmmsg` on Linux, scalar elsewhere)
and kernel receive timestamps via `SO_TIMESTAMPING`.

This is the one crate in the workspace where `unsafe_code` is allowed, and every `unsafe`
block carries a SAFETY invariant. The arithmetic is factored out into a pure `slew` module
so it is unit-tested on every host, including the targets CI cannot run.

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/main/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
