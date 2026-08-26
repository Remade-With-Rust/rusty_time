//! `rtimed service` — print or install this platform's service definition.
//!
//! Printing is the default because installing writes to a system directory,
//! and an operator should be able to read what would be written before it is.

use crate::service;

pub fn run(args: &[String]) -> i32 {
    let exec = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "rtimed".to_string());

    match args.first().map(String::as_str) {
        Some("show") | None => {
            println!("{}", service::service_definition(&exec));
            eprintln!("# would install to: {}", service::service_definition_path());
            0
        }
        Some("install") => install(&exec),
        Some("path") => {
            println!("{}", service::service_definition_path());
            0
        }
        _ => {
            eprintln!("usage: rtimed service <show|install|path>");
            eprintln!();
            eprintln!("  show     print the unit/plist/script for this platform (default)");
            eprintln!("  install  write it to the system location (needs privilege)");
            eprintln!("  path     print where install would write");
            2
        }
    }
}

fn install(exec: &str) -> i32 {
    let path = service::service_definition_path();
    let body = service::service_definition(exec);
    match std::fs::write(path, &body) {
        Ok(()) => {
            println!("wrote {path}");
            #[cfg(target_os = "linux")]
            println!("next: systemctl daemon-reload && systemctl enable --now rusty_time");
            #[cfg(target_os = "macos")]
            println!("next: launchctl load -w {path}");
            #[cfg(windows)]
            println!("next: run {path} from an elevated prompt");
            0
        }
        Err(e) => {
            eprintln!("rtimed service install: writing {path}: {e}");
            eprintln!("       (this location usually needs root/Administrator)");
            1
        }
    }
}
