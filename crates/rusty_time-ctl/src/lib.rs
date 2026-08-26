//! The control-plane *client* transport.
//!
//! Lives here rather than in the daemon so there is exactly one implementation
//! of "send an op, read the answer", and so a test or an agent can drive a
//! running daemon by linking this crate instead of shelling out to the CLI.
//!
//! The op types themselves are in `rusty_time-api`: types are shared with
//! wasm and the future mesh transport, this transport is native-only.

use rusty_time_api::{ControlRequest, ControlResponse};
use std::io::{BufRead, BufReader, Read, Write};
use std::time::Duration;

/// Bound on one response line — a daemon has no business sending more, and an
/// unbounded read would be a memory lever even from a local peer.
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);

/// Where the control socket lives by default.
pub fn default_path() -> String {
    #[cfg(windows)]
    {
        r"\\.\pipe\rusty_time".to_string()
    }
    #[cfg(unix)]
    {
        match std::env::var("XDG_RUNTIME_DIR") {
            Ok(dir) if !dir.is_empty() => format!("{dir}/rusty_time.sock"),
            _ => "/tmp/rusty_time.sock".to_string(),
        }
    }
}

/// Send one op to a running daemon and read its answer.
pub fn request(path: &str, req: &ControlRequest) -> Result<ControlResponse, String> {
    let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    line.push('\n');

    #[cfg(unix)]
    let stream = {
        use std::os::unix::net::UnixStream;
        UnixStream::connect(path).map_err(|e| format!("connecting {path}: {e}"))?
    };
    #[cfg(windows)]
    let stream = {
        // Named pipes need the Win32 API; until that lands the daemon listens
        // on loopback TCP. Same locality, weaker authorization — stated, not
        // pretended equivalent.
        let target = if path.starts_with(r"\\.\pipe\") {
            "127.0.0.1:11323".to_string()
        } else {
            path.to_string()
        };
        std::net::TcpStream::connect(&target).map_err(|e| format!("connecting {target}: {e}"))?
    };

    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(|e| e.to_string())?;

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writer
        .write_all(line.as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;
    writer.flush().map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream.take(MAX_RESPONSE_BYTES));
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
        let path = if cfg!(windows) {
            "127.0.0.1:1".to_string()
        } else {
            "/tmp/rusty_time_definitely_not_running.sock".to_string()
        };
        let err = request(&path, &ControlRequest::Ping).expect_err("should fail");
        assert!(
            err.contains("connecting"),
            "error should name the connect step, got: {err}"
        );
    }
}
