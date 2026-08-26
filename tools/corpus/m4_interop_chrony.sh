#!/bin/bash
# M4 exit gate: chronyd synchronises from a rusty_time server over PLAIN NTP,
# and again in INTERLEAVED mode (chrony's `xleave` option).
#
# Interleaved mode is the interesting one: chrony only reports `xleave` for a
# source once it has actually completed an interleaved exchange, so seeing it
# in `chronyc ntpdata` proves our server implemented the mode correctly rather
# than merely not breaking.
#
# Prereqs on Linux/WSL: ~/chrony built from source, rtimed at $RTIMED.

CHRONY_DIR=${CHRONY_DIR:-$HOME/chrony}
RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
WORK=${WORK:-$HOME/m4-interop}
NTP_PORT=${NTP_PORT:-11223}

rm -rf "$WORK"; mkdir -p "$WORK"; chmod 750 "$WORK"
cd "$WORK" || exit 1
fail=0

cleanup() {
    [ -n "$CHRONYD_PID" ] && kill "$CHRONYD_PID" 2>/dev/null
    [ -n "$RTIMED_PID" ] && kill "$RTIMED_PID" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT

# The rate limiter defaults would throttle chrony's iburst; raise the limits so
# this test measures protocol interop, not admission policy (which S12 covers).
"$RTIMED" serve --stratum 1 --bind "127.0.0.1:$NTP_PORT" \
    --ratelimit-interval -4 --ratelimit-burst 64 --ratelimit-global 5000 \
    --control "$WORK/rtimed.sock" &> "$WORK/rtimed.log" &
RTIMED_PID=$!
sleep 2

# Assert chrony's measured offset and delay are sane for a loopback server.
#
# This check exists because the first version of this gate passed on
# "Interleaved: Yes" alone while chrony was computing an offset of +362 ms and
# a delay of 4.009 s — a broken implementation that satisfied every structural
# assertion. A time server that answers in the right *shape* but the wrong
# *time* has failed, so the numbers are part of the gate.
check_sane() {
    local label=$1 max_offset=$2 max_delay=$3
    local offset delay bad=0
    offset=$(grep -E "^Offset" "$WORK/ntpdata-$label.out" | awk '{print $3}')
    delay=$(grep -E "^Peer delay" "$WORK/ntpdata-$label.out" | awk '{print $4}')
    if [ -z "$offset" ] || [ -z "$delay" ]; then
        echo "  FAIL: could not read offset/delay for $label"
        return 1
    fi
    awk -v o="$offset" -v m="$max_offset" 'BEGIN{o=o<0?-o:o; exit !(o<m)}' || {
        echo "  FAIL: |offset| ${offset}s exceeds ${max_offset}s ($label)"
        bad=1
    }
    awk -v d="$delay" -v m="$max_delay" 'BEGIN{d=d<0?-d:d; exit !(d<m)}' || {
        echo "  FAIL: delay ${delay}s exceeds ${max_delay}s ($label) — a delay near one"
        echo "        poll interval means timestamps from different exchanges were paired"
        bad=1
    }
    [ $bad -eq 0 ] && echo "  offset ${offset}s, delay ${delay}s — both sane"
    return $bad
}

run_chrony() {
    local label=$1 extra=$2
    cat > "$WORK/chrony-$label.conf" <<EOF
server 127.0.0.1 port $NTP_PORT iburst minpoll 2 maxpoll 3 $extra
driftfile $WORK/drift-$label
pidfile $WORK/pid-$label
bindcmdaddress $WORK/cmd-$label.sock
cmdport 0
port 0
EOF
    "$CHRONY_DIR/chronyd" -d -x -f "$WORK/chrony-$label.conf" &> "$WORK/chronyd-$label.log" &
    CHRONYD_PID=$!
    sleep 12
    echo "--- chronyc sources ($label) ---"
    "$CHRONY_DIR/chronyc" -h "$WORK/cmd-$label.sock" -N sources 2>&1 | tee "$WORK/sources-$label.out"
    echo "--- chronyc ntpdata ($label) ---"
    "$CHRONY_DIR/chronyc" -h "$WORK/cmd-$label.sock" ntpdata 2>&1 | tee "$WORK/ntpdata-$label.out"
    kill "$CHRONYD_PID" 2>/dev/null; wait "$CHRONYD_PID" 2>/dev/null; CHRONYD_PID=
}

echo "=============================================================="
echo " A. chronyd -> rusty_time, plain NTP"
echo "=============================================================="
run_chrony plain ""
if grep -Eq '\^\*' "$WORK/sources-plain.out" && check_sane plain 0.01 0.01; then
    echo "PASS A: chronyd selected the rusty_time server and measured it correctly"
else
    echo "FAIL A: chronyd did not select the source, or the numbers are wrong"
    fail=1
fi

echo
echo "=============================================================="
echo " B. chronyd -> rusty_time, INTERLEAVED (xleave)"
echo "=============================================================="
run_chrony xleave "xleave"
if grep -Eq '\^\*' "$WORK/sources-xleave.out"; then
    echo "  chronyd selected the source"
else
    echo "FAIL B: chronyd did not select the source in xleave mode"
    fail=1
fi
# chrony reports the mode it actually achieved. "Interleaved  : Yes" means our
# server answered an interleaved request correctly.
if grep -Eiq 'interleaved *: *yes' "$WORK/ntpdata-xleave.out"; then
    echo "  chronyd reports Interleaved: Yes"
    # The mode being right is not enough; the time must be right too.
    if check_sane xleave 0.01 0.01; then
        echo "PASS B: interleaved mode negotiated AND measured correctly"
    else
        echo "FAIL B: interleaved negotiated but the measurement is wrong"
        fail=1
    fi
else
    echo "FAIL B: chrony did not achieve interleaved mode"
    grep -i interleaved "$WORK/ntpdata-xleave.out" || true
    fail=1
fi

echo
echo "--- rtimed serverstats (via rtimec ops) ---"
"${RTIMEC:-$HOME/rt-target/release/rtimec}" --socket "$WORK/rtimed.sock" serverstats 2>&1 || true
"${RTIMEC:-$HOME/rt-target/release/rtimec}" --socket "$WORK/rtimed.sock" clients 5 2>&1 || true

echo
echo "=============================================================="
[ $fail -eq 0 ] && echo "M4 INTEROP: PASS" || echo "M4 INTEROP: FAIL"
exit $fail
