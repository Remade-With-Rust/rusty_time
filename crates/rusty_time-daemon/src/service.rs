//! Service-manager integration: systemd on Linux, launchd on macOS, the SCM on
//! Windows.
//!
//! Each platform's supervisor wants to know two things — *is it up yet* and
//! *should it stop now* — and each asks differently. The daemon should not
//! care which, so this module answers both questions behind one shape.
//!
//! systemd's side is implemented directly rather than through `libsystemd`:
//! the readiness protocol is a datagram to the socket named in `NOTIFY_SOCKET`,
//! and socket activation is a file descriptor at a known number. Both are a few
//! lines, and neither is worth a C dependency (mission plan §2).

use std::net::UdpSocket;

/// Tell the supervisor this process is up and serving.
///
/// A no-op where the supervisor does not ask. Failure is never fatal: a daemon
/// that refuses to run because it could not *announce* that it is running has
/// its priorities backwards.
pub fn notify_ready(status: &str) {
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = systemd_notify(&format!("READY=1\nSTATUS={status}\n")) {
            // Only interesting when systemd asked to be told.
            if std::env::var_os("NOTIFY_SOCKET").is_some() {
                eprintln!("rtimed: could not notify systemd: {e}");
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = status;
    }
}

/// Update the one-line status the supervisor shows (`systemctl status`).
pub fn notify_status(status: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = systemd_notify(&format!("STATUS={status}\n"));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = status;
    }
}

/// Send one datagram to systemd's notify socket.
///
/// The socket may be a plain path or an abstract-namespace name introduced by
/// `@`, which Linux encodes as a leading NUL byte — a detail that has to be
/// handled explicitly because Rust's `UnixDatagram` takes a path, not a raw
/// abstract name.
#[cfg(target_os = "linux")]
fn systemd_notify(message: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(()); // not running under systemd
    };
    let path = path.to_string_lossy().into_owned();
    let socket = UnixDatagram::unbound()?;

    if let Some(name) = path.strip_prefix('@') {
        let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
        socket.send_to_addr(message.as_bytes(), &addr)?;
    } else {
        socket.send_to(message.as_bytes(), &path)?;
    }
    Ok(())
}

/// A socket handed to us by the supervisor, if there is one.
///
/// Delegates to the clock crate, which owns the platform seam and is the one
/// crate allowed to adopt a raw descriptor (`unsafe_code` is denied
/// workspace-wide — the 8-target matrix caught this, because the offending
/// block only compiles on Linux).
pub fn activated_udp_socket() -> Option<UdpSocket> {
    rusty_time_clock::net::activated_udp_socket()
}

/// The unit / plist / service definition for this platform, ready to install.
pub fn service_definition(exec_path: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        format!(
            "[Unit]\n\
             Description=rusty_time NTP/NTS daemon\n\
             Documentation=https://github.com/remade-with-rust/rusty_time\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=notify\n\
             ExecStart={exec_path} serve --nts\n\
             Restart=on-failure\n\
             RestartSec=2\n\
             # The clock is the only privilege needed; drop everything else.\n\
             AmbientCapabilities=CAP_SYS_TIME\n\
             CapabilityBoundingSet=CAP_SYS_TIME\n\
             NoNewPrivileges=yes\n\
             ProtectSystem=strict\n\
             ProtectHome=yes\n\
             PrivateTmp=yes\n\
             PrivateDevices=yes\n\
             ProtectKernelTunables=yes\n\
             ProtectControlGroups=yes\n\
             RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX\n\
             RestrictNamespaces=yes\n\
             LockPersonality=yes\n\
             MemoryDenyWriteExecute=yes\n\
             SystemCallArchitectures=native\n\
             StateDirectory=rusty_time\n\
             RuntimeDirectory=rusty_time\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n"
        )
    }
    #[cfg(target_os = "macos")]
    {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \x20 <key>Label</key>\n\
             \x20 <string>network.mata.rusty_time</string>\n\
             \x20 <key>ProgramArguments</key>\n\
             \x20 <array>\n\
             \x20   <string>{exec_path}</string>\n\
             \x20   <string>serve</string>\n\
             \x20   <string>--nts</string>\n\
             \x20 </array>\n\
             \x20 <key>RunAtLoad</key>\n\
             \x20 <true/>\n\
             \x20 <key>KeepAlive</key>\n\
             \x20 <true/>\n\
             \x20 <key>StandardErrorPath</key>\n\
             \x20 <string>/var/log/rusty_time.log</string>\n\
             </dict>\n\
             </plist>\n"
        )
    }
    #[cfg(windows)]
    {
        format!(
            "REM Install rusty_time as a Windows service (run elevated).\r\n\
             REM The service account needs SeSystemtimePrivilege, which\r\n\
             REM LocalSystem holds by default.\r\n\
             sc.exe create rusty_time binPath= \"{exec_path} serve --nts\" \
             start= auto DisplayName= \"rusty_time NTP/NTS\"\r\n\
             sc.exe description rusty_time \"Pure-Rust NTP/NTS time service\"\r\n\
             sc.exe start rusty_time\r\n"
        )
    }
}

/// Where the definition conventionally goes.
pub fn service_definition_path() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "/etc/systemd/system/rusty_time.service"
    }
    #[cfg(target_os = "macos")]
    {
        "/Library/LaunchDaemons/network.mata.rusty_time.plist"
    }
    #[cfg(windows)]
    {
        "install-service.cmd"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        "rusty_time.service"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_definition_names_the_binary_and_is_not_empty() {
        let text = service_definition("/usr/sbin/rtimed");
        assert!(
            text.contains("/usr/sbin/rtimed"),
            "definition must reference the executable"
        );
        assert!(text.len() > 100, "definition looks truncated");
        assert!(!service_definition_path().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_unit_is_hardened_and_uses_readiness() {
        let unit = service_definition("/usr/sbin/rtimed");
        // Type=notify is what makes `systemctl start` wait for us to actually
        // be serving rather than merely forked.
        assert!(unit.contains("Type=notify"));
        // The clock is the only privilege we need; everything else must be off.
        assert!(unit.contains("CapabilityBoundingSet=CAP_SYS_TIME"));
        assert!(unit.contains("NoNewPrivileges=yes"));
        assert!(unit.contains("ProtectSystem=strict"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_installer_creates_an_auto_start_service() {
        let cmd = service_definition(r"C:\Program Files\rusty_time\rtimed.exe");
        assert!(cmd.contains("sc.exe create rusty_time"));
        assert!(cmd.contains("start= auto"));
    }

    #[test]
    fn the_packaged_unit_agrees_with_what_we_print() {
        // `rtimed service show` and the unit shipped in the .deb/.rpm are two
        // copies of the same policy. Copies drift, and the drift is invisible
        // until someone installs the package and gets different hardening from
        // the one they reviewed. This test is the thing that stops it.
        let packaged = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/linux/rusty_time.service"
        ))
        .expect("packaged unit must exist");

        // Every hardening property that matters must appear in both.
        for property in [
            "Type=notify",
            "AmbientCapabilities=CAP_SYS_TIME",
            "CapabilityBoundingSet=CAP_SYS_TIME",
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "MemoryDenyWriteExecute=yes",
            "StateDirectory=rusty_time",
            "RuntimeDirectory=rusty_time",
        ] {
            assert!(
                packaged.contains(property),
                "packaged unit is missing {property}"
            );
            #[cfg(target_os = "linux")]
            assert!(
                service_definition("/usr/sbin/rtimed").contains(property),
                "`service show` output is missing {property}"
            );
        }
        // The packaged unit additionally refuses to run beside another time
        // daemon, which is the failure an operator is least likely to diagnose.
        assert!(
            packaged.contains("Conflicts=") && packaged.contains("chronyd"),
            "packaged unit must conflict with other time daemons"
        );
    }

    #[test]
    fn notify_is_harmless_when_no_supervisor_is_listening() {
        // The common case: run from a shell, with or without a supervisor.
        // Either way this must not panic — a daemon that dies because it could
        // not announce itself has its priorities backwards.
        notify_ready("smoke test");
        notify_status("smoke test");
    }

    #[test]
    fn activation_delegates_to_the_platform_seam() {
        // The decision logic itself is tested in `rusty_time_clock::net` with
        // explicit inputs, where it needs no environment mutation. Here we only
        // confirm the daemon asks and accepts the answer.
        let adopted = activated_udp_socket();
        if std::env::var_os("LISTEN_FDS").is_none() {
            assert!(
                adopted.is_none(),
                "claimed a descriptor no supervisor passed"
            );
        }
    }
}
