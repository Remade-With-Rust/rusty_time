#!/bin/bash
# Publish the workspace to crates.io, in dependency order, respecting the
# registry's rate limit on new crates.
#
# The order is not cosmetic: crates.io resolves a dependency by looking it up in
# the index, so a crate cannot be published before the crates it depends on.
# The graph is:
#
#     alloc  core  nts  api        (no internal dependencies)
#       clock            <- core
#       wasm             <- core
#       sim              <- core, alloc
#       ctl              <- api, alloc, clock
#       daemon           <- core, clock, nts, api, alloc
#
# crates.io allows a burst of new crates and then one roughly every ten minutes,
# so a first publication of the whole workspace takes well over an hour. This
# waits rather than failing, and re-running it is safe: a version already on the
# registry is reported and skipped.
#
# Usage: tools/publish.sh [--dry-run]

set -u
DRY=${1:-}
ORDER="alloc core nts api clock wasm sim ctl daemon"

# Seconds to leave between two NEW crates. crates.io allows roughly one every
# ten minutes once the initial burst is spent, and it appears to count
# ATTEMPTS: retrying tightly against a 429 pushed the stated deadline out by
# ten minutes each time, which turns a wait into a livelock. So this waits
# generously and never retries fast.
GAP=${GAP:-660}

sleep_until() {   # sleep_until "Thu, 27 Aug 2026 14:28:39 GMT"
    local target now secs
    target=$(date -u -d "$1" +%s 2>/dev/null) || target=0
    now=$(date -u +%s)
    secs=$(( target - now + 30 ))          # margin: the deadline is exclusive
    [ "$secs" -lt 30 ] && secs=30
    echo "     waiting $((secs / 60))m$((secs % 60))s (until $1)"
    sleep "$secs"
}

publish_one() {
    local crate="rusty_time-$1" attempt=0
    while [ "$attempt" -lt 6 ]; do
        attempt=$((attempt + 1))
        local out
        out=$(cargo publish -p "$crate" $DRY 2>&1)

        if ! grep -qE "^error" <<< "$out"; then
            echo "  published $crate"
            return 0
        fi
        if grep -qiE "already (exists|uploaded)" <<< "$out"; then
            echo "  $crate is already on the registry, skipping"
            return 0
        fi
        if grep -q "429 Too Many Requests" <<< "$out"; then
            local when
            when=$(sed -n 's/.*Please try again after \(.*GMT\).*/\1/p' <<< "$out" | head -1)
            echo "  $crate rate limited (attempt $attempt)"
            if [ -n "$when" ]; then sleep_until "$when"; else sleep "$GAP"; fi
            continue
        fi
        echo "  FAILED $crate"
        grep -E "^error" -A5 <<< "$out" | head -12 | sed 's/^/      /'
        return 1
    done
    echo "  gave up on $crate after $attempt attempts"
    return 1
}

echo "publishing rusty_time to crates.io${DRY:+ ($DRY)}"
first=1
for c in $ORDER; do
    # Space the NEW crates out rather than discovering the limit by hitting it.
    [ -z "$DRY" ] && [ "$first" -eq 0 ] && { echo "  pacing $GAP s before rusty_time-$c"; sleep "$GAP"; }
    first=0
    publish_one "$c" || { echo "stopping: rusty_time-$c did not publish"; exit 1; }
done
echo "all crates published"
