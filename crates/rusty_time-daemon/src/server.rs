//! `rtimed serve` — the NTS-capable NTP server.
//!
//! Two listeners over one shared key ring:
//!
//! * **NTS-KE** on TCP 4460: TLS 1.3, ALPN `ntske/1`, exports the session keys
//!   and hands back cookies that encode them.
//! * **NTP** on UDP 123: answers plain NTPv4, and NTS-protected requests by
//!   redeeming the cookie, verifying the authenticator, and replying with fresh
//!   cookies inside the encrypted field.
//!
//! The server keeps **no per-client state** — that is the whole point of the
//! cookie design (RFC 8915 §6), and it is what lets one box answer millions of
//! clients without a session table to exhaust.

use rusty_time_clock::{ClockRead, SystemClock, net};
use rusty_time_core::ntp::{self, HEADER_LEN, LeapIndicator, Mode, NtpPacket, NtpTimestamp};
use rusty_time_core::server::{ClientTable, Disposition, RateLimitConfig, ResponseMode};
use rusty_time_nts::aead::NtsKeys;
use rusty_time_nts::cookie::{COOKIE_NONCE_LEN, KeyRing, MasterKey};
use rusty_time_nts::ef::{self, NONCE_LEN, UNIQUE_ID_LEN};
use rusty_time_nts::records::{self, record_type};
use rusty_time_nts::tls::rustls;
use rusty_time_nts::{AEAD_AES_SIV_CMAC_256, ALPN, NEXT_PROTO_NTPV4};

use crate::store::{MASTER_KEY_SLOTS, Store, StoredMasterKey};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many distinct clients we remember. Bounded on purpose: an unbounded
/// table is a memory-exhaustion lever for anyone spoofing source addresses.
const CLIENT_TABLE_CAPACITY: usize = 16_384;

/// The rate-limiter's notion of "a client": the source **address only**, never
/// the port.
///
/// The source port is chosen by the sender, so keying on `(address, port)`
/// would let anyone reset their own bucket by picking a new ephemeral port —
/// the limiter would be decorative. Found the first time a live test sent 12
/// requests from three short-lived processes and saw zero drops with a burst
/// of 8: three ports looked like three innocent clients. chrony keys on the
/// address for the same reason.
fn client_key(peer: SocketAddr) -> std::net::IpAddr {
    peer.ip()
}

/// Cookies handed out per NTS-KE exchange (RFC 8915 §4.1.6 recommends 8).
const KE_COOKIE_COUNT: usize = 8;
/// Cap on how many cookies one NTP reply may carry, so a request stuffed with
/// placeholders cannot be used as an amplification lever.
const MAX_REPLY_COOKIES: usize = 8;

pub struct ServeOptions {
    pub ntp_bind: String,
    pub ke_bind: String,
    pub stratum: u8,
    /// Serve NTS-KE as well as plain NTP.
    pub nts: bool,
    /// PEM certificate chain and PKCS#8 key. When absent and `nts` is set, a
    /// self-signed certificate is generated for `--nts-name` — development
    /// only; clients must then be told to trust it explicitly.
    pub cert_pem: Option<String>,
    pub key_pem: Option<String>,
    pub nts_name: String,
    /// Write the certificate we serve to this path, so a client on a private
    /// network can be pointed at it with `query --nts-ca`.
    pub write_cert: Option<String>,
    /// SpaceDB state file. When given, the NTS master key ring is persisted
    /// and reloaded, so cookies minted before a restart still redeem after it.
    pub state_path: Option<String>,
    /// Passphrase for the state file (env `RUSTY_TIME_STATE_PASSPHRASE`).
    pub state_passphrase: Option<String>,
    /// Per-client rate limit (chrony's `ratelimit`).
    pub rate_limit: RateLimitConfig,
    /// Where `rtimec` connects: a Unix socket path, or a named pipe on Windows.
    pub control_path: String,
}

impl ServeOptions {
    pub fn parse(args: &[String]) -> Result<ServeOptions, String> {
        let mut opts = ServeOptions {
            ntp_bind: "0.0.0.0:123".into(),
            ke_bind: "0.0.0.0:4460".into(),
            stratum: 1,
            nts: false,
            cert_pem: None,
            key_pem: None,
            nts_name: "localhost".into(),
            write_cert: None,
            state_path: None,
            state_passphrase: std::env::var("RUSTY_TIME_STATE_PASSPHRASE").ok(),
            rate_limit: RateLimitConfig::default(),
            control_path: crate::control::default_path(),
        };
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let value = |v: Option<&String>| -> Result<String, String> {
                v.cloned().ok_or(format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--nts" => opts.nts = true,
                "--bind" => opts.ntp_bind = value(it.next())?,
                "--ke-bind" => opts.ke_bind = value(it.next())?,
                "--nts-name" => opts.nts_name = value(it.next())?,
                "--write-cert" => opts.write_cert = Some(value(it.next())?),
                "--state" => opts.state_path = Some(value(it.next())?),
                "--control" => opts.control_path = value(it.next())?,
                "--ratelimit-interval" => {
                    opts.rate_limit.interval_log2 = value(it.next())?
                        .parse()
                        .map_err(|_| "--ratelimit-interval: not a number".to_string())?;
                }
                "--ratelimit-burst" => {
                    opts.rate_limit.burst = value(it.next())?
                        .parse()
                        .map_err(|_| "--ratelimit-burst: not a number".to_string())?;
                }
                "--ratelimit-global" => {
                    // The backstop that per-client limiting cannot provide once
                    // the client population exceeds the table (see S12b).
                    opts.rate_limit.global_rate_hz = value(it.next())?
                        .parse()
                        .map_err(|_| "--ratelimit-global: not a number".to_string())?;
                    opts.rate_limit.global_burst = opts.rate_limit.global_rate_hz * 2.0;
                }
                "--no-ratelimit" => {
                    // Effectively unlimited: one token per microsecond with a
                    // large burst. Stated as a config, not a special case.
                    opts.rate_limit = RateLimitConfig {
                        interval_log2: -20,
                        burst: 1_000_000,
                        leak_shift: 0,
                        global_rate_hz: 0.0,
                        global_burst: 0.0,
                    };
                }
                "--stratum" => {
                    opts.stratum = value(it.next())?
                        .parse()
                        .map_err(|_| "--stratum: not a number".to_string())?;
                }
                "--cert" => {
                    let path = value(it.next())?;
                    opts.cert_pem =
                        Some(std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?);
                }
                "--key" => {
                    let path = value(it.next())?;
                    opts.key_pem =
                        Some(std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?);
                }
                other => return Err(format!("unknown flag '{other}'")),
            }
        }
        if opts.stratum == 0 || opts.stratum > 15 {
            return Err("--stratum must be 1..=15".into());
        }
        Ok(opts)
    }
}

/// Restore the master key ring from durable state, or mint a fresh one and
/// persist it. Returns the ring and the open store (when state is configured).
fn load_or_create_ring(opts: &ServeOptions) -> Result<(KeyRing, Option<Store>), String> {
    let Some(path) = &opts.state_path else {
        return Ok((fresh_key_ring()?, None));
    };
    let passphrase = opts.state_passphrase.as_deref().ok_or(
        "--state needs a passphrase: set RUSTY_TIME_STATE_PASSPHRASE (the state file holds \
         NTS master keys, which forge every cookie we ever minted)",
    )?;

    let mut store = Store::open(path, passphrase.as_bytes()).map_err(|e| e.to_string())?;
    let stored = store.all_master_keys().map_err(|e| e.to_string())?;

    let mut ring = KeyRing::new(MASTER_KEY_SLOTS);
    if stored.is_empty() {
        let fresh = fresh_key_ring()?;
        if let Some(k) = fresh.current() {
            store
                .put_master_key(
                    0,
                    &StoredMasterKey {
                        id: k.id,
                        key: k.key,
                    },
                )
                .map_err(|e| e.to_string())?;
            store.flush().map_err(|e| e.to_string())?;
        }
        println!("rtimed serve: minted a new NTS master key; state at {path}");
        return Ok((fresh, Some(store)));
    }

    for k in &stored {
        ring.rotate_in(MasterKey {
            id: k.id,
            key: k.key,
        });
    }
    println!(
        "rtimed serve: restored {} NTS master key(s) from {path}; cookies minted before this \
         restart remain valid",
        stored.len()
    );
    Ok((ring, Some(store)))
}

/// Build a key ring with one fresh random master key.
pub fn fresh_key_ring() -> Result<KeyRing, String> {
    let mut ring = KeyRing::new(3);
    let mut key = [0u8; 32];
    rusty_time_nts::ke::fill_random(&mut key).map_err(|e| e.to_string())?;
    let mut id_bytes = [0u8; 4];
    rusty_time_nts::ke::fill_random(&mut id_bytes).map_err(|e| e.to_string())?;
    ring.rotate_in(MasterKey {
        id: u32::from_be_bytes(id_bytes),
        key,
    });
    Ok(ring)
}

pub fn run(opts: &ServeOptions) -> i32 {
    // Load the key ring from durable state when we have some, so cookies
    // minted before a restart still redeem. Without this every restart
    // strands every client holding a cookie, forcing a full NTS-KE round.
    let (ring, store) = match load_or_create_ring(opts) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("rtimed serve: {e}");
            return 1;
        }
    };
    let state = Arc::new(Mutex::new(ServerState {
        clients: ClientTable::new(CLIENT_TABLE_CAPACITY, opts.rate_limit),
        ring,
        stratum: opts.stratum,
        started_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    }));
    // Keep the store alive for the process's lifetime; dropping it would be
    // harmless but the handle documents that state is owned here.
    let _store = store;

    // The control plane: a local socket carrying the same typed ops the mesh
    // will carry later (mission plan §5). Started before the NTP loop so
    // `rtimec` can reach a server that is otherwise busy.
    {
        let ctl_state = Arc::clone(&state);
        let ctl_path = opts.control_path.clone();
        std::thread::spawn(move || {
            if let Err(e) = crate::control::serve(&ctl_path, ctl_state) {
                eprintln!("rtimed serve: control plane unavailable: {e}");
            }
        });
        println!("rtimed: control socket at {}", opts.control_path);
    }

    if opts.nts {
        let tls_config = match server_tls_config(opts) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("rtimed serve: TLS setup: {e}");
                return 1;
            }
        };
        let listener = match TcpListener::bind(&opts.ke_bind) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("rtimed serve: binding NTS-KE {}: {e}", opts.ke_bind);
                return 1;
            }
        };
        let ke_state = Arc::clone(&state);
        // Tell clients where NTP actually is. RFC 8915 §4.1.8 defaults to 123,
        // so a non-standard port must be advertised or clients query nothing.
        let ntp_port = opts
            .ntp_bind
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(123);
        println!("rtimed: NTS-KE listening on {}", opts.ke_bind);
        std::thread::spawn(move || ke_accept_loop(listener, tls_config, ke_state, ntp_port));
    }

    let socket = match UdpSocket::bind(&opts.ntp_bind) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rtimed serve: binding NTP {}: {e}", opts.ntp_bind);
            return 1;
        }
    };
    println!(
        "rtimed: NTP listening on {} (stratum {}, rate limit 1 per {} s burst {})",
        opts.ntp_bind,
        opts.stratum,
        2f64.powi(opts.rate_limit.interval_log2 as i32),
        opts.rate_limit.burst
    );
    ntp_serve_loop(&socket, &state);
    0
}

fn server_tls_config(opts: &ServeOptions) -> Result<Arc<rustls::ServerConfig>, String> {
    use rusty_time_nts::tls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

    let (certs, key): (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) =
        match (&opts.cert_pem, &opts.key_pem) {
            (Some(cert_pem), Some(key_pem)) => {
                let certs = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("parsing --cert: {e}"))?;
                let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
                    .map_err(|e| format!("parsing --key: {e}"))?;
                (certs, key)
            }
            (None, None) => {
                // Development fallback. Announced loudly: a self-signed cert is
                // not a trust anchor any client should silently accept.
                eprintln!(
                    "rtimed serve: no --cert/--key given; generating a SELF-SIGNED certificate \
                     for '{}'. Development only — clients must be told to trust it.",
                    opts.nts_name
                );
                let ck = oxitls_rcgen::generate_self_signed_p256(&[opts.nts_name.as_str()])
                    .map_err(|e| format!("generating self-signed certificate: {e}"))?;
                if let Some(path) = &opts.write_cert {
                    // Only the certificate — never the private key.
                    std::fs::write(path, &ck.cert_pem)
                        .map_err(|e| format!("writing {path}: {e}"))?;
                    println!("rtimed serve: wrote certificate to {path}");
                }
                (
                    vec![CertificateDer::from(ck.cert_der)],
                    PrivateKeyDer::try_from(ck.pkcs8_der)
                        .map_err(|e| format!("self-signed key: {e}"))?,
                )
            }
            _ => return Err("--cert and --key must be given together".into()),
        };

    let config = rusty_time_nts::tls::server_config(certs, key, &[ALPN])
        .map_err(|e| format!("building server TLS config: {e}"))?;
    Ok(Arc::new(config))
}

fn ke_accept_loop(
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    state: Arc<Mutex<ServerState>>,
    ntp_port: u16,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let config = Arc::clone(&config);
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            if let Err(e) = handle_ke(stream, config, state, ntp_port) {
                eprintln!("rtimed serve: NTS-KE session: {e}");
            }
        });
    }
}

fn handle_ke(
    stream: std::net::TcpStream,
    config: Arc<rustls::ServerConfig>,
    state: Arc<Mutex<ServerState>>,
    ntp_port: u16,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;

    let conn = rustls::ServerConnection::new(config).map_err(|e| e.to_string())?;
    let mut tls = rustls::StreamOwned::new(conn, stream);

    // Read the client's records until end-of-message.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 64 * 1024 {
                    return Err("client sent an oversized NTS-KE request".into());
                }
                if records::records(&buf)
                    .any(|r| matches!(r, Ok(rec) if rec.record_type == record_type::END_OF_MESSAGE))
                {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        }
    }

    // Negotiate: we speak NTPv4 over AES-SIV-CMAC-256 and nothing else yet.
    let mut proto_ok = false;
    let mut aead_ok = false;
    for record in records::records(&buf) {
        let record = record.map_err(|e| e.to_string())?;
        match record.record_type {
            record_type::NEXT_PROTOCOL => {
                proto_ok = record
                    .body
                    .chunks_exact(2)
                    .any(|c| u16::from_be_bytes([c[0], c[1]]) == NEXT_PROTO_NTPV4);
            }
            record_type::AEAD_ALGORITHM => {
                aead_ok = record
                    .body
                    .chunks_exact(2)
                    .any(|c| u16::from_be_bytes([c[0], c[1]]) == AEAD_AES_SIV_CMAC_256);
            }
            _ => {}
        }
    }

    if !proto_ok || !aead_ok {
        let mut out = Vec::new();
        // Error code 1: bad request (RFC 8915 §4.1.3).
        records::write_record(&mut out, true, record_type::ERROR, &1u16.to_be_bytes());
        records::write_record(&mut out, true, record_type::END_OF_MESSAGE, &[]);
        let _ = tls.write_all(&out);
        let _ = tls.flush();
        return Ok(());
    }

    // Export the same keys the client derives, then bake them into cookies.
    let keys = export_server_keys(&tls.conn)?;
    let mut out = Vec::new();
    records::write_record(
        &mut out,
        true,
        record_type::NEXT_PROTOCOL,
        &NEXT_PROTO_NTPV4.to_be_bytes(),
    );
    records::write_record(
        &mut out,
        true,
        record_type::AEAD_ALGORITHM,
        &AEAD_AES_SIV_CMAC_256.to_be_bytes(),
    );
    if ntp_port != 123 {
        records::write_record(
            &mut out,
            false,
            record_type::PORT_NEGOTIATION,
            &ntp_port.to_be_bytes(),
        );
    }
    {
        let guard = state.lock().map_err(|_| "server state poisoned")?;
        for _ in 0..KE_COOKIE_COUNT {
            let mut nonce = [0u8; COOKIE_NONCE_LEN];
            rusty_time_nts::ke::fill_random(&mut nonce).map_err(|e| e.to_string())?;
            let cookie = rusty_time_nts::cookie::mint(&guard.ring, &keys, &nonce)
                .map_err(|e| e.to_string())?;
            records::write_record(&mut out, false, record_type::NEW_COOKIE, &cookie);
        }
    }
    records::write_record(&mut out, true, record_type::END_OF_MESSAGE, &[]);

    tls.write_all(&out).map_err(|e| e.to_string())?;
    tls.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn export_server_keys(conn: &rustls::ServerConnection) -> Result<NtsKeys, String> {
    let context_for = |direction: u8| {
        [
            (NEXT_PROTO_NTPV4 >> 8) as u8,
            NEXT_PROTO_NTPV4 as u8,
            (AEAD_AES_SIV_CMAC_256 >> 8) as u8,
            AEAD_AES_SIV_CMAC_256 as u8,
            direction,
        ]
    };
    let label = b"EXPORTER-network-time-security";
    let c2s = conn
        .export_keying_material([0u8; 32], label, Some(&context_for(0x00)))
        .map_err(|e| e.to_string())?;
    let s2c = conn
        .export_keying_material([0u8; 32], label, Some(&context_for(0x01)))
        .map_err(|e| e.to_string())?;
    Ok(NtsKeys { c2s, s2c })
}

/// Everything the NTP responder needs, behind one lock.
///
/// One mutex rather than several: the per-request path touches the client
/// table and the key ring together, and two locks would only add a chance to
/// take them in different orders.
pub struct ServerState {
    pub clients: ClientTable<std::net::IpAddr>,
    pub ring: KeyRing,
    pub stratum: u8,
    pub started_unix: u64,
}

fn ntp_serve_loop(socket: &UdpSocket, state: &Arc<Mutex<ServerState>>) {
    let clock = SystemClock;
    // Batched receive: on Linux this is one recvmmsg per up-to-32 datagrams
    // instead of one recvfrom each. A server that spends its time crossing the
    // kernel boundary is not spending it answering NTP.
    let mut bufs = vec![[0u8; 1024]; net::BATCH_SIZE];
    let mut received = Vec::with_capacity(net::BATCH_SIZE);

    loop {
        match net::wait_readable(socket, Duration::from_millis(500)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => continue,
        }
        let count = match net::recv_batch(socket, &mut bufs, &mut received) {
            Ok(n) => n,
            Err(_) => continue,
        };
        // Receive timestamp as early as possible: everything after this is
        // processing delay the client should not be charged for. One reading
        // covers the batch, which is honest to within the batch's own span.
        let recv_ts = match clock.wall_ns() {
            Ok(ns) => unix_ns_to_ntp(ns),
            Err(_) => continue,
        };

        for i in 0..count {
            let Some(item) = received.get(i).copied() else {
                break;
            };
            let request = &bufs[i][..item.len.min(bufs[i].len())];
            let Some(reply) = build_reply(request, item.peer, recv_ts, state, &clock) else {
                continue;
            };
            if socket.send_to(&reply, item.peer).is_ok() {
                // Read the clock *after* the send returns: that is the closest
                // we get to a real transmit timestamp without hardware
                // timestamping (M7), and it is what interleaved mode reports
                // to this client on its next exchange.
                if let Ok(ns) = clock.wall_ns()
                    && let Ok(mut guard) = state.lock()
                {
                    guard
                        .clients
                        .note_transmit(&client_key(item.peer), unix_ns_to_ntp(ns));
                }
            }
        }
    }
}

/// A Kiss-o'-Death RATE response: stratum 0 carrying a four-character kiss
/// code, which tells a conforming client to back off rather than retry harder.
fn kiss_of_death(request: &NtpPacket, recv_ts: NtpTimestamp) -> Vec<u8> {
    NtpPacket {
        leap: LeapIndicator::NoWarning,
        version: request.version,
        mode: Mode::Server,
        stratum: 0, // stratum 0 marks this as a kiss, not a time source
        poll: request.poll,
        precision: -20,
        root_delay: ntp::NtpShort(0),
        root_dispersion: ntp::NtpShort(0),
        reference_id: *b"RATE",
        reference_ts: NtpTimestamp::ZERO,
        origin_ts: request.transmit_ts,
        receive_ts: recv_ts,
        transmit_ts: recv_ts,
    }
    .to_bytes()
    .to_vec()
}

fn unix_ns_to_ntp(ns: i128) -> NtpTimestamp {
    let secs = (ns / 1_000_000_000) as i64;
    let nanos = (ns % 1_000_000_000) as u32;
    NtpTimestamp::from_unix(secs, nanos)
}

/// Build the response for one request, or `None` if it must be ignored.
///
/// Separated from the socket loop so tests can drive it directly.
pub fn build_reply(
    request: &[u8],
    peer: SocketAddr,
    recv_ts: NtpTimestamp,
    state: &Arc<Mutex<ServerState>>,
    clock: &SystemClock,
) -> Option<Vec<u8>> {
    if request.len() < HEADER_LEN {
        if let Ok(mut guard) = state.lock() {
            guard.clients.note_refused();
        }
        return None;
    }
    let parsed = match NtpPacket::parse(request) {
        Ok(p) => p,
        Err(_) => {
            if let Ok(mut guard) = state.lock() {
                guard.clients.note_refused();
            }
            return None;
        }
    };
    // Only client-mode requests are answered: never reflect a server-mode
    // packet, which is how NTP reflection amplification starts.
    if parsed.mode != Mode::Client {
        if let Ok(mut guard) = state.lock() {
            guard.clients.note_refused();
        }
        return None;
    }

    // Rate limit before any crypto: an unauthenticated flood must not be able
    // to make us do AES work per packet.
    let now_mono = clock.mono_s().ok()?;
    let (disposition, mode) = {
        let mut guard = state.lock().ok()?;
        let key = client_key(peer);
        let disposition = guard.clients.admit(&key, now_mono);
        let mode = if disposition == Disposition::Respond {
            guard.clients.response_mode(&key, parsed.origin_ts)
        } else {
            ResponseMode::Basic
        };
        (disposition, mode)
    };
    match disposition {
        Disposition::Respond => {}
        Disposition::KissOfDeath => return Some(kiss_of_death(&parsed, recv_ts)),
        Disposition::Drop => return None,
    }
    let stratum = state.lock().ok()?.stratum;

    // Is this NTS? Find the cookie and authenticator.
    let mut cookie: Option<&[u8]> = None;
    let mut unique_id: Option<&[u8]> = None;
    let mut placeholders = 0usize;
    let mut has_auth = false;
    for field in ef::fields(request) {
        match field.field_type {
            ef::field_type::NTS_COOKIE => cookie = Some(field.body),
            ef::field_type::UNIQUE_IDENTIFIER => unique_id = Some(field.body),
            ef::field_type::NTS_COOKIE_PLACEHOLDER => placeholders += 1,
            ef::field_type::NTS_AUTHENTICATOR => {
                has_auth = true;
                break;
            }
            _ => {}
        }
    }

    // Interleaved mode reports the PREVIOUS exchange's timestamps, whose
    // transmit value was read after that packet was actually sent. The client
    // pairs them with the T1/T4 it kept, and so gets an exchange whose
    // server-side transmit is not a guess made before transmission.
    // Receive is always THIS exchange's — that is what lets the client
    // interleave again next time. Only the transmit field looks backwards.
    let (mut receive_field, mut transmit_field) = match mode {
        ResponseMode::Basic => (recv_ts, recv_ts),
        ResponseMode::Interleaved { prev_transmit } => (recv_ts, prev_transmit),
    };
    // Bit 0: receive set, transmit clear. Makes the two distinguishable and
    // lets a peer detect interleaved requests statelessly.
    rusty_time_core::server::mark_server_timestamps(&mut receive_field, &mut transmit_field);

    // RUSTY_TIME_DEBUG_XLEAVE=1 prints the pairing an interleaved reply
    // carries. The two numbers that matter: the server-side turnaround
    // (transmit - receive), which should be microseconds, and the age of the
    // pair, which should be about one poll interval.
    if matches!(mode, ResponseMode::Interleaved { .. })
        && std::env::var("RUSTY_TIME_DEBUG_XLEAVE").is_ok()
    {
        eprintln!(
            "xleave: reported_tx_age={:+.6}s (should be ~1 poll interval)",
            recv_ts.seconds_since(transmit_field),
        );
    }

    // The origin field is what tells the client which mode this reply is in.
    // Basic echoes the request's transmit; interleaved echoes the request's
    // *receive*. Getting this wrong does not fail loudly — the client reads
    // the previous exchange's timestamps as if they were this one's, and
    // silently computes an offset that is wrong by a whole poll interval.
    let origin_field = match mode {
        ResponseMode::Basic => parsed.transmit_ts,
        ResponseMode::Interleaved { .. } => parsed.receive_ts,
    };

    let mut header = NtpPacket {
        leap: LeapIndicator::NoWarning,
        version: parsed.version,
        mode: Mode::Server,
        stratum,
        poll: parsed.poll,
        precision: -20,
        root_delay: ntp::NtpShort(0),
        root_dispersion: ntp::NtpShort::from_seconds(0.000_1),
        reference_id: *b"RSTY",
        reference_ts: recv_ts,
        origin_ts: origin_field,
        receive_ts: receive_field,
        transmit_ts: transmit_field,
    };

    // Remember what we received and what we told them, so the next request can
    // be recognised as interleaved.
    if let Ok(mut guard) = state.lock() {
        guard
            .clients
            .note_response(&client_key(peer), recv_ts, receive_field);
    }

    if !has_auth {
        // Plain NTP. In basic mode stamp transmit as late as possible; in
        // interleaved mode the transmit field is the previous exchange's and
        // must not be overwritten.
        if mode == ResponseMode::Basic {
            let mut tx = clock.wall_ns().ok().map(unix_ns_to_ntp)?;
            let mut rx = header.receive_ts;
            rusty_time_core::server::mark_server_timestamps(&mut rx, &mut tx);
            header.transmit_ts = tx;
        }
        return Some(header.to_bytes().to_vec());
    }

    // NTS path: the cookie must redeem and the authenticator must verify
    // before we say anything at all about the time.
    let (cookie, unique_id) = (cookie?, unique_id?);
    let keys = {
        let guard = state.lock().ok()?;
        rusty_time_nts::cookie::redeem(&guard.ring, cookie).ok()?
    };
    verify_client_authenticator(request, &keys.c2s)?;

    // Fresh cookies: one to replace the cookie just spent, plus the
    // placeholders asked for, capped.
    let want = (1 + placeholders).min(MAX_REPLY_COOKIES);
    let mut plaintext = Vec::new();
    {
        let guard = state.lock().ok()?;
        for _ in 0..want {
            let mut nonce = [0u8; COOKIE_NONCE_LEN];
            rusty_time_nts::ke::fill_random(&mut nonce).ok()?;
            let fresh = rusty_time_nts::cookie::mint(&guard.ring, &keys, &nonce).ok()?;
            ef::write_field(&mut plaintext, ef::field_type::NTS_COOKIE, &fresh);
        }
    }

    if mode == ResponseMode::Basic {
        let mut tx = clock.wall_ns().ok().map(unix_ns_to_ntp)?;
        let mut rx = header.receive_ts;
        rusty_time_core::server::mark_server_timestamps(&mut rx, &mut tx);
        header.transmit_ts = tx;
    }
    let mut reply = header.to_bytes().to_vec();
    // Echo the unique identifier so the client can bind reply to request.
    let mut uid = [0u8; UNIQUE_ID_LEN];
    let n = unique_id.len().min(UNIQUE_ID_LEN);
    uid[..n].copy_from_slice(&unique_id[..n]);
    ef::write_field(&mut reply, ef::field_type::UNIQUE_IDENTIFIER, &uid[..n]);

    let mut nonce = [0u8; NONCE_LEN];
    rusty_time_nts::ke::fill_random(&mut nonce).ok()?;
    let ciphertext = rusty_time_nts::aead::seal(&keys.s2c, &[&reply, &nonce], &plaintext).ok()?;
    reply.extend_from_slice(&ef::authenticator_field(&nonce, &ciphertext));
    Some(reply)
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

/// Verify the client's authenticator over the packet preceding it.
fn verify_client_authenticator(request: &[u8], c2s: &[u8; 32]) -> Option<()> {
    let auth = ef::fields(request).find(|f| f.field_type == ef::field_type::NTS_AUTHENTICATOR)?;
    if auth.body.len() < 4 {
        return None;
    }
    let nonce_len = u16::from_be_bytes([auth.body[0], auth.body[1]]) as usize;
    let ct_len = u16::from_be_bytes([auth.body[2], auth.body[3]]) as usize;
    let ct_start = 4 + nonce_len.next_multiple_of(4);
    let ct_end = ct_start.checked_add(ct_len)?;
    if nonce_len == 0 || ct_end > auth.body.len() {
        return None;
    }
    let nonce = auth.body.get(4..4 + nonce_len)?;
    let ciphertext = auth.body.get(ct_start..ct_end)?;
    let aad = request.get(..auth.offset)?;
    rusty_time_nts::aead::open(c2s, &[aad, nonce], ciphertext).ok()?;
    Some(())
}
