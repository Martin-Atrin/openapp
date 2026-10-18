//! OpenApp agent evaluation runs (not tests): `probes` (eval/probes/PROBES.md, E1–E12) and `bakeoff`
//! (Codex vs Pi). Both drive a real harness in a real app VM through one scripted user (`eval/driver.rs`).
//!
//! Run:  cargo run --example eval -- probes --harness codex|pi [--only E1,E3] [--timeout-min 20] [--vm-home <dir>]
//!                                          [--dry-run] [--force] [--keep-ws] [--real-folders] [--ws-root <dir>] [--followups 2]
//!       cargo run --example eval -- bakeoff [--force]   (env: see eval/bakeoff.rs)
//! Out:  eval/probes/LOOP-<n>.md + eval/probes/raw/<n>/<id>.json; eval/bakeoff/RESULTS-<date>.md
//!
//! The VM home must hold the harness login (default ~/.openapp/vm-bakeoff, a scratch state). Both refuse to
//! run while a VM of that state looks running (two VMs must never mount one /opt/openapp volume).

#[path = "eval/bakeoff.rs"]
mod bakeoff;
#[path = "eval/driver.rs"]
mod driver;
#[path = "eval/probe_defs.rs"]
mod probe_defs;
#[path = "eval/probes.rs"]
mod probes;

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // cargo passes "--" through already; tolerate a stray one
    args.retain(|a| a != "--");
    let sub = if args.is_empty() { String::new() } else { args.remove(0) };
    match sub.as_str() {
        "probes" => probes::main(args),
        "bakeoff" => bakeoff::main(args),
        _ => {
            eprintln!("usage: eval probes|bakeoff [flags]\n  {}\n  {}", probes::USAGE, bakeoff::USAGE);
            std::process::exit(2);
        }
    }
}
