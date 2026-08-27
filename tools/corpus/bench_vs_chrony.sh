#!/bin/bash
# TIMECORP cross-implementation arm: rusty_time vs chrony, in identical
# simulated worlds.
#
# This is the comparison gates G1-G3 need, and the one this project has
# refused to claim until it could actually be run.
#
# Fairness, deliberately:
#   * ONE config file per scenario, used by both arms. Same offset, same
#     frequency error, same delays, same jitter, same duration.
#   * chrony gets a TUNED configuration -- iburst, the same makestep policy,
#     the same poll bounds -- not a strawman.
#   * Both clients talk to the same chronyd server on node 1, so the server is
#     not a variable.
#   * clknetsim is deterministic for a given config, so each number is exactly
#     reproducible rather than a sample from a noisy rig.
#
# Metrics come from clknetsim's own offset log (-o), which records each node's
# true error once per second. That is ground truth from the simulator, not
# either implementation's opinion of itself.

export PATH="$HOME/.cargo/bin:$PATH"
export CLKNETSIM_PATH=${CLKNETSIM_PATH:-$HOME/clknetsim}
CHRONY_DIR=${CHRONY_DIR:-$HOME/chrony}
RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
WORK=${WORK:-$HOME/bench-vs-chrony}
DURATION=${DURATION:-3000}

# Build what we are about to measure. A benchmark that silently runs a stale
# binary reports numbers for code that no longer exists — this script did
# exactly that once, and the result looked like a bug in the fix.
if [ -z "${SKIP_BUILD:-}" ]; then
    ( cd "$(dirname "$0")/../.." \
      && CARGO_TARGET_DIR="$(dirname "$RTIMED")/.." cargo build --release -p rusty_time-daemon ) \
      || { echo "build failed" >&2; exit 1; }
fi

rm -rf "$WORK"; mkdir -p "$WORK"; chmod 750 "$WORK"
export CLKNETSIM_TMPDIR="$WORK/tmp"
mkdir -p "$CLKNETSIM_TMPDIR"
. "$CLKNETSIM_PATH/clknetsim.bash"

# ---------------------------------------------------------------- scenarios --
# Mirrors the deterministic harness definitions (mission plan 7.2).
scenario_conf() {
    case "$1" in
        S1) # LAN symmetric: 200us RTT, small jitter, +20ppm, 10ms offset
            echo "node2_offset = 0.010"
            echo "node2_freq = 20e-6"
            echo "node2_delay1 = (+ 100e-6 (* 10e-6 (exponential)))"
            echo "node1_delay2 = (+ 100e-6 (* 10e-6 (exponential)))"
            ;;
        S6) # cold start: 500ms initial offset
            echo "node2_offset = 0.5"
            echo "node2_freq = 20e-6"
            echo "node2_delay1 = (+ 100e-6 (* 10e-6 (exponential)))"
            echo "node1_delay2 = (+ 100e-6 (* 10e-6 (exponential)))"
            ;;
        S8) # drifty oscillator: +100ppm with wander
            echo "node2_offset = 0.010"
            echo "node2_freq = (sum (* 1e-9 (normal)))"
            echo "node2_freq_offset = 100e-6"
            echo "node2_delay1 = (+ 100e-6 (* 10e-6 (exponential)))"
            echo "node1_delay2 = (+ 100e-6 (* 10e-6 (exponential)))"
            ;;
        S2) # WAN asymmetric: 40ms RTT, 2:1 asymmetry
            echo "node2_offset = 0.010"
            echo "node2_freq = 20e-6"
            echo "node2_delay1 = (+ 26.7e-3 (* 2e-3 (exponential)))"
            echo "node1_delay2 = (+ 13.3e-3 (* 2e-3 (exponential)))"
            ;;
        S4) # congested: 10% loss, bursty delay.
            # clknetsim has no loss knob: a negative delay is a dropped packet.
            # (equal 0.1 (uniform) 0) is 1.0 when the uniform draw lands within
            # 0.1 of zero -- so 10% of packets -- and subtracting 1 second from
            # a millisecond-scale delay makes those negative.
            echo "node2_offset = 0.010"
            echo "node2_freq = 20e-6"
            echo "node2_delay1 = (+ 5e-3 (* 20e-3 (exponential)) (* -1 (equal 0.1 (uniform) 0)))"
            echo "node1_delay2 = (+ 5e-3 (* 20e-3 (exponential)) (* -1 (equal 0.1 (uniform) 0)))"
            ;;
    esac
}

# ------------------------------------------------------------------- metrics --
# Column 2 of clknetsim's offset log is node 2's true error, once per second.
metrics() {
    local log=$1
    awk '
    { if (NF >= 2) { v = $2; if (v < 0) v = -v; n++; a[n] = v;
        if (conv1ms == 0 && v < 1e-3) conv1ms = n;
        if (conv1ms > 0 && v >= 1e-3) conv1ms = 0;
        if (conv100us == 0 && v < 1e-4) conv100us = n;
        if (conv100us > 0 && v >= 1e-4) conv100us = 0; } }
    END {
        if (n == 0) { print "no-data"; exit }
        # Steady state = last quarter of the run.
        start = int(n * 0.75); cnt = 0
        for (i = start; i <= n; i++) { s[cnt++] = a[i] }
        # insertion sort (cnt is small)
        for (i = 1; i < cnt; i++) { k = s[i]; j = i - 1
            while (j >= 0 && s[j] > k) { s[j+1] = s[j]; j-- } s[j+1] = k }
        p50 = s[int(cnt * 0.50)]; p95 = s[int(cnt * 0.95)]; mx = s[cnt-1]
        printf "%.9f %.9f %.9f %d %d\n", p50, p95, mx, conv1ms, conv100us
    }' "$log"
}

fmt() { awk -v v="$1" 'BEGIN{ if (v == "" ) {print "n/a"; exit}
    a = v < 0 ? -v : v
    if (a >= 1) printf "%.3f s", v
    else if (a >= 1e-3) printf "%.3f ms", v*1e3
    else printf "%.1f us", v*1e6 }'; }

fmt_conv() { [ "${1:-0}" -gt 0 ] 2>/dev/null && echo "${1}s" || echo "never"; }

# --------------------------------------------------------------------- arms --
run_arm() {
    local scenario=$1 arm=$2
    local tag="$scenario-$arm"
    rm -f "$CLKNETSIM_TMPDIR"/*
    scenario_conf "$scenario" > "$CLKNETSIM_TMPDIR/conf"

    # Node 1 is always the same chronyd stratum-1 server, so the server is not
    # a variable between arms.
    PATH="$CHRONY_DIR:$PATH" start_client 1 chronyd "local stratum 1" "" ""

    case "$arm" in
        # chrony_null is deliberately identical to chrony: it is the control.
        chrony|chrony_null)
            # Tuned, not a strawman: iburst, the same poll bounds and the same
            # makestep policy the rusty_time arm is given.
            PATH="$CHRONY_DIR:$PATH" start_client 2 chronyd \
                "server 192.168.123.1 iburst minpoll 4 maxpoll 6
                 makestep 1.0 3" "" ""
            ;;
        rusty_nostop)
            # Control arm: identical binary with drain budgets not enforced.
            LD_PRELOAD="$CLKNETSIM_PATH/clknetsim.so"                 RUSTY_TIME_NO_DRAIN_STOP=1                 CLKNETSIM_NODE=2 CLKNETSIM_SOCKET="$CLKNETSIM_TMPDIR/sock"                 "$RTIMED" sync 192.168.123.1                     --minpoll 4 --maxpoll 6 --makestep 1.0 3 ${RT_EXTRA:-}                     &> "$CLKNETSIM_TMPDIR/log.2" &
            client_pids="$client_pids $!"
            disown $! 2>/dev/null
            ;;
        rusty_time)
            LD_PRELOAD="$CLKNETSIM_PATH/clknetsim.so" \
                CLKNETSIM_NODE=2 CLKNETSIM_SOCKET="$CLKNETSIM_TMPDIR/sock" \
                "$RTIMED" sync 192.168.123.1 \
                    --minpoll 4 --maxpoll 6 --makestep 1.0 3 ${RT_EXTRA:-} \
                    &> "$CLKNETSIM_TMPDIR/log.2" &
            client_pids="$client_pids $!"
            disown $! 2>/dev/null
            ;;
    esac

    start_server 2 -l "$DURATION" -o "$WORK/offsets-$tag" > /dev/null 2>&1
    cp "$CLKNETSIM_TMPDIR/log.2" "$WORK/log-$tag" 2>/dev/null
    metrics "$WORK/offsets-$tag"
}

# --------------------------------------------------------------------- main --
SCENARIOS=${SCENARIOS:-"S1 S6 S8 S2 S4"}
echo "== TIMECORP cross-implementation arm =="
echo "   ${DURATION}s simulated per run, identical config per scenario"
echo "   chrony: $("$CHRONY_DIR/chronyd" --version 2>&1 | head -1 | cut -c1-60)"
echo

# clknetsim redraws its random streams each run, so a single run is one sample
# of a distribution, not the answer. Repeat and report the median run and the
# worst run: a time daemon is judged on its bad days.
REPS=${REPS:-11}

# Median of a whitespace-separated list.
median() { tr ' ' '\n' <<< "$1" | grep -v '^$' | sort -g | awk '{v[n++]=$1} END{print v[int(n/2)]}'; }
worst()  { tr ' ' '\n' <<< "$1" | grep -v '^$' | sort -g | tail -1; }

echo "   $REPS repetitions per arm; reporting the median run and the worst run"
echo
echo "   'chrony_null' is a SECOND chrony arm: identical code both sides, so"
echo "   whatever separates it from the chrony arm is this rig's own resolution."
echo
printf '%-5s %-12s %10s %10s %10s %9s %9s  %s\n' \
    scen arm "p50(med)" "p95(med)" "max(worst)" "to<1ms" "to<100us" "p50 spread"
printf '%.0s-' {1..104}; echo

# ARMS lets a tuning loop run just the implementation under test. Convergence
# is deterministic in this simulator -- the same change gives the same number
# to the second -- so one rep of one arm is a verdict when iterating on it.
ARMS=${ARMS:-"chrony chrony_null rusty_time"}

declare -A MED_P50 P50S
for scenario in $SCENARIOS; do
    for arm in $ARMS; do
        p50s=""; p95s=""; mxs=""; c1s=""; c2s=""; failures=0
        for _ in $(seq "$REPS"); do
            read -r p50 p95 mx c1 c2 <<< "$(run_arm "$scenario" "$arm")"
            if [ "$p50" = "no-data" ]; then failures=$((failures+1)); continue; fi
            p50s="$p50s $p50"; p95s="$p95s $p95"; mxs="$mxs $mx"
            # A run that never converged counts as the run length, not as a
            # missing value -- dropping it would flatter the implementation.
            [ "${c1:-0}" -eq 0 ] && c1=$DURATION
            [ "${c2:-0}" -eq 0 ] && c2=$DURATION
            c1s="$c1s $c1"; c2s="$c2s $c2"
        done
        if [ -z "$p50s" ]; then
            printf '%-5s %-11s %11s (all %d runs failed)\n' \
                "$scenario" "$arm" "NO DATA" "$REPS"
            continue
        fi
        note=""
        [ "$failures" -gt 0 ] && note=" [$failures/$REPS runs produced nothing]"
        med=$(median "$p50s")
        MED_P50["$scenario-$arm"]=$med
        P50S["$scenario-$arm"]=$p50s
        # The min-max spread of p50 across reps. When this is wider than the
        # distance between the arms, the distance is not a result.
        lo=$(tr ' ' '\n' <<< "$p50s" | grep -v '^$' | sort -g | head -1)
        hi=$(worst "$p50s")
        # Convergence spread too, for the same reason. This session moved S6's
        # convergence through 28/18/14/16 s on medians whose reproducibility was
        # never shown; a median with no spread beside it invites exactly that.
        c1lo=$(tr ' ' '\n' <<< "$c1s" | grep -v '^$' | sort -g | head -1)
        c1hi=$(tr ' ' '\n' <<< "$c1s" | grep -v '^$' | sort -g | tail -1)
        printf '%-5s %-12s %10s %10s %10s %9s %9s  %s-%s  conv %s-%ss%s\n' \
            "$scenario" "$arm" \
            "$(fmt "$med")" "$(fmt "$(median "$p95s")")" \
            "$(fmt "$(worst "$mxs")")" \
            "$(median "$c1s")s" "$(median "$c2s")s" \
            "$(fmt "$lo")" "$(fmt "$hi")" \
            "$c1lo" "$c1hi" "$note"
    done

    # ---- the verdict the rig owes the reader ----
    #
    # Comparing two medians treats the gap between the two chrony arms as the
    # resolution. That is a single draw and routinely comes out absurdly small
    # -- 0.04 us on S6 beside within-arm spreads of 3 us. It over-claimed badly
    # enough that S8 read "RESOLVED, rusty_time ahead" in one run and
    # "RESOLVED, chrony ahead" in the next, on identical code.
    #
    # Rounds are the pairing: every arm runs in the same round against the same
    # box, so a per-round comparison cancels the drift that defeats medians.
    ra=${P50S["$scenario-chrony"]:-}
    rr=${P50S["$scenario-rusty_time"]:-}
    if [ -n "$ra" ] && [ -n "$rr" ]; then
        paste <(tr ' ' '
' <<< "$ra" | grep -v '^$') <(tr ' ' '
' <<< "$rr" | grep -v '^$') \
        | awk 'BEGIN{w=0;n=0} { if ($1 != $2) { n++; if ($2 < $1) w++ } } END { if (n < 1) { print "      paired  : no comparable rounds"; exit } z = (w - n/2.0)/(0.5*sqrt(n)); v = (z > 2.0) ? "RESOLVED, rusty_time ahead" : ((z < -2.0) ? "RESOLVED, chrony ahead" : "NOT RESOLVED (needs |z| > 2)"); print "      paired  : rusty_time better in " w "/" n " rounds, z = " sprintf("%+.2f", z) " : " v }'
    fi
    echo
done

echo
echo "convergence times are medians; a run that never converged is counted"
echo "as the full ${DURATION}s rather than dropped."
echo "offset logs: $WORK/offsets-*"
