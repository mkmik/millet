//! `mas` — the Millet assembler (and, with `-d`, disassembler).

use std::path::PathBuf;
use std::process::ExitCode;

use millet_asm::{asm, disasm};
use millet_core::{Config, Image};

const USAGE: &str = "\
mas — Millet assembler

usage:
  mas [options] <input.mil>       assemble
  mas -d <input.mimg>             disassemble to stdout

options:
  -o <file>        output path (default: input with .mimg)
  -d               disassemble an image instead of assembling
  --check          run the static checks only, write nothing
  --predict <file> write the predicted per-bundle belt liveness (JSON)
  -Werror          treat warnings as errors
  -h, --help       this message
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut predict: Option<PathBuf> = None;
    let mut dis = false;
    let mut check_only = false;
    let mut werror = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-d" => dis = true,
            "--check" => check_only = true,
            "-Werror" => werror = true,
            "-o" => {
                i += 1;
                output = args.get(i).map(PathBuf::from);
            }
            "--predict" => {
                i += 1;
                predict = args.get(i).map(PathBuf::from);
            }
            a if a.starts_with('-') && a.len() > 1 => {
                eprintln!("mas: unknown option `{a}`\n{USAGE}");
                return ExitCode::from(2);
            }
            a => input = Some(PathBuf::from(a)),
        }
        i += 1;
    }

    let Some(input) = input else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let cfg = Config::default();

    if dis {
        let bytes = match std::fs::read(&input) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("mas: {}: {e}", input.display());
                return ExitCode::from(2);
            }
        };
        let img = match Image::from_bytes(&bytes) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("mas: {}: {e}", input.display());
                return ExitCode::from(2);
            }
        };
        match disasm::disassemble(&img) {
            Ok(text) => {
                print!("{text}");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("mas: {e}");
                return ExitCode::from(2);
            }
        }
    }

    let src = match std::fs::read_to_string(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mas: {}: {e}", input.display());
            return ExitCode::from(2);
        }
    };
    let path = input.display().to_string();

    match asm::assemble(&src, &cfg) {
        Err(e) => {
            let mut diags = e.diags;
            diags.sort_by_key(|d| d.line);
            for d in &diags {
                eprintln!("{}", d.render(&path));
            }
            let n = diags.iter().filter(|d| !d.warning).count();
            eprintln!("mas: {n} error(s)");
            ExitCode::from(1)
        }
        Ok(a) => {
            for d in &a.diags {
                eprintln!("{}", d.render(&path));
            }
            if werror && !a.diags.is_empty() {
                eprintln!("mas: warnings treated as errors");
                return ExitCode::from(1);
            }
            if let Some(p) = predict {
                let items: Vec<String> = a
                    .prediction
                    .live
                    .iter()
                    .enumerate()
                    .map(|(i, m)| format!("{{\"bundle\":{i},\"live\":{m}}}"))
                    .collect();
                if let Err(e) = std::fs::write(&p, format!("[{}]\n", items.join(","))) {
                    eprintln!("mas: {}: {e}", p.display());
                    return ExitCode::from(2);
                }
            }
            if check_only {
                return ExitCode::SUCCESS;
            }
            let out = output.unwrap_or_else(|| input.with_extension("mimg"));
            if let Err(e) = std::fs::write(&out, a.image.to_bytes()) {
                eprintln!("mas: {}: {e}", out.display());
                return ExitCode::from(2);
            }
            ExitCode::SUCCESS
        }
    }
}
