//! The control-plane *client* transport.
//!
//! Lives here rather than in the daemon so there is exactly one implementation
//! of "send an op, read the answer", and so a test or an agent can drive a
//! running daemon by linking this crate instead of shelling out to the CLI.
//!
//! The op types are in `rusty_time-api`, shared with wasm and the future mesh
//! transport; this transport is native-only. Which transport a `--control`
//! argument resolves to is decided by `rusty_time_api::control_endpoint`, so
//! the daemon and this client always agree.

use rusty_time_api::{ControlEndpoint, ControlRequest, ControlResponse};
use std::io::{BufRead, BufReader, Read, Write};
use std::time::Duration;

/// Bound on one response line — a daemon has no business sending more, and an
/// unbounded read would be a memory lever even from a local peer.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Where the control plane lives by default. Defined in `rusty_time-api` so
/// the daemon and this client cannot disagree.
pub fn default_path() -> String {
    rusty_time_api::default_control_spec()
}

/// Send one op to a running daemon and read its answer.
pub fn request(spec: &str, req: &ControlRequest) -> Result<ControlResponse, String> {
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');

    match rusty_time_api::control_endpoint(spec) {
        #[cfg(unix)]
        ControlEndpoint::UnixPath(path) => {
            use std::os::unix::net::UnixStream;
            let stream =
                UnixStream::connect(&path).map_err(|e| format!("connecting {path}: {e}"))?;
            stream
                .set_read_timeout(Some(TIMEOUT))
                .map_err(|e| e.to_string())?;
            stream
                .set_write_timeout(Some(TIMEOUT))
                .map_err(|e| e.to_string())?;
            exchange(stream, &line)
        }
        #[cfg(not(unix))]
        ControlEndpoint::UnixPath(path) => Err(format!(
            "unix domain sockets are unavailable on this platform (asked for {path})"
        )),
        ControlEndpoint::Loopback(port) => {
            let addr = format!("127.0.0.1:{port}");
            let stream = std::net::TcpStream::connect(&addr)
                .map_err(|e| format!("connecting {addr}: {e}"))?;
            stream
                .set_read_timeout(Some(TIMEOUT))
                .map_err(|e| e.to_string())?;
            stream
                .set_write_timeout(Some(TIMEOUT))
                .map_err(|e| e.to_string())?;
            exchange(stream, &line)
        }
    }
}

/// One request, one response, on whichever stream the transport produced.
fn exchange<S: Read + Write>(mut stream: S, line: &str) -> Result<ControlResponse, String> {
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;
    stream.flush().map_err(|e| e.to_string())?;

    // Disambiguate: both Read and Write offer `by_ref`.
    let mut reader = BufReader::new(Read::by_ref(&mut stream).take(MAX_RESPONSE_BYTES));
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .map_err(|e| format!("reading response: {e}"))?;
    if response.trim().is_empty() {
        return Err("daemon closed the connection without answering".into());
    }
    serde_json::from_str(response.trim()).map_err(|e| format!("parsing response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_daemon_is_a_clear_error_not_a_hang() {
        // A control name nothing is serving must fail promptly and say why.
        let err = request(
            "rusty_time_definitely_not_running_xyz",
            &ControlRequest::Ping,
        )
        .expect_err("should fail");
        assert!(
            err.contains("connecting"),
            "error should name the connect step, got: {err}"
        );
    }

    #[test]
    fn the_default_control_name_resolves() {
        // Whatever the platform, the default must map to something usable
        // rather than panicking or producing an empty spec.
        let spec = default_path();
        assert!(!spec.is_empty());
        let _ = rusty_time_api::control_endpoint(&spec);
    }
}
