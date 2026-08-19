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

// ---------------------------------------------------------------------------
// Named belt values — notation over the same positions, resolved by the belt
// model in `check.rs`.

/// Every error message, joined; for the checks that are about the wording.
fn message(src: &str) -> String {
    match asm::assemble(src, &Config::default()) {
        Ok(_) => panic!("expected an error"),
        Err(e) => e
            .diags
            .iter()
            .map(|d| d.msg.clone())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn image(src: &str) -> Vec<u8> {
    asm::assemble(src, &Config::default())
        .unwrap_or_else(|e| {
            let m: Vec<String> = e.diags.iter().map(|d| d.render("<test>")).collect();
            panic!("{}", m.join("\n"))
        })
        .image
        .to_bytes()
}

/// The whole point: names are notation. The same program written both ways
/// has to encode to the same bytes.
#[test]
fn names_encode_exactly_like_the_positions_they_stand_for() {
    let positional = "
.func main(0) -> 0
    a0  con 20

    f   call fib, b0

    f   retn

.func fib(1) -> 1
    a0  con 2

    a0  lt  b1, b0

    f   conform b0, b2

    f   brt b0, fib_base

.ebb fib_rec(2)
    a0  con 1

    a0  sub b2, b0
    a1  con 2

    a0  sub b4, b0

    f   call fib, b2

    f   call fib, b1

    a0  add b0, b1

    f   retn b0

.ebb fib_base(2)
    f   retn b1
";
    let named = "
.func main(0) -> 0
    a0  con 20 -> n

    f   call fib, b0 -> r          ; names mix freely with raw positions

    f   retn

.func fib(n) -> 1
    a0  con 2 -> two

    a0  lt  %n, %two -> cond

    f   conform %cond, %n

    f   brt %cond, fib_base

.ebb fib_rec(cond, n)
    a0  con 1 -> one

    a0  sub %n, %one -> nm1
    a1  con 2 -> two

    a0  sub %n, %two -> nm2

    f   call fib, %nm1 -> r1

    f   call fib, %nm2 -> r2

    a0  add %r2, %r1 -> sum

    f   retn %sum

.ebb fib_base(cond, n)
    f   retn %n
";
    assert_eq!(image(positional), image(named));
}

/// `calli` writes its result count at the call site; a name list counts itself.
#[test]
fn calli_result_names_are_the_result_count() {
    let positional = "
.func main(0) -> 0
    a0  con @two

    f   calli b0 -> 2

    a0  add b1, b0

    f   retn

.func two(0) -> 2
    a0  con 1
    a1  con 2

    f   retn b0, b1
";
    let named = "
.func main(0) -> 0
    a0  con @two -> f

    f   calli %f -> x, y

    a0  add %x, %y

    f   retn

.func two(0) -> 2
    a0  con 1
    a1  con 2

    f   retn b0, b1
";
    assert_eq!(image(positional), image(named));
}

#[test]
fn a_name_that_was_never_dropped_is_not_in_scope() {
    let m = message(
        "
.func main(0) -> 0
    a0  con 1 -> x

    a0  add %x, %nope

    f   retn
",
    );
    assert!(m.contains("no value named `%nope`"), "{m}");
    assert!(m.contains("live here: %x"), "{m}");
}

/// The name outlives the value, and the error says by how much.
#[test]
fn a_name_that_fell_off_the_belt_says_when() {
    let m = message(
        "
.func main(0) -> 0
    a0  con 1 -> x
    a1  con 2 -> y

    f   conform b0

    a0  add %x, b0

    f   retn
",
    );
    assert!(m.contains("`%x` fell off the belt 1 bundle(s) ago"), "{m}");
}

/// Rule 2 of "the three things that will bite you", now with a name on it.
#[test]
fn a_name_dropped_by_this_bundle_is_not_readable_in_it() {
    let m = message(
        "
.func main(0) -> 0
    a0  con 1 -> x
    a1  add %x, %x

    f   retn
",
    );
    assert!(m.contains("dropped by this same bundle"), "{m}");
}

/// A reshape does read the belt after this bundle's drops, so there the same
/// name resolves.
#[test]
fn a_reshape_sees_this_bundle_s_names() {
    assert_clean(
        "
.func main(0) -> 0
    a0  con 1 -> x
    f   conform %x

    f   retn
",
    );
}

#[test]
fn the_number_of_result_names_has_to_match() {
    let m = message(
        "
.func main(0) -> 0
    a0  con 1 -> x, y

    f   retn
",
    );
    assert!(m.contains("drops 1 value(s) but 2 name(s)"), "{m}");
}

/// A loop rewrites the same name every iteration, so the newest one wins.
#[test]
fn a_name_written_twice_shadows_the_older_value() {
    let positional = "
.func main(0) -> 0
    a0  con 1

    a0  con 2

    a0  add b0, b1

    f   retn
";
    let named = "
.func main(0) -> 0
    a0  con 1 -> x

    a0  con 2 -> x

    a0  add %x, b1

    f   retn
";
    assert_eq!(image(positional), image(named));
}

#[test]
fn names_are_scoped_to_their_ebb() {
    assert_error(
        "
.func main(0) -> 0
    a0  con 1 -> x

    f   br next

.ebb next(1)
    a0  add %x, b0

    f   retn
",
        "E1",
    );
}
