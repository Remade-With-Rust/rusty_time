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

use rusty_time_clock::{ClockRead, SystemClock};
use rusty_time_core::ntp::{self, HEADER_LEN, LeapIndicator, Mode, NtpPacket, NtpTimestamp};
use rusty_time_nts::aead::NtsKeys;
use rusty_time_nts::cookie::{COOKIE_NONCE_LEN, KeyRing, MasterKey};
use rusty_time_nts::ef::{self, NONCE_LEN, UNIQUE_ID_LEN};
use rusty_time_nts::records::{self, record_type};
use rusty_time_nts::tls::rustls;
use rusty_time_nts::{AEAD_AES_SIV_CMAC_256, ALPN, NEXT_PROTO_NTPV4};

use crate::store::{MASTER_KEY_SLOTS, Store, StoredMasterKey};
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Cookies handed out per NTS-KE exchange (RFC 8915 §4.1.6 recommends 8).
const KE_COOKIE_COUNT: usize = 8;
/// Cap on how many cookies one NTP reply may carry, so a request stuffed with
/// placeholders cannot be used as an amplification lever.
const MAX_REPLY_COOKIES: usize = 8;
/// Largest NTP datagram we will consider.
const MAX_DATAGRAM: usize = 4096;

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
    let ring = Arc::new(Mutex::new(ring));
    // Keep the store alive for the process's lifetime; dropping it would be
    // harmless but the handle documents that state is owned here.
    let _store = store;

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
        let ke_ring = Arc::clone(&ring);
        // Tell clients where NTP actually is. RFC 8915 §4.1.8 defaults to 123,
        // so a non-standard port must be advertised or clients query nothing.
        let ntp_port = opts
            .ntp_bind
            .rsplit(':')
            .next()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(123);
        println!("rtimed: NTS-KE listening on {}", opts.ke_bind);
        std::thread::spawn(move || ke_accept_loop(listener, tls_config, ke_ring, ntp_port));
    }

    let socket = match UdpSocket::bind(&opts.ntp_bind) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rtimed serve: binding NTP {}: {e}", opts.ntp_bind);
            return 1;
        }
    };
    println!(
        "rtimed: NTP listening on {} (stratum {})",
        opts.ntp_bind, opts.stratum
    );
    ntp_serve_loop(&socket, opts.stratum, &ring);
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
    ring: Arc<Mutex<KeyRing>>,
    ntp_port: u16,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let config = Arc::clone(&config);
        let ring = Arc::clone(&ring);
        std::thread::spawn(move || {
            if let Err(e) = handle_ke(stream, config, ring, ntp_port) {
                eprintln!("rtimed serve: NTS-KE session: {e}");
            }
        });
    }
}

fn handle_ke(
    stream: std::net::TcpStream,
    config: Arc<rustls::ServerConfig>,
    ring: Arc<Mutex<KeyRing>>,
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
        let ring = ring.lock().map_err(|_| "key ring poisoned")?;
        for _ in 0..KE_COOKIE_COUNT {
            let mut nonce = [0u8; COOKIE_NONCE_LEN];
            rusty_time_nts::ke::fill_random(&mut nonce).map_err(|e| e.to_string())?;
            let cookie =
                rusty_time_nts::cookie::mint(&ring, &keys, &nonce).map_err(|e| e.to_string())?;
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

fn ntp_serve_loop(socket: &UdpSocket, stratum: u8, ring: &Arc<Mutex<KeyRing>>) {
    let clock = SystemClock;
    let mut buf = [0u8; MAX_DATAGRAM];
    loop {
        let (len, peer) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Receive timestamp as early as possible: everything after this is
        // processing delay the client should not be charged for.
        let recv_ts = match clock.wall_ns() {
            Ok(ns) => unix_ns_to_ntp(ns),
            Err(_) => continue,
        };

        let Some(reply) = build_reply(&buf[..len], recv_ts, stratum, ring, &clock) else {
            continue;
        };
        let _ = socket.send_to(&reply, peer);
    }
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
    recv_ts: NtpTimestamp,
    stratum: u8,
    ring: &Arc<Mutex<KeyRing>>,
    clock: &SystemClock,
) -> Option<Vec<u8>> {
    if request.len() < HEADER_LEN {
        return None;
    }
    let parsed = NtpPacket::parse(request).ok()?;
    // Only client-mode requests are answered: never reflect a server-mode
    // packet, which is how NTP reflection amplification starts.
    if parsed.mode != Mode::Client {
        return None;
    }

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
        origin_ts: parsed.transmit_ts,
        receive_ts: recv_ts,
        transmit_ts: recv_ts,
    };

    if !has_auth {
        // Plain NTP: stamp transmit as late as possible and answer.
        header.transmit_ts = clock.wall_ns().ok().map(unix_ns_to_ntp)?;
        return Some(header.to_bytes().to_vec());
    }

    // NTS path: the cookie must redeem and the authenticator must verify
    // before we say anything at all about the time.
    let (cookie, unique_id) = (cookie?, unique_id?);
    let keys = {
        let ring = ring.lock().ok()?;
        rusty_time_nts::cookie::redeem(&ring, cookie).ok()?
    };
    verify_client_authenticator(request, &keys.c2s)?;

    // Fresh cookies: one to replace the cookie just spent, plus the
    // placeholders asked for, capped.
    let want = (1 + placeholders).min(MAX_REPLY_COOKIES);
    let mut plaintext = Vec::new();
    {
        let ring = ring.lock().ok()?;
        for _ in 0..want {
            let mut nonce = [0u8; COOKIE_NONCE_LEN];
            rusty_time_nts::ke::fill_random(&mut nonce).ok()?;
            let fresh = rusty_time_nts::cookie::mint(&ring, &keys, &nonce).ok()?;
            ef::write_field(&mut plaintext, ef::field_type::NTS_COOKIE, &fresh);
        }
    }

    header.transmit_ts = clock.wall_ns().ok().map(unix_ns_to_ntp)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nts_session::NtsSession;

    fn test_ring() -> Arc<Mutex<KeyRing>> {
        let mut ring = KeyRing::new(3);
        ring.rotate_in(MasterKey {
            id: 7,
            key: [0x5A; 32],
        });
        Arc::new(Mutex::new(ring))
    }

    #[test]
    fn answers_plain_client_requests() {
        let ring = test_ring();
        let req = NtpPacket::client_request(4, NtpTimestamp(0x1234_5678_9ABC_DEF0)).to_bytes();
        let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
        let reply = build_reply(&req, recv, 2, &ring, &SystemClock).expect("reply");
        let p = NtpPacket::parse(&reply).expect("parse");
        assert_eq!(p.mode, Mode::Server);
        assert_eq!(p.stratum, 2);
        // The origin timestamp must echo the client's transmit: that is the
        // client's only anti-spoofing binding.
        assert_eq!(p.origin_ts, NtpTimestamp(0x1234_5678_9ABC_DEF0));
    }

    #[test]
    fn refuses_to_reflect_server_mode_packets() {
        // A server-mode datagram must never be answered, or the server becomes
        // a reflection amplifier between two victims.
        let ring = test_ring();
        let mut p = NtpPacket::client_request(4, NtpTimestamp(1));
        p.mode = Mode::Server;
        let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
        assert!(build_reply(&p.to_bytes(), recv, 1, &ring, &SystemClock).is_none());
    }

    #[test]
    fn nts_round_trip_client_to_server() {
        // Mint a cookie the way NTS-KE would, hand it to a client session, and
        // run a full protected exchange through the real server path.
        let ring = test_ring();
        let keys = NtsKeys {
            c2s: [0x11; 32],
            s2c: [0x22; 32],
        };
        let cookie = {
            let r = ring.lock().expect("lock");
            rusty_time_nts::cookie::mint(&r, &keys, &[3; COOKIE_NONCE_LEN]).expect("mint")
        };

        let mut session = NtsSession::for_test(keys.clone(), vec![cookie]);
        let header = NtpPacket::client_request(4, NtpTimestamp(0xAABB_CCDD_1122_3344)).to_bytes();
        let request = session.protect(&header).expect("protect");

        let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
        let reply = build_reply(&request, recv, 1, &ring, &SystemClock).expect("server reply");

        // The client must accept it, and must get its cookie replaced.
        assert_eq!(session.cookies_held(), 0, "cookie spent");
        session
            .verify(&reply)
            .expect("client verifies server reply");
        assert!(
            session.cookies_held() >= 1,
            "server must replenish the spent cookie"
        );
    }

    #[test]
    fn forged_authenticator_gets_no_answer() {
        let ring = test_ring();
        let keys = NtsKeys {
            c2s: [0x11; 32],
            s2c: [0x22; 32],
        };
        let cookie = {
            let r = ring.lock().expect("lock");
            rusty_time_nts::cookie::mint(&r, &keys, &[3; COOKIE_NONCE_LEN]).expect("mint")
        };
        let mut session = NtsSession::for_test(keys, vec![cookie]);
        let header = NtpPacket::client_request(4, NtpTimestamp(1)).to_bytes();
        let mut request = session.protect(&header).expect("protect");

        // Corrupt the last byte of the authenticator's ciphertext.
        let last = request.len() - 1;
        request[last] ^= 0xFF;
        let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
        assert!(
            build_reply(&request, recv, 1, &ring, &SystemClock).is_none(),
            "server answered a forged NTS request"
        );
    }

    #[test]
    fn unknown_cookie_gets_no_answer() {
        let ring = test_ring();
        let keys = NtsKeys {
            c2s: [0x11; 32],
            s2c: [0x22; 32],
        };
        // A cookie minted under a different master key: not ours.
        let mut foreign = KeyRing::new(3);
        foreign.rotate_in(MasterKey {
            id: 7,
            key: [0xEE; 32],
        });
        let cookie =
            rusty_time_nts::cookie::mint(&foreign, &keys, &[3; COOKIE_NONCE_LEN]).expect("mint");
        let mut session = NtsSession::for_test(keys, vec![cookie]);
        let header = NtpPacket::client_request(4, NtpTimestamp(1)).to_bytes();
        let request = session.protect(&header).expect("protect");
        let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
        assert!(build_reply(&request, recv, 1, &ring, &SystemClock).is_none());
    }

    #[test]
    fn placeholder_count_cannot_amplify_without_bound() {
        let ring = test_ring();
        let keys = NtsKeys {
            c2s: [0x11; 32],
            s2c: [0x22; 32],
        };
        let cookie = {
            let r = ring.lock().expect("lock");
            rusty_time_nts::cookie::mint(&r, &keys, &[3; COOKIE_NONCE_LEN]).expect("mint")
        };
        // Hand-build a request stuffed with placeholders.
        let header = NtpPacket::client_request(4, NtpTimestamp(1)).to_bytes();
        let uid = [9u8; UNIQUE_ID_LEN];
        let nonce = [4u8; NONCE_LEN];
        let request =
            ef::protect_request(&header, &uid, &cookie, 60, &keys.c2s, &nonce).expect("protect");

        let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
        let reply = build_reply(&request, recv, 1, &ring, &SystemClock).expect("reply");
        // The reply must not grow without bound relative to the request.
        assert!(
            reply.len() <= request.len(),
            "reply ({}) exceeded request ({}): amplification",
            reply.len(),
            request.len()
        );
    }

    #[test]
    fn malformed_datagrams_never_panic() {
        let ring = test_ring();
        let recv = NtpTimestamp::from_unix(1_756_224_000, 0);
        let mut rng = 0xACE1_2345_6789_BEEFu64;
        for _ in 0..20_000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let len = (rng % 200) as usize;
            let mut p = vec![0u8; len];
            for b in p.iter_mut() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                *b = rng as u8;
            }
            if len > 0 {
                p[0] = (4 << 3) | 3; // bias toward "looks like a client request"
            }
            let _ = build_reply(&p, recv, 1, &ring, &SystemClock);
        }
    }
}
