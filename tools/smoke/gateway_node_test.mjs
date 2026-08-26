// M6 exit evidence: run the REAL wasm module against a REAL rtimed gateway.
//
// Not a simulation. This loads the compiled wasm, builds genuine NTPv4 packets
// inside it, posts them to the gateway over HTTP, and feeds the replies back
// in — the same path a browser takes, minus the browser.
//
// Usage: node gateway_node_test.mjs <gateway-url> [exchanges]

import { readFile } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const pkgDir = join(here, '..', '..', 'crates', 'rusty_time-wasm', 'pkg');

const url = process.argv[2] ?? 'http://127.0.0.1:8199';
const rounds = Number(process.argv[3] ?? 6);

// Node's ESM loader requires a file:// URL for a dynamic import by path; a
// bare Windows path is rejected as an unknown "c:" scheme.
const wasmModule = await import(pathToFileURL(join(pkgDir, 'rusty_time_wasm.js')).href);
const wasmBytes = await readFile(join(pkgDir, 'rusty_time_wasm_bg.wasm'));
await wasmModule.default({ module_or_path: wasmBytes });

const client = new wasmModule.TimeClient();
let fail = 0;
const check = (ok, what) => {
  console.log(`  ${ok ? 'PASS' : 'FAIL'}  ${what}`);
  if (!ok) fail = 1;
};

console.log('== wasm gateway test ==');
console.log(`   module: ${wasmBytes.length} bytes, gateway: ${url}`);
console.log();

check(!client.is_synchronized(), 'starts unsynchronised');
check(!Number.isFinite(client.confidence_ms(performance.now())),
      'error bound is infinite before the first exchange');

const offsets = [];
for (let i = 0; i < rounds; i++) {
  const rnd = new Uint32Array(2);
  crypto.getRandomValues(rnd);

  const request = client.build_request(Date.now(), rnd[0], rnd[1]);
  if (i === 0) {
    check(request.length === 48, `request is a 48-byte NTP packet (got ${request.length})`);
    // Byte 0: leap 0, version 4, mode 3 (client).
    check(request[0] === ((4 << 3) | 3), 'request header is NTPv4 client mode');
  }

  const res = await fetch(new URL('/time', url), {
    method: 'POST',
    headers: { 'content-type': 'application/octet-stream' },
    body: request,
  });
  if (!res.ok) {
    check(false, `gateway answered ${res.status}`);
    break;
  }
  const reply = new Uint8Array(await res.arrayBuffer());
  const t4 = Date.now();
  const perf = performance.now();

  if (i === 0) {
    check(reply.length >= 48, `reply is an NTP packet (${reply.length} bytes)`);
    check((reply[0] & 0b111) === 4, 'reply is server mode');
  }

  const accepted = client.process_response(reply, t4, perf);
  if (!accepted) {
    check(false, `exchange ${i + 1} rejected`);
    break;
  }
  offsets.push(client.offset_ms(perf));
  await new Promise((r) => setTimeout(r, 120));
}

console.log();
check(client.accepted() === rounds, `all ${rounds} exchanges accepted (${client.accepted()})`);
check(client.rejected() === 0, `no replies rejected (${client.rejected()})`);
check(client.is_synchronized(), 'client is synchronised');

const perf = performance.now();
const offset = client.offset_ms(perf);
const bound = client.confidence_ms(perf);
const corrected = client.now_ms(Date.now(), perf);

console.log();
console.log(`   offset      : ${offset >= 0 ? '+' : ''}${offset.toFixed(3)} ms`);
console.log(`   error bound : ±${bound.toFixed(3)} ms`);
console.log(`   corrected   : ${new Date(corrected).toISOString()}`);
console.log(`   samples     : ${offsets.map((o) => o.toFixed(2)).join(', ')}`);
console.log();

// The gateway shares this machine's clock, so a correct client must measure
// an offset near zero. This is the check that a structurally valid but wrong
// answer cannot pass.
check(Math.abs(offset) < 50, `offset within 50 ms of our own clock (${offset.toFixed(3)} ms)`);
check(Number.isFinite(bound) && bound > 0, 'error bound is finite and positive');
check(Math.abs(corrected - Date.now()) < 1000, 'corrected time is close to wall time');

// A forged reply must not be accepted: flip the origin field so it no longer
// echoes our nonce.
const rnd = new Uint32Array(2);
crypto.getRandomValues(rnd);
const req = client.build_request(Date.now(), rnd[0], rnd[1]);
const res = await fetch(new URL('/time', url), {
  method: 'POST',
  headers: { 'content-type': 'application/octet-stream' },
  body: req,
});
const forged = new Uint8Array(await res.arrayBuffer());
forged[24] ^= 0xff; // first byte of the origin timestamp
const before = client.rejected();
const took = client.process_response(forged, Date.now(), performance.now());
check(!took, 'a reply whose origin does not match our request is refused');
check(client.rejected() === before + 1, 'the rejection is counted');

console.log();
console.log(fail === 0 ? 'WASM GATEWAY: PASS' : 'WASM GATEWAY: FAIL');
process.exit(fail);
