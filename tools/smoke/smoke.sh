#!/bin/bash
# Per-OS smoke rig (M5 exit gate): prove the built binaries actually work on
# this platform, end to end, without touching the system clock.
#
# What it asserts, in order of how badly each would hurt:
#   1. doctor reports a readable clock and names what disciplining needs
#   2. the server starts, binds, and answers a real NTP exchange
#   3. the measured offset is sane (a server that answers wrongly has failed)
#   4. the control plane responds and its counters match the traffic we sent
#   5. the service definition for this platform is emitted and looks right
#
# Deliberately does NOT require privilege: it must be runnable by anyone, on
# any of the supported systems, without changing the machine's time.

set -u
RTIMED=${RTIMED:-./target/release/rtimed}
RTIMEC=${RTIMEC:-./target/release/rtimec}
PORT=${PORT:-11987}
WORK=${WORK:-$(mktemp -d)}
SOCK="$WORK/ctl.sock"
fail=0

note() { printf '  %s\n' "$*"; }
check() {
    if [ "$1" -eq 0 ]; then printf '  PASS  %s\n' "$2"
    else printf '  FAIL  %s\n' "$2"; fail=1; fi
}

cleanup() { [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

echo "== rusty_time smoke rig =="
uname -a 2>/dev/null || true
echo

echo "-- 1. clock capabilities --"
"$RTIMEC" doctor
"$RTIMEC" doctor --json > "$WORK/caps.json"
grep -q '"can_read": true' "$WORK/caps.json"
check $? "clock is readable"
grep -q '"discipline_requirement"' "$WORK/caps.json"
check $? "doctor states what disciplining requires"
if grep -q '"can_discipline": true' "$WORK/caps.json"; then
    note "(this process CAN discipline the clock; the rig still will not)"
else
    note "(unprivileged, as expected for a smoke run)"
fi
echo

echo "-- 2. server starts and answers --"
"$RTIMED" serve --stratum 1 --bind "127.0.0.1:$PORT" \
    --ratelimit-interval -4 --ratelimit-burst 64 \
    --control "$SOCK" > "$WORK/server.log" 2> "$WORK/server.err" &
SRV=$!
sleep 2
kill -0 "$SRV" 2>/dev/null
check $? "server process is alive"
grep -q "NTP listening" "$WORK/server.log"
check $? "server reported its listener"

"$RTIMED" query 127.0.0.1 --port "$PORT" --count 4 --interval-ms 100 \
    --timeout-ms 2000 --json > "$WORK/query.json" 2> "$WORK/query.err"
check $? "query completed"
grep -q '"received": 4' "$WORK/query.json"
check $? "all four exchanges answered"
echo

echo "-- 3. the answer is not merely well-formed, but right --"
# A loopback server sharing our clock must report an offset near zero. This is
# the check that a structurally valid but wrong reply cannot pass.
OFFSET=$(grep -o '"best_offset_s": *[-0-9.e]*' "$WORK/query.json" | awk '{print $2}')
note "measured offset: ${OFFSET:-none}"
if [ -n "$OFFSET" ]; then
    awk -v o="$OFFSET" 'BEGIN{o=o<0?-o:o; exit !(o<0.05)}'
    check $? "offset within 50 ms of our own clock"
else
    check 1 "offset present in the report"
fi
echo

echo "-- 4. control plane --"
"$RTIMEC" --socket "$SOCK" ping > /dev/null 2>&1
check $? "control socket answers ping"
"$RTIMEC" --socket "$SOCK" serverstats > "$WORK/stats.txt" 2>&1
check $? "serverstats op"
# The counters must reflect the traffic we actually generated.
REQS=$(awk '/ntp requests/ {print $NF}' "$WORK/stats.txt")
note "server counted ${REQS:-0} requests"
[ "${REQS:-0}" -ge 4 ] 2>/dev/null
check $? "counters reflect the four requests we sent"
"$RTIMEC" --socket "$SOCK" clients 5 > "$WORK/clients.txt" 2>&1
check $? "clients op"
grep -q "127.0.0.1" "$WORK/clients.txt"
check $? "our own address appears in the client log"
echo

echo "-- 5. wasm gateway (browser path) --"
# Only when the module has been built and node is present; the rest of the rig
# must not depend on a JS toolchain being installed.
PKG=crates/rusty_time-wasm/pkg
if command -v node >/dev/null && [ -f "$PKG/rusty_time_wasm_bg.wasm" ]; then
    GW_PORT=$((PORT + 1))
    "$RTIMED" serve --stratum 1 --bind "127.0.0.1:$((PORT + 2))" \
        --gateway "127.0.0.1:$GW_PORT" --gateway-assets "$PKG" \
        --ratelimit-interval -4 --ratelimit-burst 64 \
        --control "smoke-gw-$$" > "$WORK/gw.log" 2> "$WORK/gw.err" &
    GW=$!
    sleep 2
    node tools/smoke/gateway_node_test.mjs "http://127.0.0.1:$GW_PORT" 4 \
        > "$WORK/wasm.out" 2>&1
    check $? "real wasm module exchanges time with the gateway"
    grep -q "offset within 50 ms" "$WORK/wasm.out"
    check $? "wasm client measured a sane offset"
    grep -q "origin does not match our request is refused" "$WORK/wasm.out"
    check $? "wasm client refuses a forged reply"
    note "$(grep -E '^   offset' "$WORK/wasm.out" 2>/dev/null || true)"
    kill "$GW" 2>/dev/null; wait "$GW" 2>/dev/null
else
    note "SKIP (needs node and a wasm-pack build: tools/wasm/build-npm.sh)"
fi
echo

echo "-- 6. service definition --"
"$RTIMED" service show > "$WORK/service.txt" 2> "$WORK/service.err"
check $? "service definition emitted"
[ -s "$WORK/service.txt" ]
check $? "definition is not empty"
"$RTIMED" service path > /dev/null 2>&1
check $? "install path reported"
note "target: $("$RTIMED" service path 2>/dev/null)"
echo

echo "=========================================="
if [ $fail -eq 0 ]; then
    echo "SMOKE: PASS on $(uname -s 2>/dev/null || echo windows)"
else
    echo "SMOKE: FAIL — logs in $WORK"
    # A failure with no evidence is a failure someone will re-run and shrug at.
    # The commonest cause is a port already in use (two rigs at once, or a
    # daemon left over from a previous run), and the server's stderr says so.
    echo
    echo "--- server stdout ---"; cat "$WORK/server.log" 2>/dev/null
    echo "--- server stderr ---"; cat "$WORK/server.err" 2>/dev/null
    echo "--- query stderr ---";  cat "$WORK/query.err" 2>/dev/null
    echo
    echo "(if this mentions a bind failure, another process holds port $PORT;"
    echo " pass PORT=<n> to use a different one)"
fi
exit $fail
