//! `rtimed state` — operations on the durable store.
//!
//! Every capability is an op before it is a button (mission plan §5): these are
//! the same calls the mesh transport will make when it lands, exposed now so a
//! human, a test and an agent can all drive them.

use crate::store::Store;

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("show") => match show(args.get(1).map(String::as_str)) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("rtimed state show: {e}");
                1
            }
        },
        Some("merge") => match merge(
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
        ) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("rtimed state merge: {e}");
                1
            }
        },
        _ => {
            eprintln!("usage: rtimed state show <file>");
            eprintln!("       rtimed state merge <peer-file> <into-file>");
            eprintln!();
            eprintln!("  Both read RUSTY_TIME_STATE_PASSPHRASE. `merge` folds a peer replica's");
            eprintln!("  CRDT state into a local store — the same operation the mesh performs.");
            2
        }
    }
}

fn passphrase() -> Result<String, String> {
    std::env::var("RUSTY_TIME_STATE_PASSPHRASE").map_err(|_| {
        "set RUSTY_TIME_STATE_PASSPHRASE (the state file holds NTS keys and cookies)".to_string()
    })
}

fn show(path: Option<&str>) -> Result<i32, String> {
    let path = path.ok_or("a state file path is required")?;
    let pass = passphrase()?;
    let mut store = Store::open(path, pass.as_bytes()).map_err(|e| e.to_string())?;

    let keys = store.all_master_keys().map_err(|e| e.to_string())?;
    println!("state file : {path}");
    println!("nts master keys : {}", keys.len());
    for k in &keys {
        // Never print key material — only its identifier.
        println!("  id {:#010x}", k.id);
    }
    Ok(0)
}

fn merge(from: Option<&str>, into: Option<&str>) -> Result<i32, String> {
    let (from, into) = (
        from.ok_or("a peer state file is required")?,
        into.ok_or("a destination state file is required")?,
    );
    let pass = passphrase()?;
    let peer = Store::open(from, pass.as_bytes()).map_err(|e| e.to_string())?;
    let mut local = Store::open(into, pass.as_bytes()).map_err(|e| e.to_string())?;
    local.merge_from(&peer).map_err(|e| e.to_string())?;
    local.flush().map_err(|e| e.to_string())?;
    println!("merged {from} into {into}");
    Ok(0)
}
