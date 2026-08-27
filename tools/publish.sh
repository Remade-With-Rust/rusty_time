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
# crates.io enforces TWO different limits, and confusing them costs an hour:
#
#   * a brand-NEW crate  — a small burst, then one roughly every ten minutes
#   * a new VERSION of a crate that already exists — a burst of about thirty,
#     then one a minute
#
# So the first publication of this workspace took over an hour and every
# release since is a couple of minutes. GAP defaults to the version-bump case
# because that is the normal one; pass FIRST=1 for a workspace that has never
# been published. Getting it wrong is not fatal either way — a 429 is caught
# below and waited out — it just wastes time.
#
# Re-running is safe: a version already on the registry is reported and skipped.
#
# Usage: tools/publish.sh [--dry-run]
#        FIRST=1 tools/publish.sh        # first ever publication, slow pacing

set -u
DRY=${1:-}
ORDER="alloc core nts api clock wasm sim ctl daemon"

# Seconds to leave between publishes. The registry appears to count ATTEMPTS,
# not successes: retrying tightly against a 429 pushed the stated deadline out
# by ten minutes each time, which turns a wait into a livelock. So this never
# retries fast, whichever limit is in play.
if [ "${FIRST:-0}" = "1" ]; then
    GAP=${GAP:-680}
else
    GAP=${GAP:-20}
fi

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
            # Each verification build gets its own dependency tree under
            # target/package, and they are not shared between crates. Nine of
            # them will fill a disk that had room for one. It is pure scratch
            # once the crate is up.
            rm -rf target/package 2>/dev/null
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
        # A crate that depends on one published seconds ago can fail to verify
        # simply because the index has not caught up yet. That is a wait, not a
        # failure, and it is the normal case when a workspace is released in
        # dependency order.
        if grep -qE "failed to select a version|no matching package|could not find" <<< "$out"; then
            echo "  $crate: dependency not in the index yet (attempt $attempt)"
            cargo search rusty_time-core >/dev/null 2>&1   # nudge the index
            sleep 30
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
