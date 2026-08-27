#!/bin/bash
# G7 correctness gate: run the libFuzzer targets over the parsers that face
# untrusted bytes.
#
# Every target here parses something an attacker controls — an NTP packet off
# the open internet, an NTS-KE record stream, a config file. The property is
# the same for all of them: no input may panic, and none may be accepted as
# valid when it is not.
set -u
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/../.." || exit 1

SECONDS_PER_TARGET=${SECONDS_PER_TARGET:-60}
TARGETS=${TARGETS:-"ntp_parse config_parse nts_records discipline_loop client_table"}
fail=0

if ! command -v cargo-fuzz >/dev/null 2>&1; then
    echo "cargo-fuzz is not installed; run:"
    echo "  rustup toolchain install nightly"
    echo "  cargo install cargo-fuzz --locked"
    exit 2
fi

echo "== fuzzing (${SECONDS_PER_TARGET}s per target) =="
for target in $TARGETS; do
    echo
    echo "-- $target --"
    if cargo +nightly fuzz run "$target" -- \
        -max_total_time="$SECONDS_PER_TARGET" -print_final_stats=1 \
        2>&1 | tail -12; then
        echo "  PASS  $target survived ${SECONDS_PER_TARGET}s"
    else
        echo "  FAIL  $target — a crash artifact is in fuzz/artifacts/$target"
        fail=1
    fi
done

echo
echo "=========================================="
[ $fail -eq 0 ] && echo "FUZZ: PASS" || echo "FUZZ: FAIL"
exit $fail
