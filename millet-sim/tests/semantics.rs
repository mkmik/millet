//! Focused tests for the rules the PRD calls out as subtle or high-risk.

use millet_asm::asm;
use millet_core::isa::{BrKind, Op, encode};
use millet_core::{Config, EbbEntry, FuncEntry, Image};
use millet_sim::{Options, Stop, run_capture};

fn build(src: &str) -> asm::Assembled {
    match asm::assemble(src, &Config::default()) {
        Ok(a) => a,
        Err(e) => panic!(
            "assembly failed:\n{}",
            e.diags
                .iter()
                .map(|d| d.render("<test>"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

fn exit_code(src: &str) -> i32 {
    let a = build(src);
    match run_capture(&a.image, Options::default()).stop {
        Stop::Exit(c) => c,
        other => panic!("expected an exit, got {other:?}"),
    }
}

fn stop_of(src: &str) -> Stop {
    let a = build(src);
    run_capture(&a.image, Options::default()).stop
}

/// PRD §7.2 — "the highest-risk semantic rule in the document". A value
/// computed in the same bundle as the reshape can be rescued, named by the
/// position it occupies *after* this bundle's drops.
#[test]
fn rescue_names_the_post_drop_belt() {
    assert_eq!(
        exit_code(
            "
.func main(0) -> 0
    a0  con 7
    a1  con 9
    f   rescue 0b01

    f   sys 0
"
        ),
        9
    );
}

#[test]
fn conform_names_the_post_drop_belt() {
    assert_eq!(
        exit_code(
            "
.func main(0) -> 0
    a0  con 7
    a1  con 9
    f   conform b1

    f   sys 0
"
        ),
        7
    );
}

/// PRD §7.1 — `conform` puts the first-listed operand at b0, the opposite of
/// `call`/`retn`.
#[test]
fn conform_is_first_listed_to_b0() {
    assert_eq!(
        exit_code(
            "
.func main(0) -> 0
    a0  con 3
    a1  con 4
                ; a0 drops first, so b0=4 and b1=3

    f   conform b1, b0
                ; first-listed b1 becomes b0

    f   sys 0
"
        ),
        3
    );
}

/// PRD §3.1 — a load issued three bundles ago retires *before* an ALU result
/// computed in the current bundle.
#[test]
fn drops_sort_by_issuing_bundle_then_slot() {
    assert_eq!(
        exit_code(
            "
.data 0x1000
.u64 42

.func main(0) -> 0
    a0  con 0x1000

    m   load b0, 0, 8, zero, 3

    a0  nop

    a0  con 5

    f   conform b1

    f   sys 0
"
        ),
        42
    );
}

/// PRD §8.2 — a load samples memory at issue, so a store inside its delay
/// window is invisible to it.
#[test]
fn a_load_does_not_see_a_store_issued_during_its_delay() {
    let src = "
.data 0x1000
.u64 7

.func main(0) -> 0
    a0  con 0x1000
    a1  con 99

    m   load b1, 0, 8, zero, 4

    m   store b1, 0, 8, b0

    a0  nop

    a0  nop

    f   sys 0
";
    // The assembler proves the overlap and says so.
    let a = build(src);
    assert!(a.diags.iter().any(|d| d.code == "E8" && d.warning));
    assert_eq!(exit_code(src), 7);
}

/// PRD §6.2 / §8.2 — latencies count bundles of the issuing frame only; a
/// callee's execution consumes none of them.
#[test]
fn call_latency_is_frame_local() {
    // The load is issued in bundle 1 with delay 3, so it retires at the end
    // of bundle 3 of *this* frame no matter how long the callee runs. Bundle
    // 2 therefore still sees the address at b0, not the loaded value.
    assert_eq!(
        exit_code(
            "
.data 0x1000
.u64 0x1234

.func main(0) -> 0
    a0  con 0x1000

    m   load b0, 0, 8, zero, 3
    f   call slow

    a0  or b0, b0

    a0  nop

    f   conform b1

    f   sys 0

.func slow(0) -> 0
    a0  nop

    a0  nop

    a0  nop

    a0  nop

    f   retn
"
        ),
        0x00 // 0x1000 & 0xff — the address, not 0x1234
    );
}

/// PRD §4 — taking an edge truncates the belt to the target's arity; the
/// positions above it read as None (§8.6). The assembler will not let you read
/// them, so this one is built by hand.
#[test]
fn edges_truncate_the_belt_in_hardware() {
    let bundles = vec![
        [
            encode(&Op::Con { imm: 5 }).unwrap(),
            encode(&Op::Con { imm: 6 }).unwrap(),
            0,
            0,
        ],
        [
            0,
            0,
            0,
            encode(&Op::Br {
                kind: BrKind::Always,
                cond: 0,
                target: 1,
            })
            .unwrap(),
        ],
        [encode(&Op::IsNone { a: 1 }).unwrap(), 0, 0, 0],
        [0, 0, 0, encode(&Op::Sys(0)).unwrap()],
    ];
    let img = Image {
        bundles,
        ebbs: vec![EbbEntry::new(0, 0), EbbEntry::new(2, 1)],
        funcs: vec![FuncEntry::new(0, 0, 0)],
        data: vec![],
        entry: 0,
    };
    // b0=6 survives the edge; b1 (which held 5) is gone, and `isnone` is the
    // one way to see that without realizing it.
    assert_eq!(
        run_capture(&img, Options::default()).stop,
        Stop::Exit(1),
        "arity-1 entry should have truncated b1 away"
    );
}

/// Still a fault, but a deferred one: the `div` drops a NaR and the `sys`
/// realizes it (§8.6).
#[test]
fn division_by_zero_faults() {
    assert!(matches!(
        stop_of(
            "
.func main(0) -> 0
    a0  con 1
    a1  con 0

    a0  div b1, b0

    a0  nop

    a0  nop

    a0  nop

    a0  nop

    a0  nop

    a0  nop

    a0  nop

    f   sys 0
"
        ),
        Stop::Fault(_)
    ));
}

/// Likewise deferred — the load itself is speculable, so the `sys` is where
/// this one lands.
#[test]
fn loading_from_an_unbacked_address_faults() {
    assert!(matches!(
        stop_of(
            "
.func main(0) -> 0
    a0  con 0x7f0000

    m   load b0, 0, 8, zero, 3

    a0  nop

    a0  nop

    f   sys 0
"
        ),
        Stop::Fault(_)
    ));
}

// -- operand metadata (PRD §8.6) --------------------------------------------

/// A load that cannot be satisfied drops a NaR and carries on; the fault is
/// raised by the store that realizes it, and says where the NaR came from.
#[test]
fn a_store_realizes_a_nar_and_names_its_origin() {
    let Stop::Fault(msg) = stop_of(
        "
.func main(0) -> 0
    a0  con 0x7f0000            ; nothing is mapped there

    m   load b0, 0, 8, zero, 3

    a0  nop

    a0  nop
                                ; b0=NaR b1=addr

    m   store b1, 0, 8, b0

    f   sys 0
",
    ) else {
        panic!("the store should have realized the NaR");
    };
    assert!(
        msg.contains("store") && msg.contains("bundle 1") && msg.contains("unbacked"),
        "the diagnostic should point at the load that failed: {msg}"
    );
}

/// The point of the whole exercise: `pick` drops the path not taken, NaR and
/// all, so a load can be hoisted above the test that guards it.
#[test]
fn pick_discards_the_nar_it_did_not_select() {
    assert_eq!(
        exit_code(
            "
.func main(0) -> 0
    a0  con 0x7f0000
    a1  con 7
                                ; b0=7 b1=addr

    m   load b1, 0, 8, zero, 3
    a0  con 0                   ; a false guard
                                ; b0=cond b1=7 b2=addr

    a0  nop

    a0  nop
                                ; b0=NaR b1=cond b2=7 b3=addr

    a0  pick b1, b0, b2

    f   sys 0
"
        ),
        7
    );
}

/// None wins over NaR — a None means the operation was never meant to happen,
/// so it must not report a fault. The store below is suppressed, not realized.
#[test]
fn none_beats_nar_and_suppresses_the_store() {
    assert_eq!(
        exit_code(
            "
.func main(0) -> 0
    a0  con 0x7f0000

    m   load b0, 0, 8, zero, 3
    a0  none
                                ; b0=None b1=addr

    a0  nop

    a0  nop
                                ; b0=NaR b1=None b2=addr

    a0  add b0, b1              ; NaR + None = None

    m   store b2, 0, 8, b0      ; suppressed; a NaR here would fault

    a0  con 0

    f   sys 0
"
        ),
        0
    );
}

/// The scratchpad holds operands, not bytes: metadata survives a spill/fill
/// round trip. Memory does not — which is why a store realizes instead.
#[test]
fn the_scratchpad_preserves_metadata() {
    assert_eq!(
        exit_code(
            "
.func main(0) -> 0
    a0  con 0x7f0000

    m   load b0, 0, 8, zero, 3

    a0  nop

    a0  nop
                                ; b0=NaR

    m   spill s0, b0

    m   fill s0                 ; latency 3

    a0  nop

    a0  nop
                                ; b0=the same NaR

    a0  isnar b0

    f   sys 0
"
        ),
        1
    );
}

/// Control flow is not speculable either.
#[test]
fn a_branch_realizes_a_poisoned_condition() {
    let stop = stop_of(
        "
.func main(0) -> 0
    a0  con 0x7f0000

    m   load b0, 0, 8, zero, 3

    a0  nop

    a0  nop

    f   brt b0, done

.ebb done(0)
    f   halt
",
    );
    match stop {
        Stop::Fault(msg) => assert!(msg.contains("branch"), "{msg}"),
        other => panic!("a branch on a NaR should fault, got {other:?}"),
    }
}

#[test]
fn halt_is_an_abnormal_stop() {
    assert_eq!(
        stop_of(
            "
.func main(0) -> 0
    f   halt
"
        ),
        Stop::Halt
    );
}

/// PRD §6.1 — the callee's belt and scratchpad start empty; nothing of the
/// caller's leaks through. "Empty" is now a None rather than a zero (§8.6),
/// which is what lets the fill say so instead of inventing a value.
#[test]
fn a_callee_gets_a_fresh_belt_and_scratchpad() {
    assert_eq!(
        exit_code(
            "
.func main(0) -> 0
    a0  con 111

    m   spill s0, b0

    f   call probe

    f   sys 0

.func probe(0) -> 1
    m   fill s0

    a0  nop

    a0  nop

    a0  isnone b0

    f   retn b0
"
        ),
        1
    );
}
