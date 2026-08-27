#!/bin/bash
# Instruction-count harness for the CLIENT discipline path (corpus S6 shape).
#
# The sibling of tools/perf/ir.sh, which measures the server. Same discipline
# and the same reason for it: the wins here are individually far under 1% of
# any wall clock, and at that size a clock cannot be promoted to the verdict
# however many pairs you run. callgrind's Ir count is exact and reproducible,
# so a win is a number that went down.
#
# The gate is the workload's CHECKSUM, taken over every plan the controller
# emits, folded by exact f64 BITS. This path is floating point, so the gate is
# bit-identity rather than a tolerance: an instruction-count change must not
# move the arithmetic at all. Anything meant to change the numbers belongs in
# the corpus harness behind a paired test, not here.
#
# Usage:
#   tools/perf/client_ir.sh                 # measure, print Ir + top functions
#   tools/perf/client_ir.sh --save NAME     # record a baseline
#   tools/perf/client_ir.sh --vs NAME       # diff against a baseline

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

# Build the BINARY with symbols and take the freshest. Measuring a stale binary
# is the classic way to report on code that no longer exists.
CARGO_PROFILE_RELEASE_DEBUG=true CARGO_TARGET_DIR="$TARGET" \
    cargo build --release --bench client_path 2>&1 | grep -E '^(error|warning: unused)' -A5
bin=$(ls -t "$TARGET"/release/deps/client_path-* 2>/dev/null | grep -v '\.d$' | head -1)
[ -x "$bin" ] || { echo "no client_path binary" >&2; exit 1; }
echo "binary: $bin ($(date -r "$bin" +%H:%M:%S))"

cg="$OUT/cg-client.out"
rm -f "$cg"
run_out=$(valgrind --tool=callgrind --cache-sim=no --branch-sim=no \
    --callgrind-out-file="$cg" "$bin" 2>/dev/null)
echo "$run_out"

ir=$(grep '^summary:' "$cg" | awk '{print $2}')
checksum=$(echo "$run_out" | awk '/CHECKSUM/{print $2}')
steps=$(echo "$run_out" | awk '/^steps/{print $2}')
echo
echo "TOTAL Ir     $ir"
echo "steps        $steps"
[ -n "${steps:-}" ] && [ "$steps" -gt 0 ] && \
    echo "Ir/step      $(awk -v a="$ir" -v b="$steps" 'BEGIN{printf "%.1f", a/b}')"

echo
echo "top functions by self Ir:"
callgrind_annotate --threshold=92 "$cg" 2>/dev/null \
    | awk '/Ir  *file:function/{hit=1; next} hit && /^-+$/{next} hit && NF==0{exit} hit' \
    | head -22 | sed 's# \[/[^]]*\]##; s#/rustc/[a-f0-9]*/library/#std:#'

case "$mode" in
    save)
        printf '%s %s %s\n' "$ir" "$checksum" "$steps" > "$OUT/cbase-$name"
        cp "$cg" "$OUT/cg-client-$name.out"
        echo; echo "saved baseline '$name': Ir=$ir checksum=$checksum"
        ;;
    vs)
        [ -f "$OUT/cbase-$name" ] || { echo "no baseline '$name'" >&2; exit 1; }
        read -r base_ir base_sum base_steps < "$OUT/cbase-$name"
        echo
        echo "=== vs baseline '$name' ==="
        if [ "$checksum" != "$base_sum" ]; then
            echo "GATE FAILED: checksum $base_sum -> $checksum"
            echo "The emitted plans changed. This harness measures instruction"
            echo "count at FIXED behaviour; a numeric change is not a win here."
            exit 1
        fi
        echo "gate         checksum unchanged ($checksum)"
        [ "$steps" = "$base_steps" ] || echo "WARNING: work count moved ($base_steps -> $steps) -- arms are not comparable"
        awk -v a="$base_ir" -v b="$ir" 'BEGIN{
            d=a-b; printf "Ir           %d -> %d  (%+d, %+.2f%%)\n", a, b, -d, -100.0*d/a }'
        ;;
esac
