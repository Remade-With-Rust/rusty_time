//! The browser gateway: NTP over HTTP.
//!
//! A page has no UDP socket, so it cannot speak NTP the ordinary way. The
//! gateway is the door: it accepts a **real NTPv4 packet** in an HTTP body and
//! answers with one, using the same `build_reply` that serves UDP clients.
//!
//! That choice is the point. Inventing a JSON time API for browsers would mean
//! a second protocol, a second parser and a second set of tests, and the
//! browser path would drift from the one everything else uses. Instead the
//! wasm client runs the same codec and the same filter as the native client,
//! and the gateway is an NTP server that happens to have an HTTP door.
//!
//! The HTTP handling here is deliberately small and strictly bounded rather
//! than a web framework: one endpoint, one method, a fixed-size body. Every
//! limit below exists because this listener faces a browser, and browsers are
//! reachable by anyone.

use crate::server::{ServerState, build_reply};
use rusty_time_clock::{ClockRead, SystemClock};
use rusty_time_core::ntp::NtpTimestamp;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Ceiling on the request line plus headers. A browser sends a few hundred
/// bytes; anything approaching this is not a browser.
const MAX_HEADER_BYTES: u64 = 8 * 1024;
/// Ceiling on the body. An NTP packet with extension fields is well under this.
const MAX_BODY_BYTES: usize = 4 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Serve the gateway until the process ends.
pub fn serve(
    bind: &str,
    state: Arc<Mutex<ServerState>>,
    asset_dir: Option<String>,
) -> Result<(), String> {
    let listener = TcpListener::bind(bind).map_err(|e| format!("binding gateway {bind}: {e}"))?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = Arc::clone(&state);
        let assets = asset_dir.clone();
        std::thread::spawn(move || {
            let _ = handle(stream, &state, assets.as_deref());
        });
    }
    Ok(())
}

fn handle(
    stream: TcpStream,
    state: &Arc<Mutex<ServerState>>,
    asset_dir: Option<&str>,
) -> Result<(), String> {
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    let peer = stream.peer_addr().map_err(|e| e.to_string())?;

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.take(MAX_HEADER_BYTES));

    let mut request_line = String::new();
    if reader
        .read_line(&mut request_line)
        .map_err(|e| e.to_string())?
        == 0
    {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    // Headers: we need Content-Length and nothing else.
    //
    // One buffer, cleared and refilled, rather than a fresh `String` per
    // header line. A browser sends a dozen or more headers per request, so the
    // old form allocated and freed a dozen strings to read a single number.
    let mut content_length = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line
            .split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim())
        {
            content_length = value.parse().unwrap_or(0);
        }
    }

    match (method.as_str(), path.as_str()) {
        // A browser preflights a cross-origin POST before sending it.
        ("OPTIONS", _) => respond(&mut writer, 204, "text/plain", &[]),
        ("POST", p) if p.starts_with("/time") => {
            time_exchange(&mut reader, &mut writer, content_length, peer, state)
        }
        ("GET", "/") | ("GET", "/index.html") => respond(
            &mut writer,
            200,
            "text/html; charset=utf-8",
            STATUS_PAGE.as_bytes(),
        ),
        ("GET", p) => serve_asset(&mut writer, p, asset_dir),
        _ => respond(&mut writer, 405, "text/plain", b"method not allowed\n"),
    }
}

/// The op itself: an NTP packet in, an NTP packet out.
fn time_exchange<R: BufRead>(
    reader: &mut R,
    writer: &mut TcpStream,
    content_length: usize,
    peer: SocketAddr,
    state: &Arc<Mutex<ServerState>>,
) -> Result<(), String> {
    if content_length == 0 || content_length > MAX_BODY_BYTES {
        return respond(writer, 400, "text/plain", b"bad request body length\n");
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_err() {
        return respond(writer, 400, "text/plain", b"truncated body\n");
    }

    // Timestamp as early as possible: everything after this is processing
    // delay the client should not be charged for.
    let clock = SystemClock;
    let Ok(ns) = clock.wall_ns() else {
        return respond(writer, 500, "text/plain", b"clock unavailable\n");
    };
    let recv_ts = NtpTimestamp::from_unix((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32);

    // The same reply builder that answers UDP: rate limiting, interleaved
    // mode, NTS and all. A gateway client is a client.
    match build_reply(&body, peer, recv_ts, state, &clock) {
        Some(reply) => respond(
            writer,
            200,
            "application/octet-stream",
            reply.bytes.as_slice(),
        ),
        // Rate-limited or refused: say so in HTTP terms rather than hanging.
        None => respond(writer, 429, "text/plain", b"no reply\n"),
    }
}

fn serve_asset(writer: &mut TcpStream, path: &str, asset_dir: Option<&str>) -> Result<(), String> {
    let Some(dir) = asset_dir else {
        return respond(writer, 404, "text/plain", b"not found\n");
    };
    // Only a flat file name from the asset directory: no separators, no
    // parent references. A path-traversal bug here would serve any file the
    // daemon can read to anyone who can reach the port.
    let name = path.trim_start_matches('/');
    if name.is_empty()
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
    {
        return respond(writer, 404, "text/plain", b"not found\n");
    }
    let content_type = if name.ends_with(".wasm") {
        "application/wasm"
    } else if name.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else {
        "application/octet-stream"
    };
    match std::fs::read(std::path::Path::new(dir).join(name)) {
        Ok(bytes) => respond(writer, 200, content_type, &bytes),
        Err(_) => respond(writer, 404, "text/plain", b"not found\n"),
    }
}

fn respond(
    writer: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
         Access-Control-Allow-Headers: content-type\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    writer
        .write_all(head.as_bytes())
        .map_err(|e| e.to_string())?;
    writer.write_all(body).map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// The status page. Self-contained so the gateway can serve it with no build
/// step; it loads the wasm module from the same origin when one is present.
const STATUS_PAGE: &str = include_str!("status_page.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_that_escape_the_directory_are_refused() {
        // The check is on the name, so exercise it directly: a traversal here
        // would serve any readable file to anyone who can reach the port.
        for bad in [
            "../secret",
            "..\\secret",
            "a/b",
            "a\\b",
            "C:windows",
            "",
            "../../etc/passwd",
        ] {
            let name = bad.trim_start_matches('/');
            let refused = name.is_empty()
                || name.contains("..")
                || name.contains('/')
                || name.contains('\\')
                || name.contains(':');
            assert!(refused, "{bad} would have been served");
        }
        // Ordinary asset names still pass.
        for good in ["rusty_time_wasm.js", "rusty_time_wasm_bg.wasm", "demo.html"] {
            let refused = good.is_empty()
                || good.contains("..")
                || good.contains('/')
                || good.contains('\\')
                || good.contains(':');
            assert!(!refused, "{good} should be servable");
        }
    }

    #[test]
    fn the_status_page_is_present_and_self_contained() {
        assert!(STATUS_PAGE.len() > 500, "status page looks empty");
        assert!(STATUS_PAGE.contains("<!doctype html") || STATUS_PAGE.contains("<!DOCTYPE html"));
        // A page that reaches out to a CDN would not work on a private mesh,
        // which is exactly where this is meant to run.
        assert!(
            !STATUS_PAGE.contains("http://") && !STATUS_PAGE.contains("https://"),
            "status page must not reference external origins"
        );
    }
}
