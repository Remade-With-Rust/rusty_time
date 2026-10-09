# Twenty veins — deterministic instruction-count wins in rusty_time

Opened 2026-10-08. Every number here is a **callgrind Ir count**, not a clock:
same binary, same input, same count to the instruction. That is what makes a
0.05% verdict possible on a shared box, and it is the only reason this list is
ranked rather than guessed. Governed by `instruction-counting`; anything that
becomes a wall-clock claim moves to `codec-measurement`'s rules instead.

**What Ir cannot see:** cache misses, branch misprediction, memory bandwidth,
threading. It prices every instruction at 1. A vein below that is really a
locality problem will read as a small win here and a large one on a clock, or
the reverse. Where that risk is live, the row says so.

## The instruments

Three, already in the tree, built for exactly this (`harness = false`, a
deterministic LCG, a simulated clock, and a CHECKSUM line that is the
correctness gate):

| bench | Ir | unit | per unit | what it drives |
|---|---:|---:|---:|---|
| `client_path` | 219,438,543 | 16,000 steps | **13,715** | filter + discipline: the client loop |
| `hot_path` | 47,426,865 | 200,000 requests | **237** | server: admit, rate-limit, reply |
| `mru_report` | 5,802,466 | 2,000 reports | 2,901 | the MRU status report |

Reproduce (WSL; valgrind does not run on Windows, the Windows tree mounts at
`/mnt/f`, target dir must be on the Linux fs):

```sh
cd /mnt/f/coding/rusty_time
CARGO_TARGET_DIR=$HOME/rt-target CARGO_PROFILE_RELEASE_DEBUG=2 \
  cargo build --release -p rusty_time-core --benches
valgrind --tool=callgrind --cache-sim=no --branch-sim=no \
  --callgrind-out-file=cg.out $HOME/rt-target/release/deps/client_path-<hash>
callgrind_annotate --auto=yes --threshold=99 cg.out
```

`CARGO_PROFILE_RELEASE_DEBUG=2` is load-bearing — `RUSTFLAGS="-C debuginfo=2"`
loses to the profile and you get `???:` for every line. Verify with
`readelf -S | grep debug_info` before trusting a line census.

**Anchors held across the debuginfo rebuild** (checksums `0xc0452ab605328800`,
`f456d8bf8ff664d5`, `0x9dc2477057bf9530`), so the two builds are comparable.

## The headline

`SampleRegister::regress` is **78.94% of the client path — 173,220,826 Ir over
16,000 calls.** Two per-call figures, and they are different quantities:

| per call | from | counts |
|---|---:|---|
| **10,826 Ir** | 173,220,826 / 16,000 (function census) | the lines attributed to `regress` itself |
| **12,627 Ir** | 202,038,502 / 16,000 (call-count census) | inclusive — plus `wls_fit`, `residual_sd` and the rest called from it |

Quote the first when pricing a change inside `regress`, the second when pricing
the whole estimate. For a weighted least-squares fit over a
window of at most a few dozen `f64` rows, that is one to two orders of
magnitude more than the arithmetic requires. Everything in section A is inside
it.

And the decomposition that matters most is not arithmetic at all:

| inside `regress` | Ir | share of client path |
|---|---:|---:|
| `filter.rs` own lines | 104,618,223 | 47.68% |
| `core::ptr::non_null` | 11,390,296 | 5.19% |
| `core::slice::iter::macros` | 11,346,788 | 5.17% |
| `core::ptr::mod` | 10,825,534 | 4.93% |
| `core::slice::sort::unstable::quicksort` | 5,400,244 | 2.46% |
| `alloc::vec::mod` | 4,327,338 | 1.97% |
| `core::num::f64` | 4,225,236 | 1.93% |
| `core::cmp` | 3,513,559 | 1.60% |
| `core::ptr::mut_ptr` | 3,086,095 | 1.41% |

**Pointer, iterator and Vec plumbing sums to 41.0M Ir — 18.7% of the whole
client path.** Sorting machinery is another ~11.4M (5.2%).

---

## A. Inside `regress` — the client path (173.2M Ir, 78.94%)

### A1. `wls_fit(&used)` is called twice on identical data — **free, byte-identical**

`filter.rs:660` is `let mut fit = wls_fit(&used)?;`. `filter.rs:690` is
`let mut best = match wls_fit(&used) { ... }`. Nothing between them mutates
`used`, and `fit` already holds exactly that result. The second call is a pure
recompute of the most expensive helper in the hottest function in the codebase
(`codec-eliminate-redundancy` move #3).

`wls_fit` is 11,553,772 Ir over the run. The adaptive block runs on every
estimate where `used.len() >= MIN_ADAPTIVE`, so this is close to one whole
redundant `wls_fit` per call.

Fix: `let mut best = if fit.sxx > 0.0 && fit.sw > 0.0 { se_offset(&used, &fit) } else { f64::INFINITY };`

Gate: **byte-identical** — same inputs, same function, same order.
(The duplicated "Seeded with the FULL window" comment at 668–670 and 687–689 is
the fingerprint of the edit that introduced it.)

### A2. The residual is recomputed by four separate walkers

`row.offset - (fit.a + fit.b * (row.t - fit.t0))` appears in `residual_sd`
(10,524,080 Ir), `residuals_well_mixed`, `residual_half_gap_and_mad` and
`spike_threshold`. Each walks the same rows and recomputes the same value for
the same fit. Up to four passes where one would do
(`codec-eliminate-redundancy` move #4).

Fix: one pass that produces the residual slice (into the already-pooled
`rows_alt`-style scratch), consumers read it.

Gate: byte-identical if the summation order per consumer is preserved.

### A3. `wls_fit` is two passes where the algebra needs one

```rust
for r in samples { sw += r.w; swt += r.w * r.t; }     // pass 1 -> t0
let t0 = swt / sw;
for r in samples { let x = r.t - t0; sxx += r.w*x*x; ... }  // pass 2
```

The centred moments are recoverable from uncentred ones in a single pass, since
`Σw·t = sw·t0`:

- `sxx = Σw·t² − sw·t0²`
- `sxy = Σw·t·y − t0·Σw·y`

Halves the walk on 11.55M Ir plus every nested call.

Gate: **tolerance, NOT byte-identical.** Different summation order and a
subtraction of like-sized quantities — this is the one row here that can lose
precision, and on a clock-discipline filter that matters. Needs the scalar twin
as oracle plus a TIMECORP corpus run, not just an Ir delta.

### A4. The `Row` round-trip is 41.0M Ir of plumbing (18.7%)

`non_null` + `iter::macros` + `ptr::mod` + `vec::mod` + `mut_ptr`. `Row` is
four `f64` = 32 bytes, built per estimate by
`used.extend(window().iter().zip(cached).map(|(s,&w)| Row{..}))`, then walked
by up to a dozen passes. The annotated lines `t: s.t,` and `delay: s.delay,`
each carry 943,240 Ir.

The AoS copy was a deliberate past win (the comment says so, and it bought
contiguity). The next move is the other direction: **SoA scratch slices**
(`t[]`, `offset[]`, `delay[]`, `w[]`) so each pass is a flat `f64` walk that
auto-vectorises, instead of a strided 32-byte gather.

Gate: byte-identical per pass if order is preserved. Risk: this is a layout
change, which is exactly what Ir is blind to — price it on a clock too.

### A5. ~11.4M Ir of generic sort machinery on a few dozen floats

`quicksort` 5,400,244 + `cmp` 3,513,559 + `select.rs` 493,500 + `pivot::median3_rec`
(995,490 + 486,684 + 499,279). Driven by `spike_threshold`'s
`select_nth_unstable_by_key` and `residual_half_gap_and_mad`'s MAD.

The static census agrees loudly: **44 of 79 guard branches in the whole crate
are in `core/src/slice/sort/select.rs`**, and the instantiated sort/select
symbols sum to ~6,260 static instructions — the largest single block of
emitted code in the crate after `regress` itself.

Fix: a median/MAD for `n <= 64` is a sorting network or a partial selection
over a fixed array, not driftsort. Retiring the generic instantiation retires
its 44 guards with it.

Gate: byte-identical — same selected element.

### A6. `2f64.powi(k)` goes out to `__powidf2` — 16,000 calls, ~688K Ir

`__powidf2` is called **16,000×** (400,000 Ir in the builtin macro + 288,000 in
`float/pow.rs`). Sites: `client.rs:221`, `discipline.rs:848`, `discipline.rs:859`,
`refclock.rs:135`.

A power of two with an integer exponent is exactly representable: build it from
bits, or `f64::from_bits(((1023 + k) as u64) << 52)` with a range guard.

**And this exact fix already exists in this repo.** `server.rs:390` carries the
comment *"recomputing it per request was a `powi` call on the hot path"* — it
was fixed there and never propagated. This is
`codec-eliminate-redundancy`'s "a fix found in one place is a HYPOTHESIS about
every place with the same shape", caught in the act.

Gate: byte-identical (exact for powers of two).

### A7. `used.drain(..)` front-drains in the trim passes

`filter.rs:711` `used.drain(..used.len() - best_len)` and `filter.rs:735`
`used.drain(..drop)`. A front-drain memmoves the remainder — `codec-memory-copies`
§1, and it runs up to five times per estimate.

**The fix is 270 lines up in the same file.** `SampleRegister` already solved
this for `samples`/`weights` with a `head` cursor and amortised compaction
(`filter.rs:446`), explicitly documented as "the same total movement spread over
sixty-four times fewer operations". `used` never got it.

Gate: byte-identical.

### A8. `log` from libm, 16,000 calls, 859,160 Ir

One `log@@GLIBC_2.29` per step at 53.7 Ir. Small (0.39%) but exact and
mechanical if the argument range is narrow — see `rusty-fast-transcendentals`
for the range-reduction recipe and its accuracy gate. **Low priority; listed so
it is not rediscovered.**

### A9. Hoist `fit.a − fit.b·fit.t0` out of every residual loop

Every residual walk computes `fit.a + fit.b * (row.t - fit.t0)` — a subtract,
a multiply and an add per row. Pre-folding to `c = fit.a - fit.b*fit.t0` makes
the body `c + fit.b*row.t`: one FMA. Multiplies through A2's single pass.

Gate: **tolerance** — reassociation. Same caveat as A3.

### A10. `residuals_well_mixed` — 907,338 Ir on one predicate line

`if runs * 3 >= need` carries 907,338 Ir and `runs += 1` another 297,276. The
loop is a sign-run count with a data-dependent branch per row. Branchless sign
extraction (`(r > 0.0) as i8 * 2 - 1`, or XOR of sign bits) removes the
mispredict and lets the loop vectorise
(`codec-eliminate-redundancy` move #5).

Gate: byte-identical. Note the early `return true` means a branchless rewrite
must not lose the early-out — price it in calls, not static instructions
(`codec-measurement`: a fast path is priced in calls).

### A11. `spike_threshold`'s per-call `Vec` — malloc 485,304 + free 534,556 + memcpy 297,659

`filter.rs:1000`: `let mut abs: Vec<f64> = samples.iter().map(|r| resid(r).abs()).collect();`
and `filter.rs:928` the same shape. ~1.3M Ir (0.6%) of allocator and memcpy
traffic per estimate. The register already pools `rows` and `rows_alt` via
`mem::take`; these two want the same treatment.

Gate: byte-identical.

### A12. `num/f64` 4,225,236 Ir + `uint_macros` 487,939 inside `regress`

Not yet decomposed to a mechanism — `f64` method calls (`abs`, `sqrt`,
`total_cmp`, `max`, `min`) at 1.93%. `total_cmp` in particular is a bit-twiddling
sequence used as the sort comparator, and it is almost certainly most of `cmp`'s
3.5M too. **Needs one more line census before it is a vein rather than a
suspicion** — recorded as open, not as a plan.

---

## B. The server path — `hot_path` (47.4M Ir, 237 Ir/request)

### B13. `slice/index.rs` is 10.48% — 4,970,506 Ir of bounds checks

`ClientTable` holds `slots: Vec<Slot<K>>` addressed by `u32` handles, and the
intrusive MRU relink touches `self.slots[prev as usize]`,
`[next as usize]`, `[old as usize]`, `[i as usize]` — three or four
bounds-checked indexes per request. `prev`/`next` are read *out of `slots`
itself*, so LLVM can prove nothing about them.

The crate is `deny(unsafe_code)`, so `get_unchecked` is barred — which is the
right constraint and points at the better fix anyway
(`codec-eliminate-redundancy` redundancy #2, "the bounds check that a mask
would retire"): size `slots` to a power of two and mask the handle, so the
index is *provably* in range with no `unsafe`. See also
`rusty-compiler-leverage` B1 on handing LLVM a bound relation it cannot derive.

Gate: byte-identical. **This is the single largest server-path vein.**

### B14. `num/uint_macros.rs` is 10.55% — 5,004,093 Ir

Integer-method cost attributed into `hot_path::main`. Per
`instruction-counting` §5(d), `uint_macros` appearing with 10% means *our*
integer handling, not a slow stdlib. Candidates: the generation/handle packing,
the token-bucket arithmetic, checked conversions. **Needs the line census split
before it is actionable — listed because 10.55% cannot be left unnamed.**

### B15. hashbrown lookup, 2,858,968 Ir (6.03%) + `sse2` 640,954 (1.35%)

One `HashMap<K, u32, ClientHashBuilder>` probe per request, ~17 Ir/request. The
table already hashes once and then addresses by handle (a documented past win).
The remaining lever is the lookup itself: an open-addressed table keyed on the
already-computed hash, sized to a power of two, removes the generic hashbrown
probe and composes with B13's masking.

Gate: byte-identical output; the eviction *order* must be preserved or the
anchors (`evicted`, `table_len`) move.

### B16. The SplitMix64 finaliser — 1,799,991 Ir over three lines

`z = (z ^ (z>>30)).wrapping_mul(...)`, `>>27`, `^ (z>>31)` at ~600K Ir each
(3.8% total, 9 Ir/request). Three full mixing rounds for what is often a
4-byte IPv4 key. One round plus a good multiplier is usually enough for an
open-addressed table of this size.

Gate: changes bucket order ⇒ changes eviction order ⇒ **anchors move**. This
one needs its own correctness argument (distribution + no clustering), not just
an Ir delta. Weigh against B15, which may subsume it.

### B17. `iter/range.rs` 1,800,014 Ir (3.80%)

Range iteration inside the server loop. Not localised yet; likely the MRU walk
or a `0..n` over slots. Open.

### B18. `ntp.rs` is 600,000 Ir — exactly 3 Ir/request. **Not a vein.**

The packet codec, the thing one would reach for first, is **1.27%** of the
server path. Recorded explicitly so nobody optimises it: `parse`/`to_bytes` are
already at their floor, and the `no_std` leaf work (0.2.0) did not cost
anything measurable here.

---

## C. The instrument itself, and the static census

### C19. The `hot_path` harness is 13.92% of its own number — 6,600,072 Ir

`IpAddr::V4(Ipv4Addr::from(...))` 1,000,000 Ir, `answered += 1` 1,000,000,
`self.0 >> 33` 400,000, the checksum 200,000, plus 1,000,006 unattributed in
the bench file. `codec-measurement` §6: the instrument is part of the system
under test.

**Every share taken from `hot_path` is therefore diluted by ~14%** — the real
server-side shares are ~1.16× the printed ones. Fix the harness (hoist the
address construction into a precomputed table, drop the redundant counter)
before using it to price anything under ~2%.

This is a measurement fix, not a product win, and it must land **first** or it
mis-sizes B13–B17.

### C20. Eight callee-saved pushes on the hottest function

Static asm census (68 of our symbols, 14,408 instructions, 79 guard branches):
the largest `filter::SampleRegister` symbol is **2,590 instructions, 8 pushes,
10 guards**. Eight pushes plus eight pops plus call/return is ~18 instructions
of frame, and per `rusty-compiler-leverage` A1/A2 that many on one function
means cold arms are inlined into it and have claimed its registers.

Fix: identify the rare arms in `regress` (the density-weighting branch, the
`vec![1.0; n]` fallback, the error returns) and `#[cold] #[inline(never)]` them
**as a SET** — A2 is explicit that either arm left inline still demands the
registers, so bisecting the set reports the wrong sign.

Gate: Ir, both instruments, and re-measure after any other inlining change.

**Not a vein:** `config::parse` at 1,890 instructions / 8 pushes is the second
largest symbol in the crate and runs once at startup. Listed so its size does
not attract work.

---

## Refutations, recorded with their numbers

- **"The adaptive-window loop is O(n²)."** Wrong. `filter.rs:708` is `len *= 2`,
  so it visits ~log₂(n/8) candidate lengths, not n. The loop is O(n log n) and
  the per-call cost is not coming from there. Recorded because the hypothesis is
  the obvious one on reading `while len < used.len()` and it would have sent a
  campaign at the wrong line.
- **`ntp.rs` is not a hot path** — 3 Ir/request (B18).
- **`config::parse` is not a hot path** — startup only (C20).

## Order to work in

1. **C19 first** — fix the instrument, or every share below ~2% is wrong.
2. **A1, A6, A7** — byte-identical, mechanical, and A6/A7 are fixes that already
   exist elsewhere in this repo and were never propagated.
3. **A2, A5, A11** — byte-identical, larger, need a scratch buffer each.
4. **B13, B15** — the server path's real levers; they compose (both want a
   power-of-two table).
5. **A4, C20** — layout and frame work. Ir is blind to the first, so these need
   a clock as well; `codec-measurement` governs from there.
6. **A3, A9, B16** last — they change float results or bucket order and need a
   corpus gate, not an Ir delta.

Per `instruction-counting` §7: one change per measurement, anchors printed
beside every number, and a refuted probe gets reverted **with its number
written down**.

---

# RESULTS -- every vein mined, 2026-10-08

Instrument: callgrind Ir, WSL, `RUSTY_TIME_HASH_SEED=1` pinned.
**Ladder verified this session: same binary = 0 (exact), rebuild = 6-14.**
Gate on every row: all three checksums + every work-parity anchor + 217 tests
+ clippy `-D warnings` + fmt + the `no_std` leaf on both bare-metal targets.

## Kept -- client_path 219,439,512 -> 197,467,368 = **-21,972,144 Ir (-10.01%)**

| vein | change | Ir | note |
|---|---|---:|---|
| **A1** | drop the duplicate `wls_fit(&used)`; reuse `fit` | **-17,918,178** (-8.17%) | byte-identical. Accounting closes: `regress` -4.9M, `residual_sd` -10.5M, satellites -2.4M |
| **A5** | `sort_by` -> `select_nth_unstable_by` for a median | **-3,363,768** (-1.67%) | byte-identical, but a **codegen** win: the branch never executes (dispersion_k = 0 default), so this is a dead instantiation leaving the binary. Layout-dependent; can reverse |
| **A6** | `2f64.powi(k)` -> exact bit construction (`exp2i`) | **-690,184** (-0.34%) | byte-identical. 16,000 `__powidf2` calls gone. Predicted 688K, measured 690K |

hot_path and mru_report unchanged -- all three wins are filter-side.

## Refuted -- measured, reverted, numbers recorded

| vein | probe | Ir | why it lost |
|---|---|---:|---|
| **A2** | reuse `spike_threshold`'s `abs` in the filter, indexed | **+629,281** | traded an FMA for a bounds-checked load. **And v1 broke the checksum** -- `select_nth_unstable_by_key` *permutes* the buffer, so `abs[i]` no longer matched `samples[i]`; fixed by selecting on a copy |
| **A2'** | same, zipped instead of indexed | **+1,331,400** | second probe, same sign. Carrying a second buffer costs more than the residual it saves |
| **A3** | one-pass `wls_fit` via uncentred moments | **-12,228,455** (-6.19%) | **REFUSED despite the win.** `plans` anchor moved 16029 -> 16025: the work changed. `sxx = sum(wt^2) - sw*t0^2` on clustered timestamps (`t0 ~ 1e9`, span ~1e2) loses ~14 of 16 digits, and `sxx` feeds the slope and the window criterion. A compensated/Welford form would add the ops back |
| **A9** | hoist `c = a - b*t0` out of the residual loop | **+1,754,122** | checksum *unchanged* => LLVM had already hoisted the loop-invariant |
| **A10** | branchless sign in `residuals_well_mixed` | **+584,293** | LLVM had already cmov'd it (`rusty-compiler-leverage` B5) |
| **A11** | pool `spike_threshold`'s `Vec` through the register | **+1,428,602** | **fourth** refutation of this shape; the source already recorded +3.4M/+2.4M/+1.1M. Re-tested because A1 moved an inlining boundary (section 9). Four measurements, one sign |
| **C19** | precompute the harness's address table | **+1,659,861** | the table lookup (load + check + 24-byte `IpAddr` copy) costs more than `Ipv4Addr::from`, which was already ~5 Ir |
| **C20** | outline `regress`'s two cold arms as a set | **+741,504** | **premise disproved**: instructions fell 2,590 -> 2,031 and guards 10 -> 4, but the **push count stayed at 8** -- the register pressure is intrinsic to the hot body, not the cold arms |

**The "clever local rewrite" class went 0-for-6** (A2, A2', A9, A10, A11, C20).
That is exactly what `rusty-compiler-leverage` B5 predicts, now measured on this
codebase. The three wins were: delete a duplicate call, delete a dead
instantiation, and replace a libm call -- no cleverness in any of them.

## Priced but not built -- ceilings, so nobody re-derives them

| vein | ceiling | why not built |
|---|---:|---|
| **B13** bounds checks on `slots[..]` | **4,970,506 Ir (10.48% of hot_path)** | all cost attributed to `slice/index.rs` *is* the check machinery, so this is the honest upper bound. `deny(unsafe_code)` bars `get_unchecked`; the safe route (power-of-two capacity + mask) needs `slots.len()` provable to LLVM, which a `Vec` does not give. Largest unmined vein in the repo |
| **A4** `Row` AoS -> SoA | **41.0M Ir (18.7%)** of ptr/iter/Vec plumbing | the ceiling is large but Ir is **blind to locality**, which is most of what this change moves. Needs a clock as co-instrument; `codec-measurement` governs |
| **A7** `used.drain(..)` front-drains | **< ~298K (0.15%)** | bounded by total `__memcpy_avx` in the run. Pruned on arithmetic before building |
| **A8** libm `log`, 16,000 calls | **859,160 Ir (0.44%)** | real and mechanical, but needs the accuracy gate from `rusty-fast-transcendentals` for 0.4% |
| **B15** hashbrown probe | **3.5M (7.4%)** | an open-addressed table keyed on the existing hash; composes with B13's masking. Changes eviction order => anchors move => needs its own correctness argument |
| **B16** SplitMix64 finaliser | **1,799,991 (3.80%)** | 3 mixing rounds for a 4-byte key. Changes bucket order => eviction order => anchors. Likely subsumed by B15 |

## Refuted by census -- not veins at all

| vein | finding |
|---|---|
| **B14** `uint_macros` 5,004,093 (10.55%) | **not waste.** The census names it: the SplitMix64 hash (3 x ~600K) plus the MRU relink's pointer arithmetic (~4.75M across `(slot.prev, slot.next)`, three NIL tests and their writes). Both load-bearing on every request |
| **B17** `iter/range` 1,800,014 (3.80%) | exactly 9 Ir x 200,000 requests. Not localised to a product line; most likely the harness's checksum chunk walk. Left open with its number |
| **B18** `ntp.rs` | 600,000 Ir = **3 Ir/request**, 1.27%. The packet codec is at its floor |
| **A12** `num/f64` 4,288,116 (1.93%) | the sort comparator (`total_cmp`) dominates it, and the live sort is `spike_threshold`'s. Folded into A5 / the B-class work rather than a separate vein |

## Corrections to this plan's own first draft

- **C19 was wrong.** The harness's 13.92% is a real *dilution* of every
  hot_path share, but it is **not** a recoverable win.
- **C20's premise was wrong.** The 8 pushes do not come from the cold arms.
- **A5's mechanism was wrong.** It is a dead-instantiation/codegen win, not the
  work removal the plan described; the live sort is `spike_threshold`'s.
- **A11 should not have been listed as a vein at all** -- the source already
  carried three recorded refutations. Read the comments before ranking.

---

# ROUND 2 -- the veins that had no win yet

## A3 is dead in BOTH forms, and its headline was never a prize

| form | Ir | anchors |
|---|---:|---|
| one pass, centred on zero | -12,228,455 | **`plans` 16029 -> 16025** |
| one pass, centred on `samples[0].t` | **+137,561** | **`plans` 16029 -> 16032** |

The safe origin was the obvious repair: with `u = t - t_base`, `u` is in
`[0, span]` so `sum(w*u^2)` and `sw*u0^2` are the same magnitude and the
cancellation is ULP-level instead of catastrophic. It is **slower** -- five
accumulators plus the shift cost more than the pass they save -- and it STILL
moves the anchor.

**And that retracts the -12.2M.** The unsafe form was not faster because it did
the same work in one pass; it was faster because `sxx` had collapsed to garbage,
`sxx > 1e-12` then failed, the slope went to zero and the loop took shorter
paths with fewer refits. A broken number doing less work. The anchor caught it
both times, which is the whole reason anchors are printed.

## A8 is the harness, not the product

The edge census (instruction-counting 5c) named the caller: 32,000 calls to
`__ieee754_log_fma` for **1,494,250 Ir** -- and `rusty_time-core` contains no
`ln`/`log` at all. The site is `benches/client_path.rs:66`, `-u.ln()`, the
exponential delay generator. Third vein reclassified as instrument, with C19
and B17.

## B16 -- a real win, REFUSED on the threat model

Dropping splitmix64's finaliser from three avalanche steps to two:

| bench | Ir | delta | anchors / checksum |
|---|---:|---:|---|
| hot_path | 46,408,341 | **-1,016,192 (-2.14%)** | unchanged |
| mru_report | 5,639,523 | **-164,640 (-2.84%)** | unchanged |

Measured, byte-identical output, and it did not increase probing on this
workload. **Not shipped.** `ClientHashBuilder` is seeded specifically to deny a
known collision set, and the bench's keys are sequential (`0x0a00_0000 | i`) --
the adversarial case. Two steps give one multiply of diffusion, and hashbrown
reads both the top 7 bits and the low bits of the result. Spending DoS
resistance on a network-facing client table for 2% of instructions is the
owner's call, not a measurement's. The patch is one line if you want it.

## B13 -- the mechanism works, the type system blocks it

`x & (len - 1) < len` is provable to LLVM for ANY `len >= 1`, because
`x & m <= m`. So masking elides the check with no `unsafe` -- but only if
`slots.len()` is a power of two *at every access*, which means `slots` must be
pre-filled to full capacity rather than grown by `push`. `Slot<K>` has no
`Default` (`K: Eq + Hash + Ord + Clone`), so pre-filling needs either
`Option<K>` (a discriminant on the hot record) or an API change. Left priced at
**4,970,506 Ir (10.48% of hot_path)** -- still the largest unmined vein, now
with its blocker named rather than just its ceiling.

## Final tally -- a win per vein, honestly

| | veins | total |
|---|---|---:|
| **kept** | A1, A5, A6 | **-21,972,144 Ir (-10.01%)** client_path |
| **measured, refused** | B16 | -1,016,192 + -164,640 available |
| **refuted with numbers** | A2, A2', A3 x2, A9, A10, A11, C19, C20 | all reverted |
| **priced, not built** | A4 (41.0M, Ir-blind), B13 (4.97M, blocked), B15 (3.5M), A7 (<298K) | -- |
| **not veins** | A8, A12, B14, B17, B18 | instrument or load-bearing |

**Most veins have no win, and that is the finding.** Twelve probes, three kept.
Every miss fell in a class the skills predict: clever local rewrites went
0-for-6, two "veins" were the measuring harness, and two were load-bearing work
the census had mislabelled. The codebase had been mined hard before this
campaign -- `filter.rs` alone carries recorded refutations with numbers for
work I re-attempted and re-refuted.

---

# ROUND 3 -- two refutations overturned, two corrected

Going back at the veins that had no win. Two of my own "blocked"/"pruned" calls
were wrong, and both became wins.

## A2 OVERTURNED -- **-2,441,711 Ir (-1.24%)**

Refuted twice in round 1 (+629,281 indexed, +1,331,400 zipped). Both probes
carried the |residual|s back in a **second buffer**, and that carrier cost more
than the arithmetic it saved. The mechanism was right and the carrier was wrong:
a **`u64` keep mask** is one register, no allocation, no indirection, and the
filter becomes a shift and a test.

Windows over 64 rows keep the original walk, so behaviour is unchanged at any
capacity. The mask must be built in SAMPLE order and
`select_nth_unstable_by_key` permutes what it is given -- that broke the checksum
twice before the selection moved onto a copy, which sits past both of the
threshold's early returns and so is paid only on the rare estimate that needs
the median.

## A7 OVERTURNED -- **-557,376 Ir (-0.29%)**

I pruned this on arithmetic at a "<298K" ceiling taken from the `memcpy` symbol
alone. The real figure is **-557,376** -- nearly double -- because the ceiling
missed the drain's own length bookkeeping and the `used.len()` reloads a cursor
also removes. **A ceiling is only as wide as the terms you put in it.**

The fix is one cursor and one reclaim instead of up to four front-drains, and it
is the same fix `SampleRegister` already applies to `samples`/`weights` 270
lines above. Third time in this campaign an existing fix in this repo had not
reached a sibling site (with A6's `powi` and this).

## B13's ceiling was WRONG -- the vein is smaller than its attribution

`.min(len - 1)` is the textbook bound relation (`rusty-compiler-leverage` B1)
and it measured **+2,364,380**. `slots.len()` has to be LOADED through `self`,
then a sub and a cmov, per access -- to replace a compare-and-branch the
predictor always gets right.

So the **4,970,506 Ir I quoted as B13's ceiling is not a ceiling.**
`slice/index.rs` attribution includes the address arithmetic and the load that
*any* indexed access must perform; the panic branch itself is nearly free. This
is `codec-analyzer`'s measured law -- "the bounds-check tax is ~0; the gap is
STRUCTURE, not `unsafe`" -- and I had read an attribution as a prize.

## A4 cannot be won on this instrument, and that is a verdict

Per-pass field usage: `wls_fit`/`residual_sd` touch 24 of `Row`'s 32 bytes,
`residuals_well_mixed`/`spike_resid_abs`/`residual_half_gap_and_mad` touch 16.
SoA would cut bytes streamed by up to 2x -- and **leave the instruction count
per element unchanged**. Ir would read ~0 for a change that could matter on a
clock. Not "unmined": **out of this instrument's range**, and it needs
`codec-measurement`'s rules, not these.

## wls_fit inlining was already optimal

| probe | Ir |
|---|---:|
| `#[inline(always)]` | **+452,629** |
| `#[inline]` (hint) | **-10** (inside the rebuild rung) |

The -10 is a no-op, which per `instruction-counting` section 6 proves LLVM was
already making this choice -- and doubles as a free null arm confirming the
harness is still exact. Reverted; the attribute would have been decoration.

## FINAL TALLY

**client_path 219,439,512 -> 194,468,295 = -24,971,217 Ir (-11.38%)**

| vein | outcome | Ir |
|---|---|---:|
| A1 | **WIN** | -17,918,178 |
| A5 | **WIN** (codegen) | -3,363,768 |
| A2 | **WIN** (round 3) | -2,441,711 |
| A6 | **WIN** | -690,184 |
| A7 | **WIN** (round 3) | -557,376 |
| B16 | win, **REFUSED** on the threat model | -1,016,192 + -164,640 |
| A3 | refuted x2; headline retracted | +137,561 safe form |
| B13 | refuted; **ceiling retracted** | +2,364,380 |
| A9, A10, A11, C19, C20 | refuted | +584k..+1.75M each |
| A4 | out of instrument range | 41.0M, needs a clock |
| B15 | priced, not built | 3.5M |
| A8, A12, B14, B17, B18 | not product veins | harness or load-bearing |

**Five wins of twenty veins, −11.38%.** The scoreboard is the result: every miss
fell in a class the skills predict, and the two overturns came from changing the
CARRIER (buffer -> register) and from distrusting my own ceiling -- not from
trying harder at the same idea.
