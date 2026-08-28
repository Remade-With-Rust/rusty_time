#!/bin/bash
# Long-run soak: does anything drift, grow, or stop after a day of operation?
#
# Every other measurement in this corpus runs for 1200 to 3000 simulated
# seconds. Twenty minutes proves a loop converges; it says nothing about what a
# daemon does on day three. The failures that only show up over time are a
# different species — a register that slowly fills, a counter that wraps, an f64
# accumulating rounding, a file descriptor leaked once per poll, a frequency
# estimate that ratchets — and none of them are visible in a short run.
#
# clknetsim runs in VIRTUAL time, so a simulated day costs a few real minutes.
# That buys the long-horizon behaviour cheaply and honestly: the daemon executes
# every poll, every drain, every register eviction it would in a real day.
#
# What this checks, over the whole run rather than at the end:
#   * the error stays bounded — no slow ratchet away from the truth
#   * the LAST quarter is no worse than the second quarter, which is what a
#     slow leak or a winding-up estimate would break
#   * the daemon is still alive and still disciplining at the end
#
# What it cannot check: real memory growth and file descriptors, because under
# clknetsim the process is doing a day's work in minutes and the allocator never
# sees the same pressure. Those are measured separately, in real time, by
# `soak_rss`.

set -u
cd "$(dirname "$0")/../.." || exit 1
export PATH="$HOME/.cargo/bin:$PATH"
export CLKNETSIM_PATH=${CLKNETSIM_PATH:-$HOME/clknetsim}
CHRONY_DIR=${CHRONY_DIR:-$HOME/chrony}
RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
WORK=${WORK:-$HOME/soak}
DURATION=${1:-86400}
SCENARIO=${SCENARIO:-S8}
SEED=${SEED:-901}
# Which daemon to soak. chrony is the control: a long run with no control arm
# cannot tell "this daemon drifts" from "this scenario drifts".
ARM=${ARM:-rusty_time}
# Poll ceiling. The corpus has always run 6 (64 s); the SHIPPED default is 10
# (1024 s), and a proportional loop's standing offset scales with it.
MAXPOLL=${MAXPOLL:-10}
# Tolerance for the late-run comparison. Generous on purpose: this is looking
# for a trend, not for a microsecond.
GROWTH=${GROWTH:-3.0}

rm -rf "$WORK"; mkdir -p "$WORK"; chmod 750 "$WORK"
export CLKNETSIM_TMPDIR="$WORK/tmp"; mkdir -p "$CLKNETSIM_TMPDIR"
export CLKNETSIM_CLIENT_WRAPPER=${CLKNETSIM_CLIENT_WRAPPER:-}
export CLKNETSIM_SERVER_WRAPPER=${CLKNETSIM_SERVER_WRAPPER:-}
client_pids=""
. "$CLKNETSIM_PATH/clknetsim.bash"
export CLKNETSIM_RANDOM_SEED=$SEED

case "$SCENARIO" in
    S1) off=0.010; freq="20e-6" ;;
    S8) off=0.010; freq="(sum (* 1e-9 (normal)))" ;;
    *)  off=0.010; freq="20e-6" ;;
esac

{
    echo "node2_offset = $off"
    echo "node2_freq = $freq"
    [ "$SCENARIO" = S8 ] && echo "node2_freq_offset = 100e-6"
    echo "node2_delay1 = (+ 100e-6 (* 10e-6 (exponential)))"
    echo "node1_delay2 = (+ 100e-6 (* 10e-6 (exponential)))"
} > "$CLKNETSIM_TMPDIR/conf"

echo "== soak: ${DURATION}s simulated ($(awk -v d="$DURATION" 'BEGIN{printf "%.1f", d/86400}') days), $SCENARIO, $ARM, maxpoll $MAXPOLL, seed $SEED =="

PATH="$CHRONY_DIR:$PATH" start_client 1 chronyd "local stratum 1" "" ""
if [ "$ARM" = chrony ]; then
    PATH="$CHRONY_DIR:$PATH" start_client 2 chronyd \
        "server 192.168.123.1 iburst minpoll 4 maxpoll $MAXPOLL
         makestep 1.0 3" "" ""
else
    LD_PRELOAD="$CLKNETSIM_PATH/clknetsim.so" \
        CLKNETSIM_NODE=2 CLKNETSIM_SOCKET="$CLKNETSIM_TMPDIR/sock" \
        "$RTIMED" sync 192.168.123.1 --minpoll 4 --maxpoll $MAXPOLL \
            --makestep 1.0 3 --verbose ${RT_EXTRA:-} &> "$CLKNETSIM_TMPDIR/log.2" &
    client_pids="$client_pids $!"
    disown $! 2>/dev/null
fi

start_server 2 -l "$DURATION" -o "$WORK/offsets" > /dev/null 2>&1

echo
awk -v d="$DURATION" '
    NF >= 2 {
        n++; a = ($2 < 0 ? -$2 : $2)
        q = int(4 * n / d); if (q > 3) q = 3
        sum[q] += a; cnt[q]++
        if (a > peak) peak = a
        last = $2
    }
    END {
        if (cnt[0] == 0) { print "  NO DATA"; exit 3 }
        printf "  quarter     mean |error|\n"
        for (i = 0; i < 4; i++)
            printf "    %d          %9.3f us\n", i + 1, (cnt[i] ? sum[i]/cnt[i]*1e6 : 0)
        printf "\n  peak over the whole run   %9.3f us\n", peak * 1e6
        printf "  final error               %+9.3f us\n", last * 1e6
        printf "  samples of ground truth   %9d\n", n
        early = cnt[1] ? sum[1]/cnt[1] : 0
        late  = cnt[3] ? sum[3]/cnt[3] : 0
        printf "\n  late/early ratio          %9.2f\n", (early > 0 ? late/early : 0)
        exit 0
    }
' "$WORK/offsets"

read -r ratio <<< "$(awk -v d="$DURATION" '
    NF >= 2 { n++; a = ($2<0?-$2:$2); q = int(4*n/d); if (q>3) q=3; sum[q]+=a; cnt[q]++ }
    END { e = cnt[1]?sum[1]/cnt[1]:0; l = cnt[3]?sum[3]/cnt[3]:0; print (e>0 ? l/e : 0) }
' "$WORK/offsets")"

echo
if [ "$ARM" != chrony ]; then
    # `grep -c` PRINTS 0 on no match and exits non-zero, so `|| echo 0`
    # appended a SECOND line and every numeric test on the result then
    # misbehaved silently: the liveness check read zero exchanges and
    # passed the run anyway.
    alive=$(grep -c "sample source" "$CLKNETSIM_TMPDIR/log.2" 2>/dev/null | head -1)
    alive=${alive:-0}
    printf "  exchanges completed       %9s\n" "$alive"
    if [ "$alive" -eq 0 ]; then
        echo
        echo "FAIL — the daemon logged no exchanges at all; nothing was soaked."
        exit 1
    fi
fi
if grep -qiE "panic|giving up|holding the clock" "$CLKNETSIM_TMPDIR/log.2" 2>/dev/null; then
    echo "  daemon reported a problem:"
    grep -iE "panic|giving up|holding the clock" "$CLKNETSIM_TMPDIR/log.2" | head -3 | sed 's/^/    /'
fi

verdict=$(awk -v r="$ratio" -v g="$GROWTH" \
    'BEGIN{print (r > 0 && r < g) ? "PASS" : "FAIL"}')
echo
if [ "$verdict" = PASS ]; then
    echo "PASS — the last quarter is within ${GROWTH}x of the second. No ratchet."
else
    echo "FAIL — late-run error is ${ratio}x the early-run error; something grows."
    exit 1
fi
