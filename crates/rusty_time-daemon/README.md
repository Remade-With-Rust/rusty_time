# rusty_time-daemon (`rtimed`)

The rusty_time daemon — the chronyd analog.

```sh
rtimed sync pool.ntp.org           # discipline the system clock
rtimed serve --nts --stratum 1     # answer NTP and NTS
rtimed query time.cloudflare.com --nts
```

**Client.** Polls its sources, runs the shared `SyncController`, and applies the result
through the platform driver. Offset corrections carry a *budget* and stop when spent.

**Server.** Plain NTPv4 and NTS (RFC 8915), interleaved mode (RFC 9769), per-client and
global token-bucket rate limiting with Kiss-o'-Death, a bounded LRU client table, batched
`recvmmsg`/`sendmmsg`, and an NTP-over-HTTP gateway so a browser can hold real NTP time.

Never an amplifier: a reply is never larger than its request, and the server answers less
than it is asked.

Three independent implementations synchronise from it — chronyd (plain, NTS and
interleaved), ntpd-rs, and the project's own wasm client.

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/master/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
