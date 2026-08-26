#!/bin/bash
# TIMECORP clknetsim arm — interception probe (mission plan §7.1, M2 open item).
#
# Runs a two-node clknetsim world: node 1 is chronyd serving `local stratum 1`,
# node 2 is rtimed doing a one-shot query, launched exactly as clknetsim.bash
# launches its known clients (LD_PRELOAD + CLKNETSIM_NODE/SOCKET). Node 2's
# clock is configured 50 ms off with +20 ppm frequency error; the probe PASSES
# if rtimed's measured offset agrees with the configured offset, proving the
# preload fully intercepts a Rust std binary's clock and socket calls.
#
# Prereqs (a Linux box or WSL): ~/clknetsim and ~/chrony built from source,
# rtimed built at $RTIMED. Override any path via environment.
# (No `set -u`: clknetsim.bash relies on unset optional positionals.)

export CLKNETSIM_PATH=${CLKNETSIM_PATH:-$HOME/clknetsim}
export CLKNETSIM_TMPDIR=${CLKNETSIM_TMPDIR:-$HOME/cksim-tmp}
RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
CHRONY_DIR=${CHRONY_DIR:-$HOME/chrony}

mkdir -p "$CLKNETSIM_TMPDIR"
rm -f "$CLKNETSIM_TMPDIR"/*
. "$CLKNETSIM_PATH/clknetsim.bash"

# Network + clock config: node 2 starts 50 ms ahead, +20 ppm, ~100 us one-way.
cat > "$CLKNETSIM_TMPDIR/conf" <<EOF
node2_offset = 0.05
node2_freq = 20e-6
node2_delay1 = (+ 100e-6 (* 10e-6 (exponential)))
node1_delay2 = (+ 100e-6 (* 10e-6 (exponential)))
EOF

PATH="$CHRONY_DIR:$PATH"
start_client 1 chronyd "local stratum 1"

# rtimed is not in clknetsim.bash's client table; launch it the same way by hand.
LD_PRELOAD="$CLKNETSIM_PATH/clknetsim.so" \
    CLKNETSIM_NODE=2 CLKNETSIM_SOCKET="$CLKNETSIM_TMPDIR/sock" \
    "$RTIMED" query 192.168.123.1 --count 6 --interval-ms 2000 --timeout-ms 4000 \
    &> "$CLKNETSIM_TMPDIR/log.2" &
client_pids="$client_pids $!"
disown $!

start_server 2 -l 60

echo "=== rtimed log (node 2, simulated) ==="
cat "$CLKNETSIM_TMPDIR/log.2"
echo "=== verdict ==="
# The configured 50 ms offset must appear in the measurement (sign per θ
# convention: node 2 is ahead, so rtimed should want to SUBTRACT ~0.05 s).
if grep -Eq -- '-0\.0(49|50|51)' "$CLKNETSIM_TMPDIR/log.2"; then
    echo "PASS: rtimed measured the configured 50 ms offset inside the simulation"
    exit 0
else
    echo "FAIL: rtimed's measurement does not match the configured offset"
    exit 1
fi
