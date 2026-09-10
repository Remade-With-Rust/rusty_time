# rusty_time-core on an ESP32-S3 — the on-board proof

"Builds for a bare-metal target" and "runs on the part" are different claims.
CI makes the first one every push (`cargo check -p rusty_time-core
--no-default-features` on `thumbv7em-none-eabihf` and
`riscv32imac-unknown-none-elf`). This directory is the second one, and it is
hand-run: it needs Espressif's Rust fork, which CI does not have.

**Result, 2026-09-09, ESP32-S3 revision v0.2, 8 MB flash, no heap:**

```
=== rusty_time-core leaf on ESP32-S3 (xtensa, no_std, NO alloc) ===
header 48 bytes, no heap linked, no clock read

[1] client request                    5 checks   ok
[2] server response (literal image)  14 checks   ok
      root delay 0.0149993896484375 s, dispersion 0.0019989013671875 s
[3] offset / delay (RFC 5905 §8)      5 checks   ok
      offset 0.09999999997671694 s   delay 0.05000000004656613 s
      1 ns step measured as 0.0000000009313225746154785 s
      2 s across the 2036 era wrap measured as 2 s
[4] extension fields and rejections    6 checks   ok

checks passed 30 / 30
RESULT: PASS -- the rusty_time-core leaf ran on the board
```

Thirty checks because the leaf has four separable jobs and each is worth
failing on its own: build a request, parse a response off the wire, do the
offset arithmetic, and refuse malformed input without panicking.

## Why this part, and what makes it a real test

**No heap is linked at all.** The `rusty_zstd` board row next door needs
`esp-alloc` because a codec needs buffers; this firmware's `.cargo/config.toml`
says `build-std = ["core"]` with no `alloc`, because the leaf allocates nothing.
That is a stronger statement than "it compiles `no_std`" and it is the property
`rusty_rtos_sntp` actually depends on.

**The response is a literal wire image, not our own output.** `SERVER_RESPONSE`
in `src/main.rs` is 48 hand-written bytes. Parsing something `to_bytes` produced
would only prove the codec is self-consistent; parsing a byte image pins the RFC
5905 field offsets and big-endian order independently of the writer. The
`origin`/`receive`/`transmit` timestamps use distinguishable patterns
(`0x3333…`, `0x5555…`, `0x7777…`) precisely so a field-order slip cannot pass.

**The arithmetic is soft-float here.** Xtensa LX7 has a single-precision FPU and
no `f64` unit, so every division in `offset_delay` and `seconds_since` is a
compiler-generated soft-float call. That is the half a host test cannot stand in
for, and it is why the era-wrap and sub-nanosecond cases run on the board rather
than only in `cargo test`.

**Two numbers that look wrong and are not.** The 1 ns step reads 0.931 ns
because `NtpTimestamp::from_unix` truncates nanoseconds into the 32-bit NTP
fraction — `(1 << 32) / 1e9` is 4.29, stored as 4, i.e. 4/2³² s. That is one
tick of quantization, which is why the check's tolerance is one tick and not
something tighter. The root delay reads 0.0149993… rather than 0.015 for the
same reason in 16.16 fixed point. Both are the wire format's resolution showing
through, not error.

## Running it

```sh
espup install                 # once: the `esp` Rust fork for Xtensa
cargo install espflash        # once
cd bare-metal/esp32s3
cargo run --release -- --port COM4    # your port; omit on Linux/macOS to autodetect
```

Passes when the last line reads `RESULT: PASS`. It is not in the workspace
(`exclude` in the root manifest), so a normal `cargo build` at the repo root
never sees it and never needs the Xtensa toolchain.

To capture the output from a non-interactive shell, redirect to a file —
`espflash --monitor` never exits, so a pipe through `timeout` loses everything:

```sh
timeout 150 espflash flash --monitor --port COM4 --non-interactive \
  target/xtensa-esp32s3-none-elf/release/s3time > run.log 2>&1
sed -n '/=== rusty_time-core leaf/,$p' run.log
```

## What it does not claim

- **No timing.** There is no cycle count here, so nothing in this directory is a
  performance claim. It answers "does it work", not "how fast".
- **No network.** The leaf has no socket by design. This exercises the codec and
  the arithmetic against a wire image, not a real server exchange — the
  transport belongs to whoever wraps the leaf.
- **One part.** The S3 is Xtensa. The two targets CI compiles are ARM
  (`thumbv7em-none-eabihf`) and RISC-V (`riscv32imac-unknown-none-elf`), and
  neither has been run on silicon here.
