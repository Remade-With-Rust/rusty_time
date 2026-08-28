#!/bin/bash
# Multi-source selection, against a falseticker.
#
# Every other scenario in this corpus has exactly ONE server, which means the
# selection algorithm, the falseticker rejection and the per-source drain
# bookkeeping have never been exercised end to end — while a real deployment
# almost always configures three or four servers. A multi-source defect was
# found here once already, by reading the code rather than by running it: every
# source's drain was applied to the clock while only the selected source's
# samples were used. A single-source rig cannot see that.
#
# The topology:
#
#     node 1  chronyd, stratum 1, correct
#     node 2  chronyd, stratum 1, correct
#     node 3  chronyd, stratum 1, WRONG BY FIVE SECONDS   <- the falseticker
#     node 4  the client under test, pointed at all three
#
# The pass condition is not "converges". It is **converges to the truth**. Two
# good sources and one liar is the case the algorithm exists for: a client that
# averages its inputs lands about 1.7 s out, and a client that simply prefers
# the closest source can be walked anywhere. Only one that REJECTS the outlier
# ends near zero.
#
# Usage: tools/corpus/multisource.sh [duration]
#        ARMS="chrony rusty_time" tools/corpus/multisource.sh

set -u
export PATH="$HOME/.cargo/bin:$PATH"
export CLKNETSIM_PATH=${CLKNETSIM_PATH:-$HOME/clknetsim}
CHRONY_DIR=${CHRONY_DIR:-$HOME/chrony}
RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
WORK=${WORK:-$HOME/multisource}
DURATION=${1:-${DURATION:-1200}}
ARMS=${ARMS:-"chrony rusty_time"}
SEED=${SEED:-701}
# How far the liar is out. Large enough that following it is unmistakable.
FALSE_OFFSET=${FALSE_OFFSET:-5.0}
# The client is judged against this at the end of the run.
PASS_S=${PASS_S:-0.001}

rm -rf "$WORK"; mkdir -p "$WORK"; chmod 750 "$WORK"
export CLKNETSIM_TMPDIR="$WORK/tmp"
mkdir -p "$CLKNETSIM_TMPDIR"
# clknetsim.bash reads these unconditionally; under `set -u` an unset optional
# is a fatal error rather than an empty string.
export CLKNETSIM_CLIENT_WRAPPER=${CLKNETSIM_CLIENT_WRAPPER:-}
export CLKNETSIM_SERVER_WRAPPER=${CLKNETSIM_SERVER_WRAPPER:-}
client_pids=""
. "$CLKNETSIM_PATH/clknetsim.bash"
export CLKNETSIM_RANDOM_SEED=$SEED

write_conf() {
    {
        # Three servers. Two keep good time; the third is confidently wrong,
        # which is exactly what a falseticker is — not unreachable, not noisy,
        # just wrong and perfectly happy about it.
        echo "node1_offset = 0.0"
        echo "node2_offset = 0.0"
        echo "node3_offset = $FALSE_OFFSET"
        # The client: a 10 ms start and a real oscillator, as elsewhere.
        echo "node4_offset = 0.010"
        echo "node4_freq = 20e-6"
        # A LAN path between the client and each server, both directions.
        for n in 1 2 3; do
            echo "node4_delay$n = (+ 100e-6 (* 10e-6 (exponential)))"
            echo "node${n}_delay4 = (+ 100e-6 (* 10e-6 (exponential)))"
        done
    } > "$CLKNETSIM_TMPDIR/conf"
}

run_arm() {
    local arm=$1
    rm -f "$CLKNETSIM_TMPDIR"/*
    write_conf

    for n in 1 2 3; do
        PATH="$CHRONY_DIR:$PATH" start_client "$n" chronyd "local stratum 1" "" ""
    done

    case "$arm" in
        chrony)
            PATH="$CHRONY_DIR:$PATH" start_client 4 chronyd \
                "server 192.168.123.1 iburst minpoll 4 maxpoll 6
                 server 192.168.123.2 iburst minpoll 4 maxpoll 6
                 server 192.168.123.3 iburst minpoll 4 maxpoll 6
                 makestep 1.0 3" "" ""
            ;;
        rusty_time)
            LD_PRELOAD="$CLKNETSIM_PATH/clknetsim.so" \
                CLKNETSIM_NODE=4 CLKNETSIM_SOCKET="$CLKNETSIM_TMPDIR/sock" \
                "$RTIMED" sync 192.168.123.1 192.168.123.2 192.168.123.3 \
                    --minpoll 4 --maxpoll 6 --makestep 1.0 3 ${RT_EXTRA:-} \
                    &> "$CLKNETSIM_TMPDIR/log.4" &
            client_pids="$client_pids $!"
            disown $! 2>/dev/null
            ;;
    esac

    start_server 4 -l "$DURATION" -o "$WORK/offsets-$arm" > /dev/null 2>&1
}

echo "== multi-source selection against a falseticker =="
echo "   3 servers, one of them ${FALSE_OFFSET}s wrong; client must reject it"
echo "   ${DURATION}s simulated, seed $SEED"
echo
printf '%-12s %14s %14s %12s  %s\n' arm "final error" "worst (last ¼)" "converged" verdict
printf '%.0s-' {1..70}; echo

fail=0
for arm in $ARMS; do
    run_arm "$arm"
    # Column 4 is node 4 — the client. Ground truth from the simulator, not
    # from the daemon's opinion of itself.
    read -r final worst conv <<< "$(awk -v d="$DURATION" '
        NF >= 4 { n++; v = $4; a = (v < 0 ? -v : v); last = v
                  if (n > d * 0.75 && a > worst) worst = a
                  if (!conv && a < 0.001) conv = n }
        END { printf "%.9f %.9f %d", last, worst, conv }
    ' "$WORK/offsets-$arm")"
    absfinal=$(awk -v v="$final" 'BEGIN{print (v<0?-v:v)}')
    verdict=$(awk -v a="$absfinal" -v p="$PASS_S" \
        'BEGIN{print (a < p) ? "PASS  rejected the falseticker" : "FAIL  followed the liar"}')
    [[ $verdict == FAIL* ]] && fail=1
    printf '%-12s %11.3f ms %11.3f ms %10ss  %s\n' \
        "$arm" "$(awk -v v="$final" 'BEGIN{print v*1e3}')" \
        "$(awk -v v="$worst" 'BEGIN{print v*1e3}')" \
        "${conv:-never}" "$verdict"
done

echo
if [ "$fail" -eq 0 ]; then
    echo "PASS — every arm stayed with the majority."
else
    echo "FAIL — an arm followed a source it should have rejected."
fi
exit "$fail"
