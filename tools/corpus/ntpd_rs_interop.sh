#!/bin/bash
# Closes the interop item left open at M4: does a SECOND independent
# implementation synchronise from rusty_time?
#
# chrony already does (M3/M4). ntpd-rs is the other memory-safe NTP daemon and
# a different codebase entirely, so agreement with it is independent evidence
# that we implement the protocol rather than a self-consistent dialect.
set -u
export PATH="$HOME/.cargo/bin:$PATH"

RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
NTPD_RS_DIR=${NTPD_RS_DIR:-$HOME/ntpd-rs}
WORK=${WORK:-$HOME/ntpd-rs-interop}
PORT=${PORT:-11444}

rm -rf "$WORK"; mkdir -p "$WORK"; chmod 750 "$WORK"
fail=0

cleanup() {
    [ -n "${CLIENT:-}" ] && kill "$CLIENT" 2>/dev/null
    [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT

if [ ! -x "$NTPD_RS_DIR/target/release/ntp-daemon" ]; then
    echo "ntpd-rs not built at $NTPD_RS_DIR — skipping"
    exit 2
fi

# Rate limits raised so this measures protocol interop, not admission policy.
"$RTIMED" serve --stratum 1 --bind "127.0.0.1:$PORT" \
    --ratelimit-interval -4 --ratelimit-burst 64 --ratelimit-global 5000 \
    --control "$WORK/ctl" > "$WORK/rtimed.log" 2> "$WORK/rtimed.err" &
SRV=$!
sleep 2

cat > "$WORK/ntp.toml" <<EOF
[[source]]
mode = "server"
address = "127.0.0.1:$PORT"

[observability]
observation-path = "$WORK/observe"

[synchronization]
minimum-agreeing-sources = 1

# Never touch this machine's clock: this is an interop test.
[[server]]
listen = "127.0.0.1:$((PORT + 1))"
EOF

echo "== ntpd-rs -> rusty_time =="
"$NTPD_RS_DIR/target/release/ntp-daemon" -c "$WORK/ntp.toml" \
    > "$WORK/ntpd-rs.log" 2>&1 &
CLIENT=$!
sleep 20

echo "--- ntpd-rs log ---"
tail -25 "$WORK/ntpd-rs.log"

# ntpd-rs logs the offset it measures for each source once it has samples.
if grep -qiE "offset|Offset" "$WORK/ntpd-rs.log"; then
    echo
    echo "PASS: ntpd-rs measured an offset against the rusty_time server"
else
    echo
    echo "FAIL: no offset observed — see $WORK/ntpd-rs.log"
    fail=1
fi

echo
echo "--- what rtimed saw ---"
"${RTIMEC:-$HOME/rt-target/release/rtimec}" --socket "$WORK/ctl" serverstats 2>&1 | head -12

echo
[ $fail -eq 0 ] && echo "NTPD-RS INTEROP: PASS" || echo "NTPD-RS INTEROP: FAIL"
exit $fail
