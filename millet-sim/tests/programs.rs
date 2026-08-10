//! End-to-end: assemble every example, run it, check what it produced.
//!
//! Also the differential check of PRD §9.1 — the assembler's statically
//! predicted belt liveness for each bundle against what the simulator
//! actually had live when it executed that bundle.

use std::path::PathBuf;

use millet_asm::asm;
use millet_core::Config;
use millet_sim::{Options, Stop, run_capture};

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("examples")
}

struct Outcome {
    stop: Stop,
    out: String,
}

fn run(name: &str) -> Outcome {
    let src = std::fs::read_to_string(examples_dir().join(format!("{name}.mil")))
        .unwrap_or_else(|e| panic!("{name}.mil: {e}"));
    let cfg = Config::default();
    let a = match asm::assemble(&src, &cfg) {
        Ok(a) => a,
        Err(e) => {
            let msgs: Vec<String> = e.diags.iter().map(|d| d.render(name)).collect();
            panic!("{name}.mil failed to assemble:\n{}", msgs.join("\n"));
        }
    };
    assert!(
        a.diags.is_empty(),
        "{name}.mil assembled with warnings:\n{}",
        a.diags
            .iter()
            .map(|d| d.render(name))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let r = run_capture(
        &a.image,
        Options {
            max_bundles: 50_000_000,
            ..Default::default()
        },
    );

    // Differential: predicted liveness vs. actual, for every executed bundle.
    for p in &r.points {
        assert_eq!(
            a.prediction.live[p.bundle], p.live,
            "{name}.mil: bundle {} entered with live mask {:#06x}, assembler predicted {:#06x}",
            p.bundle, p.live, a.prediction.live[p.bundle]
        );
    }

    Outcome {
        stop: r.stop,
        out: String::from_utf8_lossy(&r.out).into_owned(),
    }
}

fn expect_exit(name: &str, code: i32) -> String {
    let o = run(name);
    assert_eq!(o.stop, Stop::Exit(code), "{name}.mil exit status");
    o.out
}

#[test]
fn hello_writes_a_string() {
    assert_eq!(expect_exit("hello", 0), "hello, millet\n");
}

#[test]
fn unrolled_sum_with_four_loads_in_flight() {
    expect_exit("sum_unrolled", 10);
}

#[test]
fn scratchpad_round_trip_and_mul_latency() {
    expect_exit("scratch", 91);
}

#[test]
fn pick_is_a_conditional_move() {
    expect_exit("pick", 91);
}

#[test]
fn strlen_loop() {
    expect_exit("strlen", 13);
}

#[test]
fn arraysum_loop_reshaped_with_rescue() {
    expect_exit("arraysum", 150);
}

#[test]
fn memcpy_defers_its_load_four_bundles() {
    assert_eq!(
        expect_exit("memcpy", 0),
        "millet: copied across a back edge\n"
    );
}

#[test]
fn recursive_fib_20() {
    assert_eq!(expect_exit("fib", 0), "6765\n");
}

#[test]
fn ackermann_2_3() {
    expect_exit("ackermann", 9);
}

#[test]
fn divmod_returns_two_results() {
    expect_exit("divmod", 142);
}
