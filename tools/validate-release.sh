#!/bin/bash
# Pre-release validator for rusty_time.
#
# Answers one question: would shipping THIS commit damage anything? It is run
# against a clean `git worktree` of the exact commit, never against a working
# tree, because a working tree can hold someone else's uncommitted edits and
# those are not what gets published.
#
# Every check is fail-loud with its own line. A `cmd | grep` that swallows an
# exit status is how a gate reports "fine" on a broken build, so nothing here
# pipes a build into a filter without checking ${PIPESTATUS[0]}.
set -u

PASS=0
FAIL=0
WARN=0

ok()   { printf '  \033[32mPASS\033[0m  %s\n' "$1"; PASS=$((PASS+1)); }
bad()  { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; FAIL=$((FAIL+1)); }
warn() { printf '  \033[33mWARN\033[0m  %s\n' "$1"; WARN=$((WARN+1)); }

run() { # run <label> <cmd...>
  local label="$1"; shift
  if "$@" >/tmp/v.$$ 2>&1; then ok "$label"; else bad "$label"; tail -12 /tmp/v.$$ | sed 's/^/          /'; fi
}

echo "=============================================================="
echo " rusty_time release validator"
echo " commit : $(git log --oneline -1)"
echo " tree   : $(git status --porcelain | grep -v validate-release.sh | wc -l) uncommitted file(s)  [must be 0]"
echo "=============================================================="

# --- 0. the tree must BE the commit ---------------------------------------
# Exclude this script: it lives in the tree it validates.
DIRTY=$(git status --porcelain | grep -v "tools/validate-release.sh" | wc -l)
if [ "$DIRTY" -eq 0 ]; then
  ok "working tree clean (validating the commit, not a working copy)"
else
  bad "working tree DIRTY -- this is not what would ship"
  git status --short | grep -v "validate-release.sh" | sed 's/^/          /'
fi

# --- 1. it builds ----------------------------------------------------------
echo "-- build --"
run "cargo build --workspace --release"      cargo build --workspace --release
run "cargo build --workspace --all-targets"  cargo build --workspace --all-targets

# --- 2. it is correct ------------------------------------------------------
echo "-- correctness --"
run "cargo test --workspace"                 cargo test --workspace

# --- 3. it is clean -------------------------------------------------------
echo "-- lint / format --"
run "cargo clippy --workspace --all-targets -- -D warnings" \
    cargo clippy --workspace --all-targets -- -D warnings
run "cargo fmt --all --check"                cargo fmt --all --check

# --- 4. every target CI ships ---------------------------------------------
echo "-- shipping targets (mission plan 6.5) --"
for t in x86_64-pc-windows-msvc wasm32-unknown-unknown; do
  if rustup target list --installed | grep -q "^$t$"; then
    if [ "$t" = "wasm32-unknown-unknown" ]; then
      run "check $t (libs)" cargo check -p rusty_time-core -p rusty_time-nts -p rusty_time-wasm -p rusty_time-api --target "$t"
    else
      run "check $t (workspace)" cargo check --workspace --all-targets --target "$t"
    fi
  else
    warn "target $t not installed locally (CI covers it)"
  fi
done

# --- 5. the no_std leaf, which is 0.2.0's headline ------------------------
echo "-- no_std leaf (both Kairos targets) --"
for t in thumbv7em-none-eabihf riscv32imac-unknown-none-elf; do
  run "leaf --no-default-features @ $t" \
      cargo check -p rusty_time-core --no-default-features --target "$t"
done
run "leaf tests on the no_std code path" cargo test -p rusty_time-core --no-default-features

# --- 6. no unsafe, no debug leftovers ------------------------------------
echo "-- hygiene --"
# The authority on `unsafe` is the compiler: the workspace sets
# `unsafe_code = "deny"` and clippy -D warnings passed above, so a block in any
# crate that has not explicitly lifted the lint could not have built. What a
# grep CAN usefully check is that nobody quietly lifted it.
LIFTED=$(grep -rl 'unsafe_code = "allow"' crates/*/Cargo.toml 2>/dev/null | sed 's|crates/||;s|/Cargo.toml||' | tr '
' ' ')
EXPECTED="rusty_time-clock "
if [ "$LIFTED" = "$EXPECTED" ]; then
  ok "unsafe lint lifted only where expected (${LIFTED% }: the syscall boundary)"
else
  bad "the set of crates lifting unsafe_code CHANGED: got [$LIFTED] expected [$EXPECTED]"
fi
# And nothing this branch adds may introduce one.
if git diff origin/main..HEAD -- '*.rs' | grep -E "^\+" | grep -qE "unsafe[[:space:]]*\{|unsafe fn|unsafe impl"; then
  bad "this branch introduces an unsafe block/fn/impl"
else
  ok "this branch introduces no unsafe block, fn or impl"
fi

LEFT=$(grep -rnE "\b(dbg!|todo!|unimplemented!|panic!\(\"TODO)" crates/*/src --include=*.rs | wc -l)
if [ "$LEFT" -eq 0 ]; then ok "no dbg!/todo!/unimplemented! left in sources"
else bad "$LEFT debug/placeholder macro(s) left in sources"
  grep -rnE "\b(dbg!|todo!|unimplemented!)" crates/*/src --include=*.rs | head -5 | sed 's/^/          /'
fi

# --- 7. it packages, which is what publish actually does -----------------
echo "-- packaging (what crates.io will do) --"
run "cargo package -p rusty_time-core --allow-dirty=false" cargo package -p rusty_time-core
run "cargo doc -p rusty_time-core --no-deps" cargo doc -p rusty_time-core --no-deps

# --- 8. semver against what is already published -------------------------
echo "-- semver vs the published version --"
if command -v cargo-semver-checks >/dev/null 2>&1; then
  if cargo semver-checks check-release -p rusty_time-core >/tmp/sv.$$ 2>&1; then
    ok "semver: $(grep -oE 'no semver update required|minor|major' /tmp/sv.$$ | tail -1)"
  else
    warn "semver-checks reported a change -- read it before choosing the version"
    grep -E "^---|failure|Summary" /tmp/sv.$$ | head -8 | sed 's/^/          /'
  fi
else
  warn "cargo-semver-checks not installed"
fi

echo "=============================================================="
printf " %d passed, %d failed, %d warned\n" "$PASS" "$FAIL" "$WARN"
if [ "$FAIL" -eq 0 ]; then echo " VERDICT: SAFE TO SHIP"; else echo " VERDICT: DO NOT SHIP"; fi
echo "=============================================================="
exit "$FAIL"
