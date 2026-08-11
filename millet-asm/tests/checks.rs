//! One test per static check of PRD §9.3, plus the encoding guards.

use millet_asm::asm;
use millet_core::Config;

/// Assemble and return (error codes, warning codes).
fn codes(src: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    match asm::assemble(src, &Config::default()) {
        Ok(a) => (vec![], a.diags.iter().map(|d| d.code).collect()),
        Err(e) => (
            e.diags
                .iter()
                .filter(|d| !d.warning)
                .map(|d| d.code)
                .collect(),
            e.diags
                .iter()
                .filter(|d| d.warning)
                .map(|d| d.code)
                .collect(),
        ),
    }
}

fn assert_error(src: &str, code: &str) {
    let (errs, warns) = codes(src);
    assert!(
        errs.contains(&code),
        "expected error {code}, got errors {errs:?} warnings {warns:?}"
    );
}

fn assert_warning(src: &str, code: &str) {
    let (errs, warns) = codes(src);
    assert!(errs.is_empty(), "expected no errors, got {errs:?}");
    assert!(
        warns.contains(&code),
        "expected warning {code}, got {warns:?}"
    );
}

fn assert_clean(src: &str) {
    let (errs, warns) = codes(src);
    assert!(
        errs.is_empty() && warns.is_empty(),
        "expected a clean assembly, got errors {errs:?} warnings {warns:?}"
    );
}

#[test]
fn e1_reads_a_position_with_no_live_value() {
    assert_error(
        "
.func main(0) -> 0
    a0  add b0, b1
    f   retn
",
        "E1",
    );
}

#[test]
fn e1_conform_names_a_dead_position() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 1

    f   conform b0, b5

    f   retn
",
        "E1",
    );
}

#[test]
fn e2_edge_does_not_deliver_the_entry_arity() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 1
    f   br target

.ebb target(3)
    f   retn
",
        "E2",
    );
}

#[test]
fn e2_fall_through_is_an_edge_too() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 1

.ebb next(2)
    f   retn
",
        "E2",
    );
}

#[test]
fn e3_op_in_the_wrong_slot() {
    assert_error(
        "
.func main(0) -> 0
    a0  halt
    f   retn
",
        "E3",
    );
}

#[test]
fn e4_reads_a_result_that_is_still_in_flight() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 0x1000

    m   load b0, 0, 8, zero, 4

    a0  add b1, b1
    f   retn
",
        "E4",
    );
}

#[test]
fn e5_conform_past_six_positions() {
    assert_error(
        "
.func main(0) -> 0
    f   conform b0, b1, b2, b3, b4, b5, b6
",
        "E5",
    );
}

#[test]
fn e6_call_arity_disagrees_with_the_declaration() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 1

    f   call callee, b0

.func callee(0) -> 0
    f   retn
",
        "E6",
    );
}

#[test]
fn e6_retn_count_disagrees_with_the_declaration() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 1

    f   retn b0
",
        "E6",
    );
}

#[test]
fn e7_scratchpad_slot_out_of_range() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 1

    m   spill s99, b0
    f   retn
",
        "E7",
    );
}

#[test]
fn e8_store_lands_inside_a_deferred_load_window() {
    assert_warning(
        "
.func main(0) -> 0
    a0  con 0x1000
    a1  con 99

    m   load b1, 0, 8, zero, 5

    m   store b1, 4, 8, b0

    a0  nop

    a0  nop

    a0  nop

    f   retn
",
        "E8",
    );
}

#[test]
fn e8_is_quiet_when_the_ranges_do_not_overlap() {
    assert_clean(
        "
.func main(0) -> 0
    a0  con 0x1000
    a1  con 99

    m   load b1, 0, 8, zero, 5

    m   store b1, 64, 8, b0

    a0  nop

    a0  nop

    a0  nop

    f   retn
",
    );
}

#[test]
fn e9_operation_in_flight_at_a_control_transfer() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 0x1000

    m   load b0, 0, 8, zero, 5
    f   br target

.ebb target(1)
    f   retn
",
        "E9",
    );
}

#[test]
fn e9_warns_on_retn_with_operations_in_flight() {
    assert_warning(
        "
.func main(0) -> 0
    a0  con 0x1000

    m   load b0, 0, 8, zero, 5
    f   retn
",
        "E9",
    );
}

#[test]
fn load_delay_below_three_is_rejected() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 0x1000

    m   load b0, 0, 8, zero, 2
    f   retn
",
        "E0",
    );
}

#[test]
fn function_arity_above_three_is_rejected() {
    assert_error(
        "
.func main(0) -> 0
    f   retn

.func wide(4) -> 0
    f   retn
",
        "E6",
    );
}

#[test]
fn calli_result_count_above_three_is_rejected() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 0

    f   calli b0 -> 4

    f   sys 0
",
        "E6",
    );
}

/// E2 cannot see an indirect edge, but E9 does not depend on the target.
#[test]
fn in_flight_ops_still_caught_at_an_indirect_branch() {
    assert_error(
        "
.func main(0) -> 0
    a0  con &tgt

    a0  mul b0, b0
    a1  con 0

    f   bri b1

.ebb tgt(0)
    f   halt
",
        "E9",
    );
}

#[test]
fn a_bundle_may_not_reuse_a_slot() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 1
    a0  con 2
    f   retn
",
        "E0",
    );
}
