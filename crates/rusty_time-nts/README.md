# rusty_time-nts

Network Time Security (RFC 8915) for NTP.

- **`records`** — the NTS-KE record layer.
- **`ef`** — NTS extension fields on NTP packets (RFC 7822 framing), including the
  authenticator.
- **`aead`** — AES-SIV-CMAC-256 seal/open, with a reusable `Sealer` so a reply that mints
  several cookies expands one AES key schedule instead of one per cookie.
- **`cookie`** — server cookies: the server's own encrypted note-to-self holding the
  session keys, so it keeps **no per-client state**.
- **`ke`** — the TLS 1.3 key-establishment client and server (feature `ke`).

No C toolchain: TLS is rustls with `oxitls-rustcrypto-provider`, because rustls's default
provider compiles C. The non-`ke` surface is wasm-clean, so a browser client can speak
authenticated NTS over whatever transport it has.

Interop is verified against chrony in both directions, not just against itself.

## Part of rusty_time

[rusty_time](https://github.com/remade-with-rust/rusty_time) is chrony, remade with Rust:
a pure-Rust NTPv4 + NTS time client and server for Linux, macOS, Windows and wasm, with
no C toolchain anywhere in the build.

Performance claims live in [corpus/LEDGER.md](https://github.com/remade-with-rust/rusty_time/blob/main/corpus/LEDGER.md)
with the run that produced them. Anything not in the ledger is not claimed.

## Licence

MIT OR Apache-2.0.
