#!/bin/bash
# Instrumented interleaved probe: shows the timestamp pairing our server puts
# in each interleaved reply, next to what chrony computed from it.
#
# The two numbers that matter:
#   turnaround = transmit - receive  (server-side processing; microseconds)
#   age        = now - receive       (how old the reported pair is; ~1 poll)
# If chrony's offset is large while both of these look sane, the fault is in
# which fields we echo, not in which timestamps we pair.

CHRONY_DIR=${CHRONY_DIR:-$HOME/chrony}
RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
WORK=${WORK:-$HOME/xl}
PORT=${PORT:-11555}

rm -rf "$WORK"; mkdir -p "$WORK"; chmod 750 "$WORK"
cd "$WORK" || exit 1

cleanup() {
    [ -n "$CH" ] && kill "$CH" 2>/dev/null
    [ -n "$RT" ] && kill "$RT" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT

RUSTY_TIME_DEBUG_XLEAVE=1 "$RTIMED" serve --stratum 1 --bind "127.0.0.1:$PORT" \
    --ratelimit-interval -4 --ratelimit-burst 64 --ratelimit-global 5000 \
    --control "$WORK/c.sock" > s.log 2> s.err &
RT=$!
sleep 2

cat > c.conf <<EOF
server 127.0.0.1 port $PORT xleave iburst minpoll 2 maxpoll 3
driftfile $WORK/d
pidfile $WORK/p
bindcmdaddress $WORK/cmd.sock
cmdport 0
port 0
EOF

"$CHRONY_DIR/chronyd" -d -x -f "$WORK/c.conf" > ch.log 2>&1 &
CH=$!
sleep 16

echo "=== our interleaved pairing (turnaround should be microseconds) ==="
grep xleave s.err | head -8
echo
echo "=== chrony's verdict ==="
"$CHRONY_DIR/chronyc" -h "$WORK/cmd.sock" ntpdata 2>&1 |
    grep -iE "offset|interleaved|peer delay|root delay"
echo
echo "=== chrony sources ==="
"$CHRONY_DIR/chronyc" -h "$WORK/cmd.sock" -N sources 2>&1 | tail -2
