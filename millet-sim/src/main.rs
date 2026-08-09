//! `msim` — the Millet simulator.

use std::io::Write;
use std::process::ExitCode;

use millet_core::{Config, Image};
use millet_sim::{Machine, Options, Stop};

const USAGE: &str = "\
msim — Millet simulator

usage:
  msim [options] <image.mimg>

options:
  --trace              per-bundle belt state, drops and memory effects (stderr)
  --trace-json         same, machine-readable, one JSON object per line (stderr)
  --stats              print cycle count and slot occupancy on exit (stderr)
  --max-bundles <n>    stop after n bundles (default 100000000, 0 = unlimited)
  -h, --help           this message
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut path = None;
    let mut opts = Options {
        max_bundles: 100_000_000,
        ..Default::default()
    };
    let mut stats = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--trace" => opts.trace = true,
            "--trace-json" => opts.trace_json = true,
            "--stats" => stats = true,
            "--max-bundles" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse().ok()) {
                    Some(n) => opts.max_bundles = n,
                    None => {
                        eprintln!("msim: --max-bundles needs a number");
                        return ExitCode::from(2);
                    }
                }
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("msim: unknown option `{a}`\n{USAGE}");
                return ExitCode::from(2);
            }
            a => path = Some(a.to_string()),
        }
        i += 1;
    }

    let Some(path) = path else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("msim: {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let img = match Image::from_bytes(&bytes) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("msim: {path}: {e}");
            return ExitCode::from(2);
        }
    };

    let mut out = std::io::stdout();
    let mut log = std::io::stderr();
    let (stop, s) = {
        let mut m = Machine::new(&img, Config::default(), opts, &mut out, &mut log);
        let stop = m.run();
        (stop, m.stats.clone())
    };
    let _ = std::io::stdout().flush();

    if stats {
        eprintln!(
            "msim: {} bundles, {} calls, max depth {}, slot occupancy {:.1}% (a0 {} a1 {} m {} f {})",
            s.bundles,
            s.calls,
            s.max_depth,
            s.occupancy() * 100.0,
            s.slot_ops[0],
            s.slot_ops[1],
            s.slot_ops[2],
            s.slot_ops[3],
        );
    }

    match stop {
        Stop::Exit(c) => ExitCode::from(c as u8),
        Stop::Halt => {
            eprintln!("msim: halted");
            ExitCode::from(3)
        }
        Stop::Fault(m) => {
            eprintln!("msim: fault: {m}");
            ExitCode::from(70)
        }
    }
}
