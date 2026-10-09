#!/bin/bash
# Instruction-count harness for the NTP CODEC (benches/codec.rs).
#
# The third instrument, and the only one that prices `ntp.rs` on its own.
# `hot_path` reads the codec as 3 Ir/request, but there `parse`/`write` are
# inlined into `hot_path::main` and half the events are unattributed, so that
# figure cannot support a floor claim. It also measures the wrong deployment:
# `ntp.rs` is the `no_std` leaf, and on a Cortex-M4F, an RV32 part or in the
# wasm client it is ~all of the work rather than 1.27% of it.
#
# `instruction-counting` §4: two instruments minimum, with DIFFERENT corpus
# shapes, because roughly a third of probes move two of them in opposite
# directions. This is the third shape -- bytes in, bytes out, no table, no
# filter, no clock.
#
# The gate is the workload's CHECKSUM over every parsed field and every emitted
# byte. Integer/exact path, so the bar is byte-identity.
#
# Usage:
#   tools/perf/codec_ir.sh                 # measure, print Ir + top functions
#   tools/perf/codec_ir.sh --save NAME     # record a baseline
#   tools/perf/codec_ir.sh --vs NAME       # diff against a baseline

set -u
export PATH="$HOME/.cargo/bin:$PATH"
TARGET=${CARGO_TARGET_DIR:-$HOME/rt-target}
OUT=${OUT:-$HOME/rt-perf}
mkdir -p "$OUT"

mode=""; name=""
case "${1:-}" in
    --save) mode=save; name=${2:?--save needs a name} ;;
    --vs)   mode=vs;   name=${2:?--vs needs a name} ;;
    "")     mode=show ;;
    *)      echo "unknown option $1" >&2; exit 2 ;;
esac

# Build the BINARY with symbols. `touch` is load-bearing on this rig: cargo's
# fingerprint across the /mnt drvfs mount does not reliably see an edit, and a
# stale binary measures code that no longer exists (caught live once -- a run
# read the pre-fix Ir with the fix sitting in the tree).
touch crates/rusty_time-core/benches/codec.rs crates/rusty_time-core/src/ntp.rs 2>/dev/null
CARGO_PROFILE_RELEASE_DEBUG=2 CARGO_TARGET_DIR="$TARGET" \
    cargo build --release --bench codec 2>&1 | grep -E '^(error|warning: unused)' -A5
bin=$(ls -t "$TARGET"/release/deps/codec-* 2>/dev/null | grep -v '\.d$' | head -1)
[ -x "$bin" ] || { echo "no codec binary" >&2; exit 1; }
echo "binary: $bin ($(date -r "$bin" +%H:%M:%S))"

cg="$OUT/cg-codec.out"
rm -f "$cg"
run_out=$(valgrind --tool=callgrind --cache-sim=no --branch-sim=no \
    --callgrind-out-file="$cg" "$bin" 2>/dev/null)
echo "$run_out"

ir=$(grep '^summary:' "$cg" | awk '{print $2}')
checksum=$(echo "$run_out" | awk '/CHECKSUM/{print $2}')
parsed=$(echo "$run_out" | awk '/^parsed/{print $2}')
written=$(echo "$run_out" | awk '/^written/{print $2}')
fields=$(echo "$run_out" | awk '/^fields/{print $2}')
echo
echo "TOTAL Ir     $ir"
echo "parsed       $parsed"
[ -n "${parsed:-}" ] && [ "$parsed" -gt 0 ] && \
    echo "Ir/packet    $(awk -v a="$ir" -v b="$parsed" 'BEGIN{printf "%.2f", a/b}')"

echo
echo "top functions by self Ir:"
callgrind_annotate --threshold=90 "$cg" 2>/dev/null \
    | awk '/Ir  *file:function/{hit=1; next} hit && /^-+$/{next} hit && NF==0{exit} hit' \
    | head -16 | sed 's# \[/[^]]*\]##; s#/rustc/[a-f0-9]*/library/#std:#'

case "$mode" in
    save)
        printf '%s %s %s %s %s\n' "$ir" "$checksum" "$parsed" "$written" "$fields" \
            > "$OUT/kbase-$name"
        cp "$cg" "$OUT/cg-codec-$name.out"
        echo; echo "saved baseline '$name': Ir=$ir checksum=$checksum"
        ;;
    vs)
        [ -f "$OUT/kbase-$name" ] || { echo "no baseline '$name'" >&2; exit 1; }
        read -r b_ir b_sum b_p b_w b_f < "$OUT/kbase-$name"
        echo
        echo "=== vs baseline '$name' ==="
        if [ "$checksum" != "$b_sum" ]; then
            echo "GATE FAILED: checksum $b_sum -> $checksum"
            echo "The codec's output changed. This harness measures instruction"
            echo "count at FIXED behaviour; a numeric change is not a win here."
            exit 1
        fi
        echo "gate         checksum unchanged ($checksum)"
        for pair in "parsed:$b_p:$parsed" "written:$b_w:$written" "fields:$b_f:$fields"; do
            k=${pair%%:*}; rest=${pair#*:}; o=${rest%%:*}; n=${rest#*:}
            [ "$o" = "$n" ] || echo "WORK PARITY BROKEN: $k $o -> $n -- arms not comparable"
        done
        echo "work parity  parsed/written/fields unchanged ($b_p/$b_w/$b_f)"
        awk -v a="$b_ir" -v b="$ir" 'BEGIN{
            d=a-b; printf "Ir           %d -> %d  (%+d, %+.4f%%)\n", a, b, -d, -100.0*d/a }'
        ;;
esac
