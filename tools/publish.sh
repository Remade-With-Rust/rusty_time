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

publish_one() {
    local crate="rusty_time-$1"
    while :; do
        local out
        out=$(cargo publish -p "$crate" $DRY 2>&1)

        if ! grep -qE "^error" <<< "$out"; then
            echo "  published $crate"
            return 0
        fi
        # Already there: fine, and re-running must not be an error.
        if grep -qiE "already (exists|uploaded)" <<< "$out"; then
            echo "  $crate is already on the registry, skipping"
            return 0
        fi
        # Rate limited: the message carries the time to come back.
        if grep -q "429 Too Many Requests" <<< "$out"; then
            local when secs
            when=$(sed -n 's/.*Please try again after \(.*GMT\).*/\1/p' <<< "$out" | head -1)
            secs=$(( $(date -u -d "$when" +%s 2>/dev/null || echo 0) - $(date -u +%s) ))
            [ "$secs" -lt 30 ] && secs=60
            echo "  $crate rate limited; waiting $((secs / 60))m$((secs % 60))s (until $when)"
            sleep "$((secs + 10))"
            continue
        fi
        echo "  FAILED $crate"
        grep -E "^error" -A5 <<< "$out" | head -12 | sed 's/^/      /'
        return 1
    done
}

echo "publishing rusty_time to crates.io${DRY:+ ($DRY)}"
for c in $ORDER; do
    publish_one "$c" || { echo "stopping: rusty_time-$c did not publish"; exit 1; }
done
echo "all crates published"
