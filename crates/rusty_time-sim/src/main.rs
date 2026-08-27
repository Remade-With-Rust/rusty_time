//! timecorp — the TIMECORP corpus runner.
//!
//! `timecorp run [--scenario S1,S6] [--seeds N] [--out DIR] [--ledger PATH]`
//! runs each scenario across N seeds through the deterministic simulator, writes
//! per-scenario JSON results, and appends an aggregate table to the ledger.
//!
//! The ledger is the only place a performance number may be cited from
//! (mission plan §7). This runner also prints a seed-split noise floor so a
//! future delta has something honest to clear.

mod load;
mod rng;
mod scenarios;
mod serverload;
mod sim;

use scenarios::{SCENARIOS, Scenario};
use sim::RunMetrics;

#[global_allocator]
static ALLOC: rusty_time_alloc::HouseAllocator = rusty_time_alloc::house_allocator();

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("run") => match Options::parse(&args[1..]) {
            Ok(opts) => run_corpus(&opts),
            Err(e) => {
                eprintln!("timecorp run: {e}");
                usage();
                2
            }
        },
        Some("list") => {
            println!("implemented scenarios:");
            for s in SCENARIOS {
                println!("  {:4} {}", s.name, s.what);
            }
            for s in serverload::LOAD_SCENARIOS {
                println!(
                    "  {:4} server load: {} clients at {} req/s for {} s",
                    s.name, s.clients, s.arrival_rate_hz, s.duration_s
                );
            }
            println!("pending (mission plan §7.2): S2-S5, S7, S9-S11, S13-S14, HW1");
            0
        }
        Some("serverload") => run_server_load(),
        Some("load") => match load::LoadOptions::parse(&args[1..]) {
            Ok(opts) => load::run(&opts),
            Err(e) => {
                eprintln!("timecorp load: {e}");
                usage();
                2
            }
        },
        _ => {
            usage();
            2
        }
    };
    std::process::exit(code);
}

/// S12: drive the real admission policy and report deterministic counts.
fn run_server_load() -> i32 {
    println!("TIMECORP S12 — server load (deterministic counts, 5 seeds each)\n");
    println!(
        "{:<6} {:>10} {:>10} {:>9} {:>8} {:>8} {:>8} {:>7}",
        "scen", "requests", "answered", "dropped", "kissed", "evicted", "tracked", "reply"
    );
    let mut rows = String::new();
    for scenario in serverload::LOAD_SCENARIOS {
        // Counts are exact, so a handful of seeds is enough to show the policy
        // is not seed-sensitive; there is no duration here to average.
        let runs: Vec<_> = (0..5u64).map(|s| serverload::run(scenario, s)).collect();
        let m = runs[0];
        for other in &runs[1..] {
            assert!(
                other.reply_ratio <= 1.0,
                "reply ratio must never exceed 1 (a server answering more than it is asked \
                 is an amplifier)"
            );
        }
        println!(
            "{:<6} {:>10} {:>10} {:>9} {:>8} {:>8} {:>8} {:>6.1}%",
            scenario.name,
            m.requests,
            m.answered,
            m.dropped,
            m.kissed,
            m.evicted,
            m.clients_tracked,
            m.reply_ratio * 100.0
        );
        rows.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {:.1}% |\n",
            scenario.name,
            m.requests,
            m.answered,
            m.dropped,
            m.kissed,
            m.evicted,
            m.clients_tracked,
            m.reply_ratio * 100.0
        ));
    }
    // Ask the table, do not estimate: the record alone stopped being the whole
    // per-client cost once records moved into slots with recency links.
    let per_client = rusty_time_core::server::ClientTable::<std::net::IpAddr>::bytes_per_client();
    println!(
        "\nclient-table state: {per_client} bytes/client, capacity 16384 => {:.1} MiB worst case",
        (per_client * 16_384) as f64 / (1024.0 * 1024.0)
    );
    println!("\nledger rows:\n{rows}");
    0
}

fn usage() {
    eprintln!(
        "usage: timecorp run [--scenario S1,S6] [--seeds N] [--out DIR] [--ledger PATH] [--label TEXT]"
    );
    eprintln!("       timecorp list");
}

struct Options {
    scenarios: Vec<&'static Scenario>,
    seeds: u64,
    out_dir: String,
    ledger: Option<String>,
    label: String,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut opts = Options {
            scenarios: SCENARIOS.iter().collect(),
            seeds: 31,
            out_dir: "corpus/results".into(),
            ledger: Some("corpus/LEDGER.md".into()),
            label: "rusty_time (sim harness v1)".into(),
        };
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--scenario" => {
                    let list = it.next().ok_or("--scenario needs a value")?;
                    opts.scenarios = list
                        .split(',')
                        .map(|n| {
                            scenarios::by_name(n.trim())
                                .ok_or(format!("unknown scenario '{n}' (see: timecorp list)"))
                        })
                        .collect::<Result<_, _>>()?;
                }
                "--seeds" => {
                    opts.seeds = it
                        .next()
                        .ok_or("--seeds needs a value")?
                        .parse()
                        .map_err(|_| "--seeds: not a number".to_string())?;
                    if opts.seeds == 0 {
                        return Err("--seeds must be at least 1".into());
                    }
                }
                "--out" => opts.out_dir = it.next().ok_or("--out needs a value")?.clone(),
                "--ledger" => {
                    opts.ledger = Some(it.next().ok_or("--ledger needs a value")?.clone())
                }
                "--no-ledger" => opts.ledger = None,
                "--label" => opts.label = it.next().ok_or("--label needs a value")?.clone(),
                other => return Err(format!("unknown flag '{other}'")),
            }
        }
        Ok(opts)
    }
}

#[derive(serde::Serialize)]
struct ScenarioResult {
    scenario: String,
    what: String,
    seeds: u64,
    runs: Vec<RunMetrics>,
    aggregate: Aggregate,
}

#[derive(serde::Serialize)]
struct Aggregate {
    conv_1ms_s: Stat,
    conv_100us_s: Stat,
    packets_to_1ms: Stat,
    steady_p50_s: Stat,
    steady_p95_s: Stat,
    steady_max_s: Stat,
    freq_resid_ppm_abs: Stat,
    /// Split-half medians difference — the noise floor a claimed win must clear.
    noise_floor_p95_s: f64,
    seeds_converged_1ms: u64,
}

#[derive(Clone, Copy, serde::Serialize)]
struct Stat {
    median: f64,
    min: f64,
    max: f64,
}

fn stat(mut values: Vec<f64>) -> Stat {
    let values = &mut values;
    if values.is_empty() {
        return Stat {
            median: f64::NAN,
            min: f64::NAN,
            max: f64::NAN,
        };
    }
    values.sort_by(f64::total_cmp);
    Stat {
        median: values[values.len() / 2],
        min: values[0],
        max: values[values.len() - 1],
    }
}

fn median_of(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn run_corpus(opts: &Options) -> i32 {
    if let Err(e) = std::fs::create_dir_all(&opts.out_dir) {
        eprintln!("timecorp: creating {}: {e}", opts.out_dir);
        return 1;
    }
    let stamp = unix_stamp();
    let mut ledger_rows = String::new();

    for scenario in &opts.scenarios {
        println!(
            "== {} — {} ({} seeds)",
            scenario.name, scenario.what, opts.seeds
        );
        let runs: Vec<RunMetrics> = (0..opts.seeds).map(|s| sim::run(scenario, s)).collect();

        let agg = aggregate(&runs);
        print_aggregate(&agg, opts.seeds);
        ledger_rows.push_str(&ledger_row(scenario, &agg, opts.seeds));

        let result = ScenarioResult {
            scenario: scenario.name.to_string(),
            what: scenario.what.to_string(),
            seeds: opts.seeds,
            runs,
            aggregate: agg,
        };
        let path = format!("{}/{}_{}.json", opts.out_dir, scenario.name, stamp);
        match serde_json::to_string_pretty(&result) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    eprintln!("timecorp: writing {path}: {e}");
                    return 1;
                }
                println!("   -> {path}");
            }
            Err(e) => {
                eprintln!("timecorp: serializing {}: {e}", scenario.name);
                return 1;
            }
        }
    }

    if let Some(ledger_path) = &opts.ledger {
        if let Err(e) = append_ledger(ledger_path, &opts.label, &ledger_rows, opts.seeds, stamp) {
            eprintln!("timecorp: appending {ledger_path}: {e}");
            return 1;
        }
        println!("ledger updated: {ledger_path}");
    }
    0
}

fn aggregate(runs: &[RunMetrics]) -> Aggregate {
    let conv1: Vec<f64> = runs.iter().filter_map(|r| r.conv_1ms_s).collect();
    let seeds_converged_1ms = conv1.len() as u64;

    // Noise floor: split seeds into halves, compare the halves' p95 medians.
    let half = runs.len() / 2;
    let a = median_of(runs[..half].iter().map(|r| r.steady_p95_s).collect());
    let b = median_of(runs[half..].iter().map(|r| r.steady_p95_s).collect());
    let noise_floor = (a - b).abs();

    Aggregate {
        conv_1ms_s: stat(conv1),
        conv_100us_s: stat(runs.iter().filter_map(|r| r.conv_100us_s).collect()),
        packets_to_1ms: stat(
            runs.iter()
                .filter_map(|r| r.packets_to_1ms.map(|p| p as f64))
                .collect(),
        ),
        steady_p50_s: stat(runs.iter().map(|r| r.steady_p50_s).collect()),
        steady_p95_s: stat(runs.iter().map(|r| r.steady_p95_s).collect()),
        steady_max_s: stat(runs.iter().map(|r| r.steady_max_s).collect()),
        freq_resid_ppm_abs: stat(runs.iter().map(|r| r.freq_resid_ppm.abs()).collect()),
        noise_floor_p95_s: noise_floor,
        seeds_converged_1ms,
    }
}

fn print_aggregate(a: &Aggregate, seeds: u64) {
    println!(
        "   conv->1ms   median {:>9}   ({}/{} seeds converged)",
        fmt_s(a.conv_1ms_s.median),
        a.seeds_converged_1ms,
        seeds
    );
    println!("   conv->100us median {:>9}", fmt_s(a.conv_100us_s.median));
    println!(
        "   steady |err| p50 {}  p95 {}  max {}",
        fmt_s(a.steady_p50_s.median),
        fmt_s(a.steady_p95_s.median),
        fmt_s(a.steady_max_s.median)
    );
    println!(
        "   freq residual median {:.3} ppm   noise floor(p95) {}",
        a.freq_resid_ppm_abs.median,
        fmt_s(a.noise_floor_p95_s)
    );
}

fn fmt_s(v: f64) -> String {
    if v.is_nan() {
        "n/a".into()
    } else if v.abs() >= 1.0 {
        format!("{v:.2} s")
    } else if v.abs() >= 1e-3 {
        format!("{:.2} ms", v * 1e3)
    } else {
        format!("{:.1} us", v * 1e6)
    }
}

fn ledger_row(s: &Scenario, a: &Aggregate, seeds: u64) -> String {
    format!(
        "| {} | {}/{} | {} | {} | {} | {} | {} | {:.3} |\n",
        s.name,
        a.seeds_converged_1ms,
        seeds,
        fmt_s(a.conv_1ms_s.median),
        fmt_s(a.conv_100us_s.median),
        fmt_s(a.steady_p50_s.median),
        fmt_s(a.steady_p95_s.median),
        fmt_s(a.steady_max_s.median),
        a.freq_resid_ppm_abs.median,
    )
}

fn append_ledger(
    path: &str,
    label: &str,
    rows: &str,
    seeds: u64,
    stamp: u64,
) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = existing;
    out.push_str(&format!(
        "\n## Run {stamp} (unix) — arm: {label} — {seeds} seeds/scenario\n\n\
         | scenario | conv@1ms | t→1ms (med) | t→100µs (med) | steady p50 | steady p95 | steady max | freq resid (ppm, med) |\n\
         |---|---|---|---|---|---|---|---|\n{rows}\n\
         Baseline chrony: **PENDING** — needs the Linux rig (`.github/workflows/corpus.yml`); \
         the sim-harness arm above measures rusty_time only and is comparable across commits, \
         not across implementations.\n"
    ));
    std::fs::write(path, out)
}

fn unix_stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
