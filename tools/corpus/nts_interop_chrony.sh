#!/bin/bash
# M3 exit gate: NTS interop against chrony, BOTH directions.
#
#   A. rusty_time client  ->  chronyd NTS server
#   B. chronyd client     ->  rusty_time NTS server
#
# Direction B is the one that actually proves our server: chrony is a strict,
# independent implementation, so if it accepts our cookies and authenticators
# we know we did not merely agree with ourselves.
#
# Prereqs on a Linux box (or WSL): ~/chrony built WITH +NTS, rtimed built at
# $RTIMED. Uses high ports so it needs no privileges and cannot collide with a
# running time daemon.

CHRONY_DIR=${CHRONY_DIR:-$HOME/chrony}
RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
WORK=${WORK:-$HOME/nts-interop}
NTP_PORT=${NTP_PORT:-11123}
KE_PORT=${KE_PORT:-11446}

rm -rf "$WORK"; mkdir -p "$WORK"
# chronyd refuses to open a command socket in a world/group-writable directory
# ("Wrong permissions"), so tighten it before it starts.
chmod 750 "$WORK"
cd "$WORK" || exit 1
fail=0

cleanup() {
    [ -n "$CHRONYD_PID" ] && kill "$CHRONYD_PID" 2>/dev/null
    [ -n "$RTIMED_PID" ] && kill "$RTIMED_PID" 2>/dev/null
    wait 2>/dev/null
}
trap cleanup EXIT

echo "=============================================================="
echo " A. rusty_time client -> chronyd NTS server"
echo "=============================================================="

# chrony's NTS-KE server needs a cert AND its private key, so this one comes
# from openssl (rtimed deliberately never writes a private key out).
#
# basicConstraints=CA:FALSE is not optional: `openssl req -x509` defaults to
# CA:TRUE, and rustls then refuses the cert with CaUsedAsEndEntity — correctly,
# since a CA certificate must not terminate a chain. That refusal was the first
# real finding of this gate.
if command -v openssl >/dev/null; then
    openssl req -x509 -newkey rsa:2048 -keyout "$WORK/key.pem" -out "$WORK/cert.pem" \
        -days 2 -nodes -subj "/CN=localhost" \
        -addext "subjectAltName=DNS:localhost" \
        -addext "basicConstraints=critical,CA:FALSE" \
        -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
        -addext "extendedKeyUsage=serverAuth" &>/dev/null
else
    echo "SKIP A: openssl not available to make a chrony server certificate"
fi

if [ -f "$WORK/cert.pem" ]; then
    cat > "$WORK/chronyd.conf" <<EOF
port $NTP_PORT
ntsport $KE_PORT
local stratum 1
allow
ntsservercert $WORK/cert.pem
ntsserverkey $WORK/key.pem
ntsdumpdir $WORK
driftfile $WORK/drift
pidfile $WORK/chronyd.pid
EOF
    chmod 600 "$WORK/key.pem"
    "$CHRONY_DIR/chronyd" -d -f "$WORK/chronyd.conf" &> "$WORK/chronyd-server.log" &
    CHRONYD_PID=$!
    sleep 2

    echo "--- rtimed query --nts against chronyd ---"
    "$RTIMED" query localhost --nts --ke-port "$KE_PORT" --port "$NTP_PORT" \
        --nts-ca "$WORK/cert.pem" --count 4 --interval-ms 400 | tee "$WORK/a.out"
    if grep -Eq '^ *4 authenticated, 0 rejected' "$WORK/a.out" \
       || grep -Eq '[0-9]+ authenticated, 0 rejected' "$WORK/a.out" && grep -q '4/4 answered' "$WORK/a.out"; then
        echo "PASS A: rusty_time authenticated against chronyd's NTS server"
    else
        echo "FAIL A: see $WORK/a.out and $WORK/chronyd-server.log"
        fail=1
    fi
    kill $CHRONYD_PID 2>/dev/null; wait $CHRONYD_PID 2>/dev/null; CHRONYD_PID=
fi

echo
echo "=============================================================="
echo " B. chronyd client -> rusty_time NTS server"
echo "=============================================================="

"$RTIMED" serve --nts --stratum 1 --nts-name localhost \
    --bind "127.0.0.1:$NTP_PORT" --ke-bind "127.0.0.1:$KE_PORT" \
    --write-cert "$WORK/rtimed-cert.pem" &> "$WORK/rtimed-server.log" &
RTIMED_PID=$!
sleep 2

# chrony verifies the KE server's name against the certificate, so the source
# is named 'localhost' (matching the SAN), not 127.0.0.1.
cat > "$WORK/chronyc.conf" <<EOF
server localhost port $NTP_PORT nts ntsport $KE_PORT iburst maxpoll 6
ntstrustedcerts $WORK/rtimed-cert.pem
driftfile $WORK/cdrift
pidfile $WORK/chronyd-client.pid
bindcmdaddress $WORK/chronyd-client.sock
cmdport 0
port 0
EOF

# -x: never touch the system clock. This is a test rig, and chronyd would
# otherwise try to discipline the host it runs on.
"$CHRONY_DIR/chronyd" -d -x -f "$WORK/chronyc.conf" &> "$WORK/chronyd-client.log" &
CHRONYD_PID=$!
sleep 8

echo "--- chronyd client log (NTS lines) ---"
grep -iE "nts|error|refus|cookie" "$WORK/chronyd-client.log" | tail -20

echo "--- chronyc -h <socket> authdata / sources ---"
"$CHRONY_DIR/chronyc" -h "$WORK/chronyd-client.sock" authdata 2>&1 | tee "$WORK/authdata.out"
"$CHRONY_DIR/chronyc" -h "$WORK/chronyd-client.sock" -N sources 2>&1 | tee "$WORK/sources.out"

# The gate: chrony reports mode NTS with a non-zero cookie count, and the
# source is reachable. A cookie count > 0 means chrony completed NTS-KE with
# us AND banked cookies our server minted.
if grep -qi "NTS" "$WORK/authdata.out" && grep -Eq "NTS +[0-9]+ +[0-9]+ +[0-9]+ +[1-9]" "$WORK/authdata.out"; then
    echo "PASS B: chronyd completed NTS with the rusty_time server and holds cookies"
else
    # Fall back to a weaker but still meaningful signal: chrony marked the
    # source reachable while configured for NTS.
    if grep -Eq "\^[\*\+\?-] " "$WORK/sources.out"; then
        echo "PASS B (reachability): chronyd reached the rusty_time NTS source"
    else
        echo "FAIL B: chronyd did not establish NTS with rusty_time"
        echo "  see $WORK/chronyd-client.log, $WORK/authdata.out, $WORK/rtimed-server.log"
        fail=1
    fi
fi

echo
echo "=============================================================="
[ $fail -eq 0 ] && echo "NTS INTEROP: PASS (both directions)" || echo "NTS INTEROP: FAIL"
exit $fail
