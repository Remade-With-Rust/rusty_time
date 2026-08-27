#!/bin/bash
# TIMECORP G4: server throughput, rusty_time vs chrony, on the same rig.
#
# G4 asks for replies/second at least chrony's, at no worse p99. This is the
# arm that prices the server work — batched receive, the client table, the NTS
# reply path — none of which the client-side comparison can see.
#
# Both servers are driven by the SAME generator over a real UDP socket, on the
# same loopback, alternating which arm goes first (ABBA) so that "the second
# one is warmer" cancels. A NULL arm runs chrony against itself, so the rig
# states its own resolution instead of leaving the reader to assume it has
# none.
#
# Two numbers per arm, and the second is the trustworthy one:
#
#   replies/s        the gate's unit. Wall clock, on a shared box: it drifts.
#   cpu_us_reply     server CPU microseconds per reply, read from /proc around
#                    the run. CPU time does not accrue while descheduled, so
#                    this barely moves — and it is what actually changes when
#                    the server gets cheaper.
#
# RATE LIMITING IS OFF IN BOTH ARMS, deliberately and symmetrically. Left on,
# this measures two rate limiters refusing traffic rather than two servers
# answering it, and the faster refuser would win.

set -u
export PATH="$HOME/.cargo/bin:$PATH"
CHRONY_DIR=${CHRONY_DIR:-$HOME/chrony}
TARGET=${CARGO_TARGET_DIR:-$HOME/rt-target}
RTIMED=${RTIMED:-$TARGET/release/rtimed}
TIMECORP=${TIMECORP:-$TARGET/release/timecorp}
WORK=${WORK:-$HOME/g4}
PORT=${PORT:-11123}
REQUESTS=${REQUESTS:-300000}
CONCURRENCY=${CONCURRENCY:-64}
ROUNDS=${ROUNDS:-5}

if [ -z "${SKIP_BUILD:-}" ]; then
    ( cd "$(dirname "$0")/../.." \
      && CARGO_TARGET_DIR="$TARGET" cargo build --release -p rusty_time-daemon -p rusty_time-sim ) \
      || { echo "build failed" >&2; exit 1; }
fi

# Pin the server and the generator to DIFFERENT cores. Affinity restricts, it
# does not reserve, but keeping the two off each other stops the measurement
# being a fight between the thing under test and the thing testing it -- and
# stops migration masquerading as "the box is busy".
CORES=$(nproc)
if [ "$CORES" -ge 4 ]; then
    PIN_SERVER="taskset -c 2"
    PIN_LOAD="taskset -c 3"
else
    PIN_SERVER=""; PIN_LOAD=""
fi

rm -rf "$WORK"; mkdir -p "$WORK"

cleanup() { [ -n "${SRV_PID:-}" ] && kill "$SRV_PID" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

start_server() {
    local arm=$1
    case "$arm" in
        rusty_scalar)
            # Identical binary, batched sending switched off: the within-run
            # control for the sendmmsg change. Comparing it against the
            # rusty_time arm keeps the comparison inside one run, where the
            # box's own drift cancels.
            RUSTY_TIME_NO_BATCH_SEND=1 $PIN_SERVER "$RTIMED" serve                 --bind "127.0.0.1:$PORT" --stratum 1 --no-ratelimit                 --control "$WORK/rtimed.sock" &> "$WORK/server-$arm.log" &
            SRV_PID=$!
            ;;
        rusty_time)
            # --no-ratelimit: the symmetric counterpart of chrony's config below.
            $PIN_SERVER "$RTIMED" serve --bind "127.0.0.1:$PORT" --stratum 1 \
                --no-ratelimit --control "$WORK/rtimed.sock" \
                &> "$WORK/server-$arm.log" &
            SRV_PID=$!
            ;;
        chrony|chrony_null)
            cat > "$WORK/chrony-$arm.conf" <<EOF
port $PORT
bindaddress 127.0.0.1
local stratum 1
allow all
# No rate limiting, matching the rusty_time arm. A limiter here would make
# this a comparison of refusal rates.
clientloglimit 0
driftfile $WORK/chrony-$arm.drift
pidfile $WORK/chrony-$arm.pid
EOF
            $PIN_SERVER "$CHRONY_DIR/chronyd" -f "$WORK/chrony-$arm.conf" -d -x \
                &> "$WORK/server-$arm.log" &
            SRV_PID=$!
            ;;
    esac
    # Wait for it to answer rather than sleeping a guessed interval.
    for _ in $(seq 100); do
        if SERVER_PID=$SRV_PID "$TIMECORP" load --target "127.0.0.1:$PORT" \
             --requests 1 --concurrency 1 --timeout-ms 200 2>/dev/null \
             | grep -q '^answered   1$'; then
            return 0
        fi
        sleep 0.1
    done
    echo "  ($arm did not come up; see $WORK/server-$arm.log)" >&2
    return 1
}

run_arm() {
    local arm=$1 round=$2
    SRV_PID=""
    if ! start_server "$arm"; then echo "no-data"; return; fi
    SERVER_PID=$SRV_PID $PIN_LOAD "$TIMECORP" load --target "127.0.0.1:$PORT" \
        --requests "$REQUESTS" --concurrency "$CONCURRENCY" \
        > "$WORK/load-$arm-$round.txt" 2>&1
    kill "$SRV_PID" 2>/dev/null; wait "$SRV_PID" 2>/dev/null
    SRV_PID=""
    awk '/^answered/{a=$2} /^replies_s/{r=$2} /^p99_us/{p=$2} /^cpu_us_reply/{c=$2}
         END{ if (a=="" || a==0) print "no-data"; else printf "%s %s %s %s\n", a, r, p, (c==""?"nan":c) }' \
        "$WORK/load-$arm-$round.txt"
}

median() { tr ' ' '\n' <<< "$1" | grep -v '^$' | sort -g | awk '{v[n++]=$1} END{print v[int(n/2)]}'; }

echo "== TIMECORP G4: server throughput =="
echo "   $REQUESTS requests per round, concurrency $CONCURRENCY, $ROUNDS rounds"
echo "   arm order rotates each round; chrony_null is chrony vs itself"
echo

# Arms to run. rusty_scalar is the within-run control for batched sending;
# leave it out for a plain chrony comparison.
ALL_ARMS=${ALL_ARMS:-"rusty_time chrony chrony_null"}
ALL_ARR=($ALL_ARMS)

declare -A ANS REP P99 CPU
for arm in $ALL_ARMS; do ANS[$arm]=""; REP[$arm]=""; P99[$arm]=""; CPU[$arm]=""; done

for round in $(seq "$ROUNDS"); do
    # ROTATE the order, do not merely reverse it. With three arms, swapping
    # first and last leaves the middle arm permanently in the middle -- which
    # is what the first version of this script did, and it showed up as the two
    # identical chrony arms differing by 31%. A rotation gives every arm every
    # position once per three rounds, so run ROUNDS in multiples of three.
    shift=$(( (round - 1) % ${#ALL_ARR[@]} ))
    all=($ALL_ARMS)
    order=""
    for k in $(seq 0 $(( ${#ALL_ARR[@]} - 1 ))); do
        order="$order ${ALL_ARR[$(( (shift + k) % ${#ALL_ARR[@]} ))]}"
    done
    for arm in $order; do
        read -r a r p c <<< "$(run_arm "$arm" "$round")"
        [ "$a" = "no-data" ] && { echo "  round $round $arm: no data"; continue; }
        ANS[$arm]="${ANS[$arm]} $a"; REP[$arm]="${REP[$arm]} $r"
        P99[$arm]="${P99[$arm]} $p"; CPU[$arm]="${CPU[$arm]} $c"
    done
    printf '  round %s done\n' "$round"
done

echo
printf '%-12s %12s %12s %10s %14s\n' arm answered replies_s p99_us cpu_us_reply
printf '%.0s-' {1..64}; echo
for arm in $ALL_ARMS; do
    [ -z "${REP[$arm]}" ] && { printf '%-12s %12s\n' "$arm" "NO DATA"; continue; }
    printf '%-12s %12s %12s %10s %14s\n' "$arm" \
        "$(median "${ANS[$arm]}")" "$(median "${REP[$arm]}")" \
        "$(median "${P99[$arm]}")" "$(median "${CPU[$arm]}")"
done

echo
# Work-count parity: a throughput comparison where the arms answered different
# numbers of requests is void, so it is checked rather than assumed.
have() { [ -n "${REP[$1]:-}" ]; }

if have chrony && have rusty_time; then
    CA=$(median "${ANS[chrony]}"); RA=$(median "${ANS[rusty_time]}")
    awk -v c="$CA" -v r="$RA" 'BEGIN { d = c > r ? (c-r)/c : (r-c)/c; printf "work parity : chrony answered %d, rusty_time %d (%.2f%% apart)%s\n", c, r, d*100, (d > 0.01 ? "  <-- NOT COMPARABLE" : "") }'
fi

# label  reference-arm  control-arm  test-arm  higher_is_better  test-name
verdict() {
    awk -v lbl="$1" -v a="$2" -v b="$3" -v r="$4" -v hib="$5" -v ta="$6" 'BEGIN { if (a == "" || b == "" || r == "" || a == "nan" || r == "nan") exit; floor = a > b ? a - b : b - a; delta = r > a ? r - a : a - r; ahead = hib ? (r > a) : (r < a); printf "%-13s: floor %.2f | gap %.2f : ", lbl, floor, delta; if (delta > floor * 2.0) printf "RESOLVED, %s\n", (ahead ? ta " ahead" : "chrony ahead"); else printf "NOT RESOLVED by this rig\n" }'
}

# vs chrony, with the chrony-vs-chrony null arm as the floor.
if have chrony && have chrony_null && have rusty_time; then
    verdict "replies/s" "$(median "${REP[chrony]}")" "$(median "${REP[chrony_null]}")" "$(median "${REP[rusty_time]}")" 1 "rusty_time"
    # The stable one: server CPU does not accrue while descheduled, so a real
    # efficiency change shows up here before wall throughput can see it.
    verdict "cpu_us/reply" "$(median "${CPU[chrony]}")" "$(median "${CPU[chrony_null]}")" "$(median "${CPU[rusty_time]}")" 0 "rusty_time"
fi

# Within-run A/B of one rusty_time build against another. No null arm here, so
# the floor is the control arm's own spread across rounds -- the same question
# asked another way -- and it keeps the comparison inside a single run rather
# than across runs whose absolute throughput drifts by tens of percent.
if have rusty_time && have rusty_scalar; then
    spread() { tr ' ' '\n' <<< "$1" | grep -v '^$' | sort -g \
               | awk 'NR==1{lo=$1} {hi=$1} END{print hi-lo}'; }
    echo
    echo "within-run A/B: rusty_time (batched send) vs rusty_scalar (sendto per reply)"
    A=$(median "${REP[rusty_scalar]}"); R=$(median "${REP[rusty_time]}")
    S=$(spread "${REP[rusty_scalar]}")
    awk -v a="$A" -v r="$R" -v sp="$S" 'BEGIN { printf "replies/s    : scalar %.0f -> batched %.0f (%+.2f%%); control spread %.0f : %s\n", a, r, (r-a)/a*100, sp, ((r-a) > sp ? "RESOLVED" : "NOT RESOLVED by this rig") }'
    A=$(median "${CPU[rusty_scalar]}"); R=$(median "${CPU[rusty_time]}")
    S=$(spread "${CPU[rusty_scalar]}")
    awk -v a="$A" -v r="$R" -v sp="$S" 'BEGIN { if (a == "nan" || r == "nan") exit; printf "cpu_us/reply : scalar %.3f -> batched %.3f (%+.2f%%); control spread %.3f : %s\n", a, r, (r-a)/a*100, sp, ((a-r) > sp ? "RESOLVED" : "NOT RESOLVED by this rig") }'
fi

echo
echo "note: p99 here is queueing latency at concurrency $CONCURRENCY against a"
echo "      saturated server, so all arms sit near the same value; it measures"
echo "      the generator's queue depth, not server responsiveness."
echo "logs: $WORK/"
