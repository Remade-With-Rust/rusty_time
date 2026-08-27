#!/bin/bash
# The instruction-count harness. Everything that claims a win here calls this.
#
# Why a counter and not a clock: the wins in this area are individually well
# under 1% of any wall time, and at that size the clock cannot be promoted to
# the verdict however many pairs you run -- you either discard real work
# removal because a noisy box hid it, or bank a regression because a noisy box
# flattered it. callgrind's Ir count is exact and reproducible, so a win is
# simply a number that went down.
#
# It also prints the workload's CHECKSUM. This is an integer/exact path, so the
# correctness gate is byte-identity: the checksum must not move. A number
# without its gate is not evidence.
#
# Usage:
#   tools/perf/ir.sh                 # measure, print Ir + top functions
#   tools/perf/ir.sh --save NAME     # measure and record as a baseline
#   tools/perf/ir.sh --vs NAME       # measure and diff against a baseline

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

# Build the BINARY, with symbols, and find the freshest one. A stale binary is
# the classic way to measure code that no longer exists -- three identical
# results in a row means "is this even rebuilding?".
CARGO_PROFILE_RELEASE_DEBUG=true CARGO_TARGET_DIR="$TARGET" \
    cargo build --release --bench hot_path 2>&1 | grep -E '^(error|warning: unused)' -A5
bin=$(ls -t "$TARGET"/release/deps/hot_path-* 2>/dev/null | grep -v '\.d$' | head -1)
[ -x "$bin" ] || { echo "no hot_path binary" >&2; exit 1; }
echo "binary: $bin ($(date -r "$bin" +%H:%M:%S))"

# Pin the client table's hash seed. Without it the seed is OS-random per run,
# which shifts probe counts and makes Ir reproducible only to ~0.002%. The
# effects being measured are far larger than that, but an exact instrument
# beats a nearly-exact one, and a harness that drifts is a harness nobody can
# audit later.
export RUSTY_TIME_HASH_SEED=${RUSTY_TIME_HASH_SEED:-1}

cg="$OUT/cg.out"
rm -f "$cg"
# --cache-sim=no: we want instructions retired, not a cache model. The cache
# model is a different (and much slower) question, and mixing them invites
# reading a cache effect as an instruction saving.
run_out=$(valgrind --tool=callgrind --cache-sim=no --branch-sim=no \
    --callgrind-out-file="$cg" "$bin" 2>/dev/null)
echo "$run_out"

ir=$(grep '^summary:' "$cg" | awk '{print $2}')
checksum=$(echo "$run_out" | awk '/CHECKSUM/{print $2}')
answered=$(echo "$run_out" | awk '/^answered/{print $2}')
echo
echo "TOTAL Ir     $ir"
echo "answered     $answered"
[ -n "${answered:-}" ] && [ "$answered" -gt 0 ] && \
    echo "Ir/request   $(awk -v a="$ir" -v b="$answered" 'BEGIN{printf "%.1f", a/b}')"

echo
echo "top functions by self Ir:"
callgrind_annotate --threshold=90 "$cg" 2>/dev/null \
    | sed -n '/Ir *file:function/,/^--/p' | head -26

case "$mode" in
    save)
        printf '%s %s %s\n' "$ir" "$checksum" "$answered" > "$OUT/base-$name"
        cp "$cg" "$OUT/cg-$name.out"
        echo; echo "saved baseline '$name': Ir=$ir checksum=$checksum"
        ;;
    vs)
        [ -f "$OUT/base-$name" ] || { echo "no baseline '$name'" >&2; exit 1; }
        read -r base_ir base_sum base_ans < "$OUT/base-$name"
        echo
        echo "=== vs baseline '$name' ==="
        if [ "$checksum" != "$base_sum" ]; then
            echo "GATE FAILED: checksum $base_sum -> $checksum"
            echo "This path is integer/exact; output MUST be byte-identical."
            exit 1
        fi
        echo "gate         checksum unchanged ($checksum)"
        [ "$answered" = "$base_ans" ] || echo "WARNING: work count moved ($base_ans -> $answered) -- arms are not comparable"
        awk -v a="$base_ir" -v b="$ir" 'BEGIN{
            d=a-b; printf "Ir           %d -> %d  (%+d, %+.2f%%)\n", a, b, -d, -100.0*d/a }'
        ;;
esac
