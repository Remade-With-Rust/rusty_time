#!/bin/bash
# Instructions retired per reply served, for the rusty_time server.
#
# The wall-clock G4 rig cannot resolve a change of a few percent: its control
# arm's spread across rounds is wider than its own median. That is not a reason
# to guess the sign — it is a reason to change instrument. callgrind counts the
# server's user-space instructions exactly, so a change that removes work shows
# up as a smaller number, reproducibly, on any box under any load.
#
# The server runs ~50x slower under callgrind, so the load is small and the
# concurrency low. Neither matters: Ir per reply is a rate, not a duration.
#
# Usage: tools/perf/server_ir.sh [arm ...]     (default: batched scalar)

set -u
export PATH="$HOME/.cargo/bin:$PATH"
TARGET=${CARGO_TARGET_DIR:-$HOME/rt-target}
RTIMED=${RTIMED:-$TARGET/release/rtimed}
TIMECORP=${TIMECORP:-$TARGET/release/timecorp}
OUT=${OUT:-$HOME/rt-perf}
PORT=${PORT:-11144}
REQUESTS=${REQUESTS:-20000}
CONCURRENCY=${CONCURRENCY:-16}
ARMS=${*:-"batched scalar"}

mkdir -p "$OUT"
if [ -z "${SKIP_BUILD:-}" ]; then
    ( cd "$(dirname "$0")/../.." \
      && CARGO_PROFILE_RELEASE_DEBUG=true CARGO_TARGET_DIR="$TARGET" \
         cargo build --release -p rusty_time-daemon -p rusty_time-sim ) \
      || { echo "build failed" >&2; exit 1; }
fi

echo "== server instructions per reply =="
echo "   $REQUESTS requests, concurrency $CONCURRENCY, callgrind"
echo

for arm in $ARMS; do
    cg="$OUT/srv-$arm.out"
    rm -f "$cg"
    env_flag=""
    [ "$arm" = "scalar" ] && env_flag="RUSTY_TIME_NO_BATCH_SEND=1"

    # shellcheck disable=SC2086
    env $env_flag valgrind --tool=callgrind --cache-sim=no --branch-sim=no \
        --callgrind-out-file="$cg" \
        "$RTIMED" serve --bind "127.0.0.1:$PORT" --stratum 1 --no-ratelimit \
        --control "$OUT/srv-$arm.sock" &> "$OUT/srv-$arm.log" &
    srv=$!

    # Wait until it answers rather than sleeping a guessed interval.
    ready=0
    for _ in $(seq 200); do
        if "$TIMECORP" load --target "127.0.0.1:$PORT" --requests 1 \
             --concurrency 1 --timeout-ms 500 2>/dev/null | grep -q '^answered   1$'; then
            ready=1; break
        fi
        sleep 0.2
    done
    if [ "$ready" -ne 1 ]; then
        echo "  $arm: server never answered (see $OUT/srv-$arm.log)"
        kill $srv 2>/dev/null; wait $srv 2>/dev/null
        continue
    fi

    answered=$("$TIMECORP" load --target "127.0.0.1:$PORT" \
        --requests "$REQUESTS" --concurrency "$CONCURRENCY" --timeout-ms 60000 \
        | awk '/^answered/{print $2}')

    # SIGTERM: callgrind dumps its counts on exit.
    kill -TERM $srv 2>/dev/null
    wait $srv 2>/dev/null
    for _ in $(seq 50); do [ -s "$cg" ] && break; sleep 0.2; done

    ir=$(grep '^summary:' "$cg" 2>/dev/null | awk '{print $2}')
    if [ -z "$ir" ] || [ -z "$answered" ] || [ "$answered" = "0" ]; then
        echo "  $arm: no data (answered=${answered:-0}, ir=${ir:-none})"
        continue
    fi
    # The readiness probe and startup are in this total too, so the figure is
    # only meaningful compared against another arm measured the same way.
    awk -v a="$arm" -v ir="$ir" -v n="$answered" 'BEGIN{
        printf "  %-8s answered %6d   Ir %12d   %8.1f Ir/reply\n", a, n, ir, ir/n }'
done

echo
echo "compare arms against each other; the absolute figure includes startup."
