#!/bin/bash
# Drive the reference-clock inputs the way gpsd and a PPS daemon actually do.
#
# The GPS/PPS path is the one feature in this project that has never run
# against anything real — no hardware, and the unit tests construct the sample
# structs directly rather than going through the interface a producer uses.
# That leaves the parts most likely to be wrong completely unexercised: the SHM
# segment's layout and its valid/count handshake, and the SOCK datagram's byte
# order and field offsets. Those are ABI contracts with somebody else's program;
# getting them wrong is silent, because a misread segment still yields *a*
# number.
#
# This does not need a GPS. It needs a producer that writes exactly what gpsd
# writes, which is a few dozen lines of Python — and that is the point: the
# consumer is then reading a byte layout produced independently of it, rather
# than one its own tests made up.
#
# What it establishes:
#   * `rtimed refclock shm`  reads a segment written by an outside process,
#     honours the valid/count handshake, and recovers the timestamp exactly.
#   * `rtimed refclock sock` decodes a datagram of the shape chrony's SOCK
#     refclock protocol defines, including the leap field.
#
# What it does NOT establish, and must not be read as: that a real GPS works.
# There is no PPS edge here, no NMEA, no receiver, and nothing about jitter or
# sawtooth correction. This closes "the code path has never been executed by a
# foreign producer". It does not close "this has run on hardware".

set -u
cd "$(dirname "$0")/../.." || exit 1
RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
WORK=${WORK:-$HOME/refclock-loopback}
rm -rf "$WORK"; mkdir -p "$WORK"
fail=0
# A skip is not a pass. Reporting one as the other is how a harness comes to
# certify code it never ran.
skipped=0

echo "== reference clock, driven by a foreign producer =="
echo

# ------------------------------------------------------------------- SHM --
# The layout gpsd and ntpd agree on: mode, count, clockTimeStampSec,
# clockTimeStampUSec, receiveTimeStampSec, receiveTimeStampUSec, leap,
# precision, nsamples, valid, clockTimeStampNSec, receiveTimeStampNSec.
# All ints, native order, in a System V segment keyed 0x4e545030 + unit.
python3 - "$WORK" <<'PY' &
import ctypes, struct, sys, time, os
libc = ctypes.CDLL("libc.so.6", use_errno=True)
UNIT = 2
KEY = 0x4e545030 + UNIT
IPC_CREAT = 0o1000
SIZE = 96

shmget = libc.shmget
shmget.argtypes = [ctypes.c_int, ctypes.c_size_t, ctypes.c_int]
shmat = libc.shmat
shmat.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
shmat.restype = ctypes.c_void_p

shmid = shmget(KEY, SIZE, IPC_CREAT | 0o666)
if shmid < 0:
    print("  SHM: could not create segment:", os.strerror(ctypes.get_errno()))
    sys.exit(2)
addr = shmat(shmid, None, 0)
buf = (ctypes.c_char * SIZE).from_address(addr)

# A known instant, so the consumer's answer can be checked exactly rather than
# "looks about right".
sec, nsec = 1_787_856_000, 123_456_789
count = 8  # even: no write in progress
# The real struct shmTime, as ntpd and gpsd define it. time_t is 64-bit on
# every platform this ships to, which puts clockTimeStampSec at offset 8 as an
# i64 and forces four bytes of padding after clockTimeStampUSec:
#
#   int mode; volatile int count; time_t clkSec; int clkUSec; /* pad */
#   time_t rcvSec; int rcvUSec; int leap; int precision; int nsamples;
#   volatile int valid; unsigned clkNSec; unsigned rcvNSec; int dummy[8];
#
# Writing twelve consecutive ints instead — the obvious guess — puts every
# field after the first two at the wrong offset, and the consumer correctly
# reports no valid sample. That is the whole reason to drive this from an
# independent producer rather than from the consumer's own tests.
layout = "=iiqi4xqiiiiiII"
assert struct.calcsize(layout) == 60, struct.calcsize(layout)
buf[:60] = struct.pack(
    layout,
    1,              # mode
    count,          # count
    sec,            # clockTimeStampSec   (i64)
    nsec // 1000,   # clockTimeStampUSec
    sec,            # receiveTimeStampSec (i64)
    nsec // 1000,   # receiveTimeStampUSec
    0,              # leap
    -20,            # precision
    3,              # nsamples
    1,              # valid
    nsec,           # clockTimeStampNSec
    nsec,           # receiveTimeStampNSec
)
with open(os.path.join(sys.argv[1], "shm.ready"), "w") as f:
    f.write(f"{sec}.{nsec:09d}\n")
time.sleep(6)
PY
producer=$!

for _ in $(seq 1 40); do [ -f "$WORK/shm.ready" ] && break; sleep 0.2; done
if [ -f "$WORK/shm.ready" ]; then
    expected=$(cat "$WORK/shm.ready")
    got=$("$RTIMED" refclock shm 2 2>&1 | head -20)
    echo "-- SHM unit 2, segment written by an outside process --"
    if grep -qE "1787856000|123456789|\.123456" <<< "$got"; then
        echo "   PASS  consumer recovered the producer's timestamp ($expected)"
    else
        echo "   FAIL  consumer did not recover $expected"
        echo "$got" | sed 's/^/         /' | head -6
        fail=1
    fi
else
    echo "-- SHM --"
    echo "   SKIP  the producer never came up — nothing was tested here"
    skipped=1
fi
wait $producer 2>/dev/null

# ------------------------------------------------------------------ SOCK --
# chrony's SOCK refclock datagram: struct sock_sample — tv_sec (i64),
# tv_usec (i64), offset (double), pulse (int), leap (int), _pad (int),
# magic (int) = 0x534f434b.
echo
echo "-- SOCK, datagram of chrony's sock_sample shape --"
SOCKPATH="$WORK/refclock.sock"
"$RTIMED" refclock sock "$SOCKPATH" 4 > "$WORK/sock.out" 2>&1 &
consumer=$!
for _ in $(seq 1 40); do [ -S "$SOCKPATH" ] && break; sleep 0.2; done
if [ -S "$SOCKPATH" ]; then
    python3 - "$SOCKPATH" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
# offset deliberately non-round and leap set, so a field mix-up shows up.
pkt = struct.pack("=qqdiiii", 1_787_856_000, 654_321, -0.001234567, 1, 1, 0, 0x534f434b)
s.sendto(pkt, sys.argv[1])
PY
    sleep 1.5
    kill $consumer 2>/dev/null; wait $consumer 2>/dev/null
    if grep -qE "0.001234|-0.00123|1234567" "$WORK/sock.out"; then
        echo "   PASS  consumer decoded the offset the producer sent"
    else
        echo "   FAIL  offset not recovered from the datagram"
        sed 's/^/         /' "$WORK/sock.out" | head -6
        fail=1
    fi
else
    kill $consumer 2>/dev/null
    echo "   SKIP  consumer did not create the socket"
    skipped=1
    sed 's/^/         /' "$WORK/sock.out" 2>/dev/null | head -4
fi

echo
if [ "$fail" -ne 0 ]; then
    echo "FAIL — a refclock input did not survive contact with a real producer."
    exit 1
elif [ "$skipped" -ne 0 ]; then
    echo "INCOMPLETE — something was skipped, so this run certifies nothing."
    exit 2
else
    echo "PASS — both refclock inputs were read from a foreign producer."
    echo "       This is NOT evidence that a GPS works: no PPS edge, no NMEA,"
    echo "       no receiver. It closes 'never executed', not 'runs on hardware'."
fi
exit 0
