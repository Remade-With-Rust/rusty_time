//! rtimed — the rusty_time daemon.
//!
//! M1/M2 surface: `rtimed query <server>` — a one-shot SNTP measurement against a
//! real server, the first end-to-end proof of the packet codec + filter on live
//! network. The resident daemon loop (discipline against the platform driver,
//! control socket, server mode) lands across M3–M5.

mod control;
mod gateway;
mod nts_session;
mod query;
mod refclock_cmd;
mod server;
mod service;
mod service_cmd;
mod state_cmd;
mod store;
mod sync;

#[global_allocator]
static ALLOC: rusty_time_alloc::HouseAllocator = rusty_time_alloc::house_allocator();

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("query") => match query::Options::parse(&args[1..]) {
            Ok(opts) => query::run(&opts),
            Err(msg) => {
                eprintln!("rtimed query: {msg}");
                usage();
                2
            }
        },
        Some("serve") => match server::ServeOptions::parse(&args[1..]) {
            Ok(opts) => server::run(&opts),
            Err(msg) => {
                eprintln!("rtimed serve: {msg}");
                usage();
                2
            }
        },
        Some("sync") => match sync::SyncOptions::parse(&args[1..]) {
            Ok(opts) => sync::run(&opts),
            Err(msg) => {
                eprintln!("rtimed sync: {msg}");
                usage();
                2
            }
        },
        Some("state") => state_cmd::run(&args[1..]),
        Some("service") => service_cmd::run(&args[1..]),
        Some("refclock") => refclock_cmd::run(&args[1..]),
        Some("version") | Some("--version") => {
            println!("rtimed {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            usage();
            2
        }
    };
    std::process::exit(code);
}

fn usage() {
    eprintln!(
        "usage: rtimed query <server> [--nts] [--count N] [--interval-ms N] [--timeout-ms N]"
    );
    eprintln!("                            [--port N] [--ke-port N] [--json]");
    eprintln!("       rtimed serve [--nts] [--bind ADDR] [--ke-bind ADDR] [--stratum N]");
    eprintln!("                    [--cert FILE] [--key FILE] [--nts-name NAME]");
    eprintln!("                    [--gateway ADDR] [--gateway-assets DIR]");
    eprintln!("       rtimed sync <server>... [--dry-run] [--seconds N] [--minpoll N]");
    eprintln!("                   [--maxpoll N] [--makestep T N] [--verbose]");
    eprintln!("       rtimed state <show|merge> ...");
    eprintln!("       rtimed service <show|install|path>");
    eprintln!("       rtimed refclock <shm|sock|phc> ...");
    eprintln!("       rtimed version");
    eprintln!();
    eprintln!("  --nts   query: authenticate with NTS (RFC 8915) — key establishment on TCP 4460,");
    eprintln!("          then every exchange is AEAD-protected and verified before use.");
    eprintln!("          serve: also run an NTS-KE listener and answer protected requests.");
}
