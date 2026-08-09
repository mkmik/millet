//! Golden `--trace-json` traces. Regenerate with `MILLET_BLESS=1 cargo test`.

use std::path::PathBuf;

use millet_asm::asm;
use millet_core::Config;
use millet_sim::{run_capture, Options};

const PROGRAMS: &[&str] = &[
    "hello",
    "sum_unrolled",
    "scratch",
    "pick",
    "arraysum",
    "divmod",
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn traces_match_the_golden_files() {
    let cfg = Config::default();
    let mut stale = Vec::new();
    for name in PROGRAMS {
        let src =
            std::fs::read_to_string(root().join("examples").join(format!("{name}.mil"))).unwrap();
        let a = asm::assemble(&src, &cfg).expect("assembles");
        let r = run_capture(
            &a.image,
            Options {
                trace_json: true,
                ..Default::default()
            },
        );
        let actual = String::from_utf8(r.log).unwrap();
        let path = root().join("tests/golden").join(format!("{name}.jsonl"));

        if std::env::var("MILLET_BLESS").is_ok() {
            std::fs::write(&path, &actual).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (run with MILLET_BLESS=1 to create it)",
                path.display()
            )
        });
        if expected != actual {
            stale.push(name.to_string());
        }
    }
    assert!(
        stale.is_empty(),
        "golden traces differ for {stale:?}; inspect the change, then re-bless with MILLET_BLESS=1"
    );
}
