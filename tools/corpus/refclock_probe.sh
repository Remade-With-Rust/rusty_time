#!/bin/bash
# M7 evidence: exercise every reference-clock transport this machine can
# actually reach, and say plainly which ones it cannot.
#
# SHM and SOCK are driven by synthetic producers written here, so they are
# tested end to end with no GPS present. PHC is read from real hardware when
# /dev/ptp* exists. PPS and NIC hardware timestamping need a lab box.

RTIMED=${RTIMED:-$HOME/rt-target/release/rtimed}
WORK=${WORK:-$HOME/refclock-probe}
rm -rf "$WORK"; mkdir -p "$WORK"; chmod 750 "$WORK"
fail=0
check() {
    if [ "$1" -eq 0 ]; then printf '  PASS  %s\n' "$2"
    else printf '  FAIL  %s\n' "$2"; fail=1; fi
}

echo "== reference clock probe =="
echo

echo "-- PHC (/dev/ptp*) --"
if [ -r /dev/ptp0 ]; then
    "$RTIMED" refclock phc 0 > "$WORK/phc.out" 2>&1
    rc=$?
    cat "$WORK/phc.out" | sed 's/^/    /'
    # The PHC on a hypervisor tracks host time, so its offset from the guest
    # system clock should be small. A large offset is a real finding, not a
    # pass, so the exit status is what decides.
    check $rc "PHC read and validated"
elif [ -e /dev/ptp0 ]; then
    # Present but not ours to open. That is a fact about this machine, not a
    # defect in the code, and reporting it as a failure makes every unprivileged
    # CI run red for a reason no change here can fix.
    echo "    /dev/ptp0 exists but is not readable by $(id -un) — PENDING (needs privilege)"
else
    echo "    no /dev/ptp* on this machine — PENDING hardware"
fi
echo

echo "-- SOCK (chrony refclock protocol) --"
SOCKPATH="$WORK/sock"
"$RTIMED" refclock sock "$SOCKPATH" 4 > "$WORK/sock.out" 2>&1 &
LISTENER=$!
sleep 1
# A synthetic producer: exactly the struct chrony's SOCK producers send.
python3 - "$SOCKPATH" <<'PY'
import socket, struct, sys, time
path = sys.argv[1]
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
for i in range(3):
    now = time.time()
    sec, usec = int(now), int((now % 1) * 1e6)
    # struct sock_sample { timeval tv; double offset; int pulse, leap, pad, magic; }
    pkt = struct.pack('=qqdiiii', sec, usec, 0.000123, 1, 0, 0, 0x534F434B)
    assert len(pkt) == 40, len(pkt)
    s.sendto(pkt, path)
    time.sleep(0.3)
PY
wait $LISTENER
rc=$?
sed 's/^/    /' "$WORK/sock.out"
check $rc "SOCK producer samples received and validated"
grep -q "offset    : +0.000123" "$WORK/sock.out"
check $? "SOCK offset decoded exactly as sent"
echo

echo "-- SHM (gpsd/ntpd shared memory) --"
# Create the segment and publish a sample the way gpsd would.
python3 - > "$WORK/shm.log" 2>&1 <<'PY'
import ctypes, ctypes.util, struct, time, sys
libc = ctypes.CDLL(ctypes.util.find_library('c'), use_errno=True)
KEY = 0x4E545030  # "NTP0"
SIZE = 96
libc.shmget.restype = ctypes.c_int
shmid = libc.shmget(KEY, SIZE, 0o1000 | 0o666)  # IPC_CREAT | rw
if shmid < 0:
    print("shmget failed", ctypes.get_errno()); sys.exit(1)
libc.shmat.restype = ctypes.c_void_p
addr = libc.shmat(shmid, None, 0)
if addr in (None, ctypes.c_void_p(-1).value):
    print("shmat failed", ctypes.get_errno()); sys.exit(1)
buf = (ctypes.c_char * SIZE).from_address(addr)

now = time.time()
sec, nsec = int(now), int((now % 1) * 1e9)
# Reference is 1.5 ms ahead of our clock, a realistic GPS-ish offset.
ref = now + 0.0015
rsec, rnsec = int(ref), int((ref % 1) * 1e9)

def put(off, fmt, val):
    struct.pack_into(fmt, buf, off, val)

put(0,  '=i', 1)      # mode 1: count-guarded
put(4,  '=i', 1)      # count
put(8,  '=q', rsec)   # clockTimeStampSec (the reference)
put(16, '=i', 0)      # clockTimeStampUSec
put(24, '=q', sec)    # receiveTimeStampSec (ours)
put(32, '=i', 0)      # receiveTimeStampUSec
put(36, '=i', 0)      # leap: none
put(40, '=i', -20)    # precision
put(48, '=i', 1)      # valid
put(52, '=I', rnsec)  # clockTimeStampNSec
put(56, '=I', nsec)   # receiveTimeStampNSec
print(f"published: reference {ref:.9f} local {now:.9f} offset {ref-now:+.9f}")
PY
sed 's/^/    /' "$WORK/shm.log"
"$RTIMED" refclock shm 0 > "$WORK/shm.out" 2>&1
rc=$?
sed 's/^/    /' "$WORK/shm.out"
check $rc "SHM segment read and validated"
grep -qE "offset    : \+0.0014[0-9]|offset    : \+0.0015[0-9]" "$WORK/shm.out"
check $? "SHM offset matches what the producer published (~1.5 ms)"
# Clean the segment up so a rerun starts fresh.
python3 -c "
import ctypes, ctypes.util
libc = ctypes.CDLL(ctypes.util.find_library('c'), use_errno=True)
shmid = libc.shmget(0x4E545030, 96, 0)
if shmid >= 0: libc.shmctl(shmid, 0, None)
" 2>/dev/null
echo

echo "-- PPS (/dev/pps*) --"
ls /dev/pps0 >/dev/null 2>&1 && echo "    present" || echo "    none — PENDING hardware (needs a PPS source)"
echo

echo "-- NIC hardware timestamping --"
if command -v ethtool >/dev/null 2>&1; then
    caps=$(ethtool -T eth0 2>/dev/null | grep -c "hardware-transmit\|hardware-receive")
    if [ "${caps:-0}" -gt 0 ]; then echo "    supported"
    else echo "    software only on this NIC — PENDING hardware"; fi
else
    echo "    ethtool absent — cannot determine"
fi

echo
echo "=========================================="
[ $fail -eq 0 ] && echo "REFCLOCK PROBE: PASS (for the transports this machine has)" \
                || echo "REFCLOCK PROBE: FAIL"
exit $fail
