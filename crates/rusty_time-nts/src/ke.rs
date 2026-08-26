//! NTS Key Establishment client (RFC 8915 §4): a TLS 1.3 exchange on port 4460
//! that yields the C2S/S2C AEAD keys and an initial batch of cookies.
//!
//! The TLS stack is rustls with an explicitly supplied **pure-Rust** provider
//! (`oxitls-rustcrypto-provider`); rustls's default provider compiles C, which
//! the house rule forbids (mission plan §2). This module is behind the `ke`
//! feature so wasm and codec-only consumers never pull a TLS stack.

use crate::aead::NtsKeys;
use crate::records::{self, record_type};
use crate::{AEAD_AES_SIV_CMAC_256, ALPN, KE_PORT, NEXT_PROTO_NTPV4};
use core::fmt;
use rustls::pki_types::ServerName;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// RFC 8915 §4.3: the RFC 5705 exporter label for NTS.
const EXPORTER_LABEL: &[u8] = b"EXPORTER-network-time-security";
/// Cap on the NTS-KE response we will buffer — a server that never sends
/// end-of-message must not grow our memory without bound.
const MAX_KE_RESPONSE: usize = 64 * 1024;

#[derive(Debug)]
pub enum KeError {
    Io(std::io::Error),
    Tls(String),
    /// The server name could not be parsed as a DNS name.
    BadServerName(String),
    /// Server declined: an NTS-KE Error record, with its code.
    Server {
        code: u16,
    },
    /// The server did not agree to what we require.
    Negotiation(&'static str),
    /// Response records were malformed or truncated.
    Malformed,
    /// TLS gave us no exported keying material.
    Export(String),
}

impl fmt::Display for KeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeError::Io(e) => write!(f, "NTS-KE I/O: {e}"),
            KeError::Tls(e) => write!(f, "NTS-KE TLS: {e}"),
            KeError::BadServerName(n) => write!(f, "NTS-KE: '{n}' is not a valid DNS name"),
            KeError::Server { code } => {
                let meaning = match code {
                    0 => " (unrecognized critical record)",
                    1 => " (bad request)",
                    2 => " (internal server error)",
                    _ => "",
                };
                write!(f, "NTS-KE server returned error {code}{meaning}")
            }
            KeError::Negotiation(what) => write!(f, "NTS-KE negotiation failed: {what}"),
            KeError::Malformed => write!(f, "NTS-KE response was malformed"),
            KeError::Export(e) => write!(f, "NTS-KE key export failed: {e}"),
        }
    }
}

impl std::error::Error for KeError {}

impl From<std::io::Error> for KeError {
    fn from(e: std::io::Error) -> Self {
        KeError::Io(e)
    }
}

/// Everything one NTS-KE exchange establishes.
pub struct KeResult {
    pub keys: NtsKeys,
    /// Initial cookie batch. Each is spent once, never reused.
    pub cookies: Vec<Vec<u8>>,
    /// The NTP server to query — the KE server may delegate elsewhere
    /// (RFC 8915 §4.1.7), so this is not necessarily the KE host.
    pub ntp_server: String,
    pub ntp_port: u16,
}

/// Run NTS-KE against `host`, returning keys and cookies.
///
/// `timeout` bounds the TCP connect and each read.
pub fn establish(host: &str, port: u16, timeout: Duration) -> Result<KeResult, KeError> {
    establish_with_roots(host, port, timeout, &[])
}

/// As [`establish`], additionally trusting `extra_roots` — a private CA or a
/// pinned self-signed KE server. Verification still happens; these only widen
/// the anchor set.
pub fn establish_with_roots(
    host: &str,
    port: u16,
    timeout: Duration,
    extra_roots: &[rustls::pki_types::CertificateDer<'static>],
) -> Result<KeResult, KeError> {
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| KeError::BadServerName(host.to_string()))?;

    // The provider choice lives in `tls`, not here (see that module).
    let config = crate::tls::client_config_with_roots(&[ALPN], extra_roots)
        .map_err(|e| KeError::Tls(e.to_string()))?;

    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| KeError::Tls(e.to_string()))?;

    // Connect with a timeout: a black-holed KE host must not hang the daemon.
    // Try every resolved address, not just the first — a host with both A and
    // AAAA records where one family is unreachable is common (and is exactly
    // what a dual-stack `localhost` looks like), and giving up on the first
    // refusal would strand a client that has a working path.
    let addrs: Vec<_> = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))?.collect();
    if addrs.is_empty() {
        return Err(KeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no addresses for KE host",
        )));
    }
    let mut last_err = None;
    let mut connected = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, timeout) {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let tcp = connected.ok_or_else(|| {
        KeError::Io(last_err.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no reachable KE address")
        }))
    })?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;

    let mut tls = rustls::StreamOwned::new(conn, tcp);
    tls.write_all(&client_request())?;
    tls.flush()?;

    // Read until the server closes or we have the whole record stream.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_KE_RESPONSE {
                    return Err(KeError::Malformed);
                }
                if has_end_of_message(&buf) {
                    break;
                }
            }
            // A server that closes without close_notify still gave us bytes.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(KeError::Io(e)),
        }
    }

    // ALPN is not optional: without it we cannot know the peer agreed to
    // speak NTS-KE rather than some other protocol on 4460.
    match tls.conn.alpn_protocol() {
        Some(p) if p == ALPN => {}
        _ => return Err(KeError::Negotiation("peer did not select ALPN ntske/1")),
    }

    let parsed = parse_response(&buf, host, port)?;

    // RFC 8915 §4.3: export 32 octets per direction, keyed by the negotiated
    // protocol and AEAD, with the direction in the final context octet.
    let mut keys = NtsKeys {
        c2s: [0u8; 32],
        s2c: [0u8; 32],
    };
    keys.c2s = export_key(&tls.conn, 0x00)?;
    keys.s2c = export_key(&tls.conn, 0x01)?;

    Ok(KeResult {
        keys,
        cookies: parsed.cookies,
        ntp_server: parsed.ntp_server,
        ntp_port: parsed.ntp_port,
    })
}

fn export_key(conn: &rustls::ClientConnection, direction: u8) -> Result<[u8; 32], KeError> {
    let context = [
        (NEXT_PROTO_NTPV4 >> 8) as u8,
        NEXT_PROTO_NTPV4 as u8,
        (AEAD_AES_SIV_CMAC_256 >> 8) as u8,
        AEAD_AES_SIV_CMAC_256 as u8,
        direction,
    ];
    conn.export_keying_material([0u8; 32], EXPORTER_LABEL, Some(&context))
        .map_err(|e| KeError::Export(e.to_string()))
}

/// The canonical client request: NTPv4 next-protocol, AES-SIV-CMAC-256, EOM.
fn client_request() -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
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
    records::write_record(&mut out, true, record_type::END_OF_MESSAGE, &[]);
    out
}

fn has_end_of_message(buf: &[u8]) -> bool {
    records::records(buf)
        .any(|r| matches!(r, Ok(rec) if rec.record_type == record_type::END_OF_MESSAGE))
}

#[derive(Debug)]
struct Parsed {
    cookies: Vec<Vec<u8>>,
    ntp_server: String,
    ntp_port: u16,
}

fn parse_response(buf: &[u8], host: &str, ke_port: u16) -> Result<Parsed, KeError> {
    let mut cookies = Vec::new();
    let mut ntp_server = host.to_string();
    // Default per RFC 8915 §4.1.8: NTP's own port, not the KE port.
    let mut ntp_port = 123u16;
    let mut proto_ok = false;
    let mut aead_ok = false;
    let _ = ke_port;

    for record in records::records(buf) {
        let record = record.map_err(|_| KeError::Malformed)?;
        match record.record_type {
            record_type::ERROR => {
                let code = if record.body.len() >= 2 {
                    u16::from_be_bytes([record.body[0], record.body[1]])
                } else {
                    u16::MAX
                };
                return Err(KeError::Server { code });
            }
            record_type::WARNING => { /* advisory only; RFC 8915 §4.1.3 */ }
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
            record_type::NEW_COOKIE => cookies.push(record.body.to_vec()),
            record_type::SERVER_NEGOTIATION => {
                ntp_server = core::str::from_utf8(record.body)
                    .map_err(|_| KeError::Malformed)?
                    .to_string();
            }
            record_type::PORT_NEGOTIATION => {
                if record.body.len() >= 2 {
                    ntp_port = u16::from_be_bytes([record.body[0], record.body[1]]);
                }
            }
            record_type::END_OF_MESSAGE => break,
            other => {
                // RFC 8915 §4: an unrecognized *critical* record is fatal.
                if record.critical {
                    return Err(KeError::Negotiation("unrecognized critical record"));
                }
                let _ = other;
            }
        }
    }

    if !proto_ok {
        return Err(KeError::Negotiation("server did not accept NTPv4"));
    }
    if !aead_ok {
        return Err(KeError::Negotiation(
            "server did not accept AES-SIV-CMAC-256",
        ));
    }
    if cookies.is_empty() {
        return Err(KeError::Negotiation("server sent no cookies"));
    }

    Ok(Parsed {
        cookies,
        ntp_server,
        ntp_port,
    })
}

/// Convenience: NTS-KE on the standard port.
pub fn establish_default(host: &str, timeout: Duration) -> Result<KeResult, KeError> {
    establish(host, KE_PORT, timeout)
}

/// Fill `out` with cryptographically secure random bytes (nonces, unique ids).
pub fn fill_random(out: &mut [u8]) -> Result<(), KeError> {
    getrandom::fill(out).map_err(|e| KeError::Export(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_request_is_the_canonical_three_records() {
        let req = client_request();
        let recs: Vec<_> = records::records(&req)
            .collect::<Result<Vec<_>, _>>()
            .expect("parse");
        assert_eq!(recs.len(), 3);
        assert!(recs.iter().all(|r| r.critical), "all three are critical");
        assert_eq!(recs[0].body, &NEXT_PROTO_NTPV4.to_be_bytes());
        assert_eq!(recs[1].body, &AEAD_AES_SIV_CMAC_256.to_be_bytes());
        assert_eq!(recs[2].record_type, record_type::END_OF_MESSAGE);
    }

    fn response(records_in: &[(bool, u16, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (crit, ty, body) in records_in {
            records::write_record(&mut out, *crit, *ty, body);
        }
        out
    }

    #[test]
    fn parses_a_well_formed_response() {
        let buf = response(&[
            (true, record_type::NEXT_PROTOCOL, vec![0, 0]),
            (true, record_type::AEAD_ALGORITHM, vec![0, 15]),
            (false, record_type::NEW_COOKIE, vec![1; 100]),
            (false, record_type::NEW_COOKIE, vec![2; 100]),
            (
                false,
                record_type::SERVER_NEGOTIATION,
                b"ntp.example.org".to_vec(),
            ),
            (false, record_type::PORT_NEGOTIATION, vec![0, 123]),
            (true, record_type::END_OF_MESSAGE, vec![]),
        ]);
        let p = parse_response(&buf, "ke.example.org", 4460).expect("parse");
        assert_eq!(p.cookies.len(), 2);
        assert_eq!(p.ntp_server, "ntp.example.org");
        assert_eq!(p.ntp_port, 123);
    }

    #[test]
    fn ntp_port_defaults_to_123_not_the_ke_port() {
        let buf = response(&[
            (true, record_type::NEXT_PROTOCOL, vec![0, 0]),
            (true, record_type::AEAD_ALGORITHM, vec![0, 15]),
            (false, record_type::NEW_COOKIE, vec![1; 100]),
            (true, record_type::END_OF_MESSAGE, vec![]),
        ]);
        let p = parse_response(&buf, "ke.example.org", 4460).expect("parse");
        assert_eq!(p.ntp_port, 123);
        assert_eq!(p.ntp_server, "ke.example.org");
    }

    #[test]
    fn server_error_record_is_surfaced_with_its_code() {
        let buf = response(&[
            (true, record_type::ERROR, vec![0, 1]),
            (true, record_type::END_OF_MESSAGE, vec![]),
        ]);
        match parse_response(&buf, "h", 4460) {
            Err(KeError::Server { code: 1 }) => {}
            other => panic!("expected server error 1, got {other:?}"),
        }
    }

    #[test]
    fn refuses_an_aead_we_cannot_speak() {
        let buf = response(&[
            (true, record_type::NEXT_PROTOCOL, vec![0, 0]),
            (true, record_type::AEAD_ALGORITHM, vec![0, 17]), // not SIV-CMAC-256
            (false, record_type::NEW_COOKIE, vec![1; 100]),
            (true, record_type::END_OF_MESSAGE, vec![]),
        ]);
        assert!(matches!(
            parse_response(&buf, "h", 4460),
            Err(KeError::Negotiation(_))
        ));
    }

    #[test]
    fn refuses_a_response_with_no_cookies() {
        let buf = response(&[
            (true, record_type::NEXT_PROTOCOL, vec![0, 0]),
            (true, record_type::AEAD_ALGORITHM, vec![0, 15]),
            (true, record_type::END_OF_MESSAGE, vec![]),
        ]);
        assert!(matches!(
            parse_response(&buf, "h", 4460),
            Err(KeError::Negotiation(_))
        ));
    }

    #[test]
    fn unrecognized_critical_record_is_fatal() {
        let buf = response(&[
            (true, record_type::NEXT_PROTOCOL, vec![0, 0]),
            (true, record_type::AEAD_ALGORITHM, vec![0, 15]),
            (true, 0x4242, vec![0; 12]),
            (false, record_type::NEW_COOKIE, vec![1; 100]),
            (true, record_type::END_OF_MESSAGE, vec![]),
        ]);
        assert!(matches!(
            parse_response(&buf, "h", 4460),
            Err(KeError::Negotiation(_))
        ));
    }

    #[test]
    fn random_fill_produces_distinct_values() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        fill_random(&mut a).expect("random");
        fill_random(&mut b).expect("random");
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
    }
}
