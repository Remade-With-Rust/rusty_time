#!/bin/bash
# One (scenario, arm, seed) cell of the steady-state accuracy sweep.
#
# Usage: accuracy_cell.sh <scenario> <arm> <seed> <duration> [rt-extra-args...]
# Prints: <scenario> <arm-label> <seed> <signed-mean-us> <mean-abs-us> <n> <poll-s>
#
# `poll-s` is the mean interval between packets ARRIVING AT THE SERVER, taken
# from clknetsim's own statistics rather than from either implementation's
# opinion of itself. It is the sample count, and the sample count is most of
# the accuracy: offset error falls as 1/sqrt(N), so an arm that polls 18% more
# often is 8.7% more accurate before its estimator does anything at all.
#
# Report it beside every error figure. A comparison of two time daemons that
# does not state how many packets each one spent is not a comparison.
#
# Two things make this parallel-safe, and both are worth stating because they
# are the reason a 20-seed sweep takes twenty minutes instead of two hours:
#
#   * Each cell gets its OWN work directory. bench_vs_chrony.sh begins with
#     `rm -rf "$WORK"`, so cells sharing one would delete each other's logs.
#
#   * clknetsim runs in VIRTUAL time. A cell's output depends on its seed and
#     its config, never on how contended the box is, so running twenty at once
#     changes nothing about the numbers. This is the opposite of a wall-clock
#     benchmark, where concurrency is the enemy -- here it is free.
#
# The reported window is the last quarter of the run: steady state, after any
# convergence transient. Both the signed mean (standing bias) and the mean
# absolute error (what a user actually experiences) are printed, because they
# answer different questions and a loop can be good at one and bad at the other.

set -u
# Optional leading `--label NAME`, so a job list stays one line of plain words
# and can be fed straight to `xargs -L1`.
explicit_label=""
if [ "${1:-}" = "--label" ]; then
    explicit_label=$2
    shift 2
fi

scen=$1; arm=$2; seed=$3; dur=$4; shift 4
label=$arm
extra=""
if [ $# -gt 0 ]; then
    extra="$*"
    # Label the arm by the digits in its flags. That is enough for a numeric
    # sweep and silently WRONG for a boolean one: two arms differing only by a
    # flag with no digits collapse to the same label, and the paired test then
    # compares an arm against itself and reports it as a tie. Pass --label for
    # those.
    label="$arm$(echo "$extra" | tr -cd '0-9.')"
fi
[ -n "$explicit_label" ] && label="$explicit_label"

export PATH="$HOME/.cargo/bin:$PATH"
repo=$(cd "$(dirname "$0")/../.." && pwd)
W="${SWEEP_ROOT:-$HOME/sweep}/w-$scen-$label-$seed"

WORK="$W" RT_EXTRA="$extra" SEED_BASE=$((seed - 1)) SCENARIOS="$scen" \
    DURATION="$dur" REPS=1 ARMS="$arm" SKIP_BUILD=1 \
    bash "$repo/tools/corpus/bench_vs_chrony.sh" >/dev/null 2>&1

# Node 1 is the server and prints first, so the first match is packets arriving
# from the client under test.
poll=$(awk '/Mean incoming packet interval:/ { print $NF; exit }' \
    "$W/tmp/stats" 2>/dev/null)
poll=${poll:-0}

awk -v s="$seed" -v a="$label" -v sc="$scen" -v cut="$((dur * 3 / 4))" -v p="$poll" '
    NR > cut { t += $2; u += ($2 < 0 ? -$2 : $2); n++ }
    END { if (n) printf "%s %s %s %.4f %.4f %d %.2f\n", sc, a, s, t/n*1e6, u/n*1e6, n, p }
' "$W/offsets-$scen-$arm"

rm -rf "$W"
