//! §14 — the disassembler round-trips the whole corpus byte-identically.

use std::path::PathBuf;

use millet_asm::{asm, disasm};
use millet_core::Config;

fn examples() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("examples");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension()?.to_str()? == "mil").then_some(p)
        })
        .collect();
    v.sort();
    assert!(!v.is_empty(), "no examples found");
    v
}

#[test]
fn every_example_round_trips_byte_identically() {
    let cfg = Config::default();
    for path in examples() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        let first = asm::assemble(&src, &cfg)
            .unwrap_or_else(|e| {
                let m: Vec<String> = e.diags.iter().map(|d| d.render(&name)).collect();
                panic!("{name}:\n{}", m.join("\n"))
            })
            .image;
        let text = disasm::disassemble(&first).unwrap();
        let second = asm::assemble(&text, &cfg)
            .unwrap_or_else(|e| {
                let m: Vec<String> = e.diags.iter().map(|d| d.render("<disassembly>")).collect();
                panic!(
                    "{name} disassembly did not re-assemble:\n{text}\n{}",
                    m.join("\n")
                )
            })
            .image;
        assert_eq!(
            first.to_bytes(),
            second.to_bytes(),
            "{name} did not round-trip"
        );
    }
}
