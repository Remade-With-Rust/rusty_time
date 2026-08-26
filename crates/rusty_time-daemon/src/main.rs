//! rtimed — the rusty_time daemon.
//!
//! M1/M2 surface: `rtimed query <server>` — a one-shot SNTP measurement against a
//! real server, the first end-to-end proof of the packet codec + filter on live
//! network. The resident daemon loop (discipline against the platform driver,
//! control socket, server mode) lands across M3–M5.

mod query;

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
        "usage: rtimed query <server> [--count N] [--interval-ms N] [--timeout-ms N] [--port N] [--json]"
    );
    eprintln!("       rtimed version");
}
