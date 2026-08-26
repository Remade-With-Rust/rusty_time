#!/bin/bash
# Build the npm package for the browser/edge client.
#
# wasm-pack names the package after the crate; the published identity is
# @remade-with-rust/rusty-time (mission plan §6.4), so the manifest is
# rewritten here rather than by renaming the crate — the crate name is what
# every Rust consumer sees, and it should stay conventional.
set -e

cd "$(dirname "$0")/../.."
OUT=${OUT:-crates/rusty_time-wasm/pkg}
TARGET=${TARGET:-web}   # web | bundler | nodejs

echo "building wasm ($TARGET) -> $OUT"
wasm-pack build crates/rusty_time-wasm \
    --target "$TARGET" \
    --out-dir "$(basename "$OUT")" \
    --out-name rusty_time_wasm

# Rewrite the published identity. node is already required to consume this
# package, so using it here adds no new dependency.
node - "$OUT" <<'NODE'
const fs = require('fs');
const path = require('path');
const dir = process.argv[2];
const file = path.join(dir, 'package.json');
const pkg = JSON.parse(fs.readFileSync(file, 'utf8'));

pkg.name = '@remade-with-rust/rusty-time';
pkg.description =
  'Disciplined time for the browser and the edge: NTP-accurate offset ' +
  'estimation with an honest error bound, in wasm. No OS clock required.';
pkg.keywords = ['ntp', 'time', 'clock', 'wasm', 'sntp', 'rusty_time', 'mata'];
pkg.homepage = 'https://github.com/remade-with-rust/rusty_time';
// The .wasm is loaded at runtime, so it must ship even though nothing
// imports it statically.
pkg.files = Array.from(new Set([...(pkg.files ?? []), 'rusty_time_wasm_bg.wasm']));

fs.writeFileSync(file, JSON.stringify(pkg, null, 2) + '\n');
console.log(`  package: ${pkg.name}@${pkg.version}`);
NODE

WASM_BYTES=$(wc -c < "$OUT/rusty_time_wasm_bg.wasm")
echo "  wasm:    $WASM_BYTES bytes"
echo
echo "publish with:  npm publish --access public $OUT"
