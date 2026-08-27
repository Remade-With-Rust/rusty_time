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
use rusty_time_core::server::{
    ClientHandle, ClientTable, Disposition, RateLimitConfig, ResponseMode,
};
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

/// How often the service-manager status line is refreshed.
const STATUS_INTERVAL: Duration = Duration::from_secs(60);

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
/// Roughly the wire size of one cookie extension field, for pre-sizing reply
/// buffers. An over-estimate costs a few bytes; an under-estimate costs a
/// reallocate-and-copy, which is what this exists to avoid.
const COOKIE_FIELD_HINT: usize = 112;

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
    /// HTTP gateway address, when the browser door should be open.
    pub gateway_bind: Option<String>,
    /// Directory holding the built wasm assets the status page loads.
    pub gateway_assets: Option<String>,
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
            gateway_bind: None,
            gateway_assets: None,
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
                "--gateway" => opts.gateway_bind = Some(value(it.next())?),
                "--gateway-assets" => opts.gateway_assets = Some(value(it.next())?),
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
        // Print what it *resolved to*, not what was typed: on Windows a path
        // becomes a loopback port, and an operator who cannot see that has no
        // way to tell why `rtimec` is not connecting.
        match rusty_time_api::control_endpoint(&opts.control_path) {
            rusty_time_api::ControlEndpoint::UnixPath(p) => {
                println!("rtimed: control socket at {p}")
            }
            rusty_time_api::ControlEndpoint::Loopback(port) => println!(
                "rtimed: control on 127.0.0.1:{port} (from '{}')",
                opts.control_path
            ),
        }
    }

    // The browser door. Started before the UDP loop so a page can reach it
    // even while the server is busy.
    if let Some(bind) = &opts.gateway_bind {
        let gw_state = Arc::clone(&state);
        let bind = bind.clone();
        let assets = opts.gateway_assets.clone();
        println!(
            "rtimed: gateway (NTP over HTTP) on http://{bind}/{}",
            if assets.is_some() {
                " with wasm assets"
            } else {
                " (status page only; pass --gateway-assets for the wasm demo)"
            }
        );
        std::thread::spawn(move || {
            if let Err(e) = crate::gateway::serve(&bind, gw_state, assets) {
                eprintln!("rtimed serve: gateway unavailable: {e}");
            }
        });
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

    // Prefer a socket the service manager already opened. Under systemd socket
    // activation that socket was bound as root on port 123, which lets the
    // daemon itself run without the privilege to bind it.
    let socket = match crate::service::activated_udp_socket() {
        Some(s) => {
            println!("rtimed: using the socket passed by the service manager");
            s
        }
        None => match UdpSocket::bind(&opts.ntp_bind) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("rtimed serve: binding NTP {}: {e}", opts.ntp_bind);
                return 1;
            }
        },
    };
    println!(
        "rtimed: NTP listening on {} (stratum {}, rate limit 1 per {} s burst {})",
        opts.ntp_bind,
        opts.stratum,
        2f64.powi(opts.rate_limit.interval_log2 as i32),
        opts.rate_limit.burst
    );

    // Announce readiness only now: everything a client can reach is up, so a
    // supervisor that waits for this is waiting for something true.
    let caps = rusty_time_clock::capabilities();
    crate::service::notify_ready(&format!(
        "serving NTP on {} (stratum {}{})",
        opts.ntp_bind,
        opts.stratum,
        if caps.can_discipline {
            ""
        } else {
            ", clock read-only"
        }
    ));

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
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .any(|c| u16::from_be_bytes(*c) == NEXT_PROTO_NTPV4);
            }
            record_type::AEAD_ALGORITHM => {
                aead_ok = record
                    .body
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .any(|c| u16::from_be_bytes(*c) == AEAD_AES_SIV_CMAC_256);
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
    // Allocated once and reused for every receive, like the buffers above.
    // No control data: this loop timestamps the batch itself and never reads
    // the per-datagram kernel stamp, so asking for one is work the kernel does
    // for nobody. (Reading it instead would be an accuracy improvement worth
    // its cost -- a separate change, not this one.)
    let mut scratch = net::BatchScratch::without_timestamps();
    let mut send_scratch = net::BatchScratch::without_timestamps();
    // Measurement arm, resolved ONCE here rather than per reply: an A/B switch
    // inside the loop being timed is overhead added in order to take the
    // measurement. Set RUSTY_TIME_NO_BATCH_SEND=1 to answer with one `sendto`
    // per reply, which is what this server did before batching, so the two can
    // be compared inside a single run instead of across runs whose absolute
    // throughput drifts by tens of percent.
    let batch_send = std::env::var_os("RUSTY_TIME_NO_BATCH_SEND").is_none();
    // How many datagrams a receive actually collects. A send-batching change
    // cannot pay if the batches are one packet deep, and "the toggle changed
    // nothing" reads the same whether the idea is wrong or the arm is never
    // exercised -- so the depth is counted rather than assumed.
    let report_batches = std::env::var_os("RUSTY_TIME_BATCH_STATS").is_some();
    let mut batch_calls: u64 = 0;
    let mut batch_datagrams: u64 = 0;
    let mut batch_max: usize = 0;
    let mut next_batch_report = std::time::Instant::now() + Duration::from_secs(1);
    let mut replies: Vec<(Reply, SocketAddr)> = Vec::with_capacity(net::BATCH_SIZE);
    // Refresh the supervisor's one-line status occasionally, so `systemctl
    // status` shows what the server is actually doing rather than only what it
    // said at startup. Cheap: once a minute, off the hot path.
    let mut next_status = std::time::Instant::now() + STATUS_INTERVAL;

    loop {
        if std::time::Instant::now() >= next_status {
            next_status = std::time::Instant::now() + STATUS_INTERVAL;
            if let Ok(guard) = state.lock() {
                let stats = guard.clients.stats;
                crate::service::notify_status(&format!(
                    "{} requests, {} answered, {} rate-limited, {} clients",
                    stats.requests,
                    stats.responses,
                    stats.dropped_rate_limit,
                    guard.clients.len()
                ));
            }
        }

        // Always poll before receiving. Skipping it after a full batch, on the
        // theory that a backlog makes the answer obvious, was tried and
        // measured 0.4% WORSE: `poll` is cheaper than the extra `recvmmsg`
        // that comes back empty when the guess is wrong.
        match net::wait_readable(socket, Duration::from_millis(500)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => continue,
        }
        let count = match net::recv_batch(socket, &mut bufs, &mut scratch, &mut received) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if count == 0 {
            continue;
        }
        if report_batches {
            batch_calls += 1;
            batch_datagrams += count as u64;
            batch_max = batch_max.max(count);
            if std::time::Instant::now() >= next_batch_report && batch_calls > 0 {
                next_batch_report = std::time::Instant::now() + Duration::from_secs(1);
                eprintln!(
                    "batch: {} receives, {} datagrams, mean {:.2}, max {}",
                    batch_calls,
                    batch_datagrams,
                    batch_datagrams as f64 / batch_calls as f64,
                    batch_max
                );
            }
        }
        // Receive timestamp as early as possible: everything after this is
        // processing delay the client should not be charged for. One reading
        // covers the batch, which is honest to within the batch's own span.
        let recv_ts = match clock.wall_parts() {
            Ok((s, n)) => unix_parts_to_ntp(s, n),
            Err(_) => continue,
        };

        // Build every reply first, then send the whole batch in one syscall.
        //
        // The receive side has been one `recvmmsg` since M5 while the reply
        // side was one `sendto` per packet, so a batch of 32 requests cost 1
        // receive syscall and 32 send syscalls. It also took the state lock
        // and read the clock once per reply, after each individual send.
        replies.clear();
        if let Ok(mut guard) = state.lock() {
            for i in 0..count {
                let Some(item) = received.get(i).copied() else {
                    break;
                };
                let request = &bufs[i][..item.len.min(bufs[i].len())];
                if let Some(reply) = build_reply_in(request, item.peer, recv_ts, &mut guard, &clock)
                {
                    replies.push((reply, item.peer));
                }
            }
        }
        if replies.is_empty() {
            continue;
        }
        let sent = if batch_send {
            // Read straight out of `replies`: no temporary vector describing
            // data the loop already holds.
            net::send_batch_by(
                socket,
                &replies,
                |(reply, _)| reply.bytes.as_slice(),
                |(_, peer)| *peer,
                &mut send_scratch,
            )
            .unwrap_or(0)
        } else {
            let mut n = 0;
            for (reply, peer) in &replies {
                if socket.send_to(reply.bytes.as_slice(), peer).is_err() {
                    break;
                }
                n += 1;
            }
            n
        };

        // One clock reading for the batch, taken straight after the syscall
        // that put every one of these packets on the wire. This is not a loss
        // of precision against the old per-send stamp — it is a gain: the
        // packets really did all leave in one syscall, whereas the old loop
        // charged each client the accumulated cost of every send before it.
        if sent > 0
            && let Ok((s, n)) = clock.wall_parts()
        {
            let transmit = unix_parts_to_ntp(s, n);
            // One lock for the batch, not one per reply. Addressed by handle,
            // so none of these costs a hash either.
            if let Ok(mut guard) = state.lock() {
                for (reply, _) in replies.iter().take(sent) {
                    guard.clients.note_transmit_at(reply.handle, transmit);
                }
            }
        }
    }
}

/// A Kiss-o'-Death RATE response: stratum 0 carrying a four-character kiss
/// code, which tells a conforming client to back off rather than retry harder.
fn kiss_of_death(request: &NtpPacket, recv_ts: NtpTimestamp) -> [u8; HEADER_LEN] {
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
}

/// Whether interleaved-mode debug output is on. Read from the environment
/// once; it cannot change while the process runs.
fn debug_xleave() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("RUSTY_TIME_DEBUG_XLEAVE").is_ok())
}

/// Unix (seconds, nanoseconds) to an NTP timestamp.
///
/// Takes the parts the OS already had rather than a nanosecond count, so
/// nothing has to divide a 128-bit integer back apart to recover them — that
/// division is a software routine, and it measured 5% of everything the server
/// did per reply.
fn unix_parts_to_ntp(secs: i64, nanos: u32) -> NtpTimestamp {
    NtpTimestamp::from_unix(secs, nanos)
}

/// The bytes of one reply.
///
/// A plain NTP response is always exactly 48 bytes, so it travels in the
/// enum rather than on the heap: the server used to `to_vec()` a fixed-size
/// array once per request, which is an allocation and a free per packet for a
/// value whose size is known at compile time. Only NTS replies, which carry
/// cookies and an authenticator of variable length, actually need a `Vec`.
pub enum ReplyBytes {
    Plain([u8; HEADER_LEN]),
    Extended(Vec<u8>),
}

impl ReplyBytes {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            ReplyBytes::Plain(buf) => buf,
            ReplyBytes::Extended(v) => v,
        }
    }
}

/// One built reply, plus the handle addressing the client it is for.
///
/// The handle is carried out so the caller can record the true transmit time
/// after `send` without hashing the address a second time.
pub struct Reply {
    pub bytes: ReplyBytes,
    pub handle: ClientHandle,
}

/// So a `Reply` can be used anywhere the bytes were used before — the tests
/// that check what goes on the wire are unchanged by the switch away from
/// `Vec`, which is what makes them evidence that nothing else changed either.
impl std::ops::Deref for Reply {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.bytes.as_slice()
    }
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
) -> Option<Reply> {
    let mut guard = state.lock().ok()?;
    build_reply_in(request, peer, recv_ts, &mut guard, clock)
}

/// `build_reply` against state the caller has already locked.
///
/// The UDP loop answers a whole batch at a time, so it takes the lock once for
/// the batch rather than once per request — up to 32 acquisitions collapsed
/// into one. The locking wrapper above stays for the gateway, whose requests
/// arrive one per connection on their own threads.
///
/// The trade is that a batch of NTS replies now holds the lock across their
/// cryptography. That is acceptable here because the UDP responder is the only
/// writer and the gateway is a browser-facing path where a few hundred
/// microseconds is invisible; it would need revisiting if the responder ever
/// became multi-threaded.
pub fn build_reply_in(
    request: &[u8],
    peer: SocketAddr,
    recv_ts: NtpTimestamp,
    guard: &mut ServerState,
    clock: &SystemClock,
) -> Option<Reply> {
    if request.len() < HEADER_LEN {
        guard.clients.note_refused();
        return None;
    }
    let parsed = match NtpPacket::parse(request) {
        Ok(p) => p,
        Err(_) => {
            guard.clients.note_refused();
            return None;
        }
    };
    // Only client-mode requests are answered: never reflect a server-mode
    // packet, which is how NTP reflection amplification starts.
    if parsed.mode != Mode::Client {
        guard.clients.note_refused();
        return None;
    }

    // Rate limit before any crypto: an unauthenticated flood must not be able
    // to make us do AES work per packet.
    let now_mono = clock.mono_s().ok()?;

    // ONE lock for the whole per-client path. This used to take the mutex four
    // separate times per request -- admit, stratum, note_response, and
    // note_transmit in the caller -- three of which were re-acquiring it
    // immediately after releasing it. Everything inside is either a table
    // operation or pure arithmetic on values the table just produced, so
    // there is nothing to be gained by letting go in between.
    let key = client_key(peer);
    let locked = {
        let (disposition, handle) = guard.clients.admit_handle(&key, now_mono);
        if disposition != Disposition::Respond {
            (
                disposition,
                handle,
                ResponseMode::Basic,
                0u8,
                recv_ts,
                recv_ts,
            )
        } else {
            let mode = guard.clients.response_mode_at(handle, parsed.origin_ts);
            let stratum = guard.stratum;
            // Interleaved mode reports the PREVIOUS exchange's transmit, whose
            // value was read after that packet actually went out. Receive is
            // always THIS exchange's -- that is what lets the client
            // interleave again next time. Only transmit looks backwards.
            let (mut receive_field, mut transmit_field) = match mode {
                ResponseMode::Basic => (recv_ts, recv_ts),
                ResponseMode::Interleaved { prev_transmit } => (recv_ts, prev_transmit),
            };
            // Bit 0: receive set, transmit clear. Makes the two distinguishable
            // and lets a peer detect interleaved requests statelessly.
            rusty_time_core::server::mark_server_timestamps(
                &mut receive_field,
                &mut transmit_field,
            );
            // Remember what we received and what we told them, so the next
            // request can be recognised as interleaved.
            guard
                .clients
                .note_response_at(handle, recv_ts, receive_field);
            (
                disposition,
                handle,
                mode,
                stratum,
                receive_field,
                transmit_field,
            )
        }
    };
    let (disposition, handle, mode, stratum, receive_field, transmit_field) = locked;
    match disposition {
        Disposition::Respond => {}
        Disposition::KissOfDeath => {
            return Some(Reply {
                bytes: ReplyBytes::Plain(kiss_of_death(&parsed, recv_ts)),
                handle,
            });
        }
        Disposition::Drop => return None,
    }

    // Is this NTS? Find the cookie and authenticator. No length guard here on
    // purpose: `ef::fields` already clamps its start to the packet length and
    // yields nothing for a bare 48-byte header, so a guard would only add a
    // branch to buy a check that already happens.
    let mut cookie: Option<&[u8]> = None;
    let mut unique_id: Option<&[u8]> = None;
    let mut placeholders = 0usize;
    // The authenticator field itself, not just whether there was one: the
    // verifier needs its body and offset, and finding them again means walking
    // every extension field a second time on a path that has just walked them.
    let mut auth_field: Option<ef::Field> = None;
    for field in ef::fields(request) {
        match field.field_type {
            ef::field_type::NTS_COOKIE => cookie = Some(field.body),
            ef::field_type::UNIQUE_IDENTIFIER => unique_id = Some(field.body),
            ef::field_type::NTS_COOKIE_PLACEHOLDER => placeholders += 1,
            ef::field_type::NTS_AUTHENTICATOR => {
                auth_field = Some(field);
                break;
            }
            _ => {}
        }
    }

    // RUSTY_TIME_DEBUG_XLEAVE=1 prints the pairing an interleaved reply
    // carries. The two numbers that matter: the server-side turnaround
    // (transmit - receive), which should be microseconds, and the age of the
    // pair, which should be about one poll interval.
    //
    // The variable is read once per process, not once per request: `env::var`
    // walks the environment and allocates a String, which is real work to do
    // on a hot path for a flag that cannot change while the process runs.
    if matches!(mode, ResponseMode::Interleaved { .. }) && debug_xleave() {
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

    let Some(auth_field) = auth_field else {
        // Plain NTP. In basic mode stamp transmit as late as possible; in
        // interleaved mode the transmit field is the previous exchange's and
        // must not be overwritten.
        if mode == ResponseMode::Basic {
            let mut tx = clock
                .wall_parts()
                .ok()
                .map(|(s, n)| unix_parts_to_ntp(s, n))?;
            let mut rx = header.receive_ts;
            rusty_time_core::server::mark_server_timestamps(&mut rx, &mut tx);
            header.transmit_ts = tx;
        }
        return Some(Reply {
            bytes: ReplyBytes::Plain(header.to_bytes()),
            handle,
        });
    };

    // NTS path: the cookie must redeem and the authenticator must verify
    // before we say anything at all about the time.
    let (cookie, unique_id) = (cookie?, unique_id?);
    let keys = rusty_time_nts::cookie::redeem(&guard.ring, cookie).ok()?;
    verify_client_authenticator(request, auth_field, &keys.c2s)?;

    // Fresh cookies: one to replace the cookie just spent, plus the
    // placeholders asked for, capped.
    let want = (1 + placeholders).min(MAX_REPLY_COOKIES);
    // Sized up front: a cookie extension field is ~104 bytes, so growing from
    // empty walks through several reallocate-and-copy rounds per reply.
    let mut plaintext = Vec::with_capacity(want * COOKIE_FIELD_HINT);
    // One draw from the OS for every cookie nonce, not one per cookie: this
    // used to be up to eight separate `getrandom` calls per reply for 128
    // bytes of entropy. The nonces are still independent — they are distinct
    // slices of one random buffer.
    let mut nonce_bytes = [0u8; COOKIE_NONCE_LEN * MAX_REPLY_COOKIES];
    rusty_time_nts::ke::fill_random(&mut nonce_bytes[..want * COOKIE_NONCE_LEN]).ok()?;
    let mut nonces = [[0u8; COOKIE_NONCE_LEN]; MAX_REPLY_COOKIES];
    for (i, slot) in nonces.iter_mut().enumerate().take(want) {
        slot.copy_from_slice(&nonce_bytes[i * COOKIE_NONCE_LEN..(i + 1) * COOKIE_NONCE_LEN]);
    }
    {
        // One key schedule and no per-cookie allocation, writing the fields
        // straight into the plaintext buffer.
        rusty_time_nts::cookie::mint_fields_into(
            &guard.ring,
            &keys,
            &nonces[..want],
            &mut plaintext,
        )
        .ok()?;
    }

    if mode == ResponseMode::Basic {
        let mut tx = clock
            .wall_parts()
            .ok()
            .map(|(s, n)| unix_parts_to_ntp(s, n))?;
        let mut rx = header.receive_ts;
        rusty_time_core::server::mark_server_timestamps(&mut rx, &mut tx);
        header.transmit_ts = tx;
    }
    let mut reply = Vec::with_capacity(HEADER_LEN + 64 + want * COOKIE_FIELD_HINT + 64);
    reply.extend_from_slice(&header.to_bytes());
    // Echo the unique identifier so the client can bind reply to request.
    let mut uid = [0u8; UNIQUE_ID_LEN];
    let n = unique_id.len().min(UNIQUE_ID_LEN);
    uid[..n].copy_from_slice(&unique_id[..n]);
    ef::write_field(&mut reply, ef::field_type::UNIQUE_IDENTIFIER, &uid[..n]);

    let mut nonce = [0u8; NONCE_LEN];
    rusty_time_nts::ke::fill_random(&mut nonce).ok()?;
    let ciphertext = rusty_time_nts::aead::seal(&keys.s2c, &[&reply, &nonce], &plaintext).ok()?;
    ef::write_authenticator(&mut reply, &nonce, &ciphertext);
    Some(Reply {
        bytes: ReplyBytes::Extended(reply),
        handle,
    })
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

/// Verify the client's authenticator over the packet preceding it.
fn verify_client_authenticator(request: &[u8], auth: ef::Field<'_>, c2s: &[u8; 32]) -> Option<()> {
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
