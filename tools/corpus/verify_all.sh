#!/bin/bash
# One command that runs every gate this machine can run, for a final check
# before calling a milestone done.
#
# Deliberately says PENDING for the gates that need hardware or a network
# baseline, rather than skipping them quietly.
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/../.." || exit 1
fail=0
line() { printf '\n== %s ==\n' "$1"; }

line "workspace tests"
if cargo test --workspace 2>&1 | tail -3 | grep -q "test result: ok"; then
    echo "  PASS"
else
    echo "  FAIL"; fail=1
fi

line "lints"
cargo clippy --workspace --all-targets -- -D warnings >/dev/null 2>&1
[ $? -eq 0 ] && echo "  PASS clippy" || { echo "  FAIL clippy"; fail=1; }
cargo fmt --all --check >/dev/null 2>&1
[ $? -eq 0 ] && echo "  PASS fmt" || { echo "  FAIL fmt"; fail=1; }

line "smoke rig"
if bash tools/smoke/smoke.sh >/tmp/rt_smoke.out 2>&1; then
    echo "  PASS"
else
    echo "  FAIL — see /tmp/rt_smoke.out"; fail=1
fi

line "reference clocks"
if bash tools/corpus/refclock_probe.sh >/tmp/rt_refclock.out 2>&1; then
    echo "  PASS (for the transports this machine has)"
    grep -E "PENDING" /tmp/rt_refclock.out | sed 's/^/    /'
else
    echo "  FAIL — see /tmp/rt_refclock.out"; fail=1
fi

line "deterministic corpus (S12)"
if cargo run --release -q -p rusty_time-sim -- serverload >/tmp/rt_s12.out 2>&1; then
    echo "  PASS"
    grep -E "^S12" /tmp/rt_s12.out | sed 's/^/    /'
else
    echo "  FAIL"; fail=1
fi

line "gates that need something this machine lacks"
echo "  PENDING  HW1 lab corpus            (needs GPS + PPS ground truth)"
echo "  PENDING  PPS refclock              (no /dev/pps*)"
echo "  PENDING  NIC hardware timestamping (NIC reports none)"
echo "  PENDING  chrony performance baseline (G1-G6; rig proven, runs not done)"
echo "  (fuzzing and chrony interop are separate: run_fuzzers.sh,"
echo "   nts_interop_chrony.sh, m4_interop_chrony.sh)"

printf '\n==========================================\n'
[ $fail -eq 0 ] && echo "VERIFY: PASS" || echo "VERIFY: FAIL"
exit $fail
