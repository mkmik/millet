# PRD review — questions before implementation

Questions raised by a close read of `PRD.md`, grouped by how much they block
implementation. Each question cites the relevant section and, where possible,
proposes a default so answering can be a one-word "accept" or a correction.

## Decisions so far

- **A1 — resolved: post-drop numbering.** Reshape operands/mask are
  interpreted against the belt as it exists after the bundle's §3.1 drops;
  §5.3's "reads at bundle entry" is to be amended for `conform`/`rescue`.
- **A2 — resolved: do what the Mill does** (research in `MILL-NOTES.md`).
  On the Mill, in-flight ops are frame+cycle-tagged and branches cancel
  nothing; ops may retire across an *argless* branch. But at any join that
  needs belt reconciliation (an arg-carrying branch — the analogue of every
  Millet EBB entry, since Millet entries declare an arity), the specializer
  "forces all operations (except loads) to have retired before any branch
  with carried args"; loads cross such edges only via the tag+pickup form,
  which Millet v0 doesn't have. Translated to Millet v0, where **every**
  EBB entry is arity-declaring: **all in-flight ops must have retired
  before any control transfer (taken branch or fall-through into an EBB
  entry).** This becomes assembler check #9; the simulator may assert it
  too. Consequences: §4's "`bk..b15` poison on entry" needs no in-flight
  carve-out, §3.2's static-belt claim holds trivially at joins, and the
  §14 `memcpy` criterion is still satisfiable — the load and its use just
  live in the same iteration, with the loop body ≥ delay+1 bundles.
  Pickup-form loads (Mill's tagged load/`pickup`/`refuse`) are noted as a
  future extension alongside NaR and phasing.
- **A3 — resolved: frame-local latency; discard at `retn`** — both are
  exactly what the Mill does (`MILL-NOTES.md` §2). Mill calls are
  zero-latency in the caller's schedule; in-flights are saved and "replayed
  when control returns to the caller, timed and belted as if the call
  hadn't happened," so callee time never counts against caller delays —
  Millet's load delays count bundles of the issuing frame only. A `retn`
  with the callee's own ops still in flight is legal on the Mill and the
  results are silently discarded ("dead on creation"); Millet does the
  same, plus an assembler warning as a courtesy the Mill doesn't offer.
  (Note A2's check subsumes most cases: ops can only be in flight at
  `retn` if issued in the `retn`'s own EBB.)
- **§12.2 (fused reshape+branch) — stays unfused in v0, direction noted.**
  The research (`MILL-NOTES.md` §1b) shows the modern Mill deleted the
  standalone `conform`: "the functionality was moved to branch ops so a
  taken branch can rearrange the belt to match the destination." But that
  fusion rides on the Mill's variable-width encoding: fused in Millet would
  need 48 bits (branch+rescue) to 56 bits (branch+conform) against a 32-bit
  slot that the branch already fills exactly. v0 keeps the unfused
  reshape-bundle-then-branch form. The designated future shape is a
  **two-slot "long F" op** — the F opcode claims the M slot's 32 bits as an
  extension word — which fuses reshape into branches *and* lifts the
  `conform` 6-position cap (§12.4) with one mechanism, at the cost of the
  memory op in those bundles, while keeping fixed-width decode trivial.
- **A7 / §8.4 — resolved: no poison in v0.** Proper poison needs operand
  metadata (NaR), which is explicitly out of scope. Uninitialized belt
  positions and scratchpad slots simply read as **zero** in the simulator.
  §8.4's out-of-band taint tracking is dropped; all of A7's propagation
  questions are moot until the metadata extension.

---

## A. Blocking semantic questions

These affect the core execution model. The simulator and the assembler's belt
model cannot be written without answers.

### A1. Reshape operand numbering: entry-state or post-drop? (§5.3 vs §7.2)

§5.3 says every op in a bundle, including `conform`/`rescue` in slot F, "reads
the belt state as it exists at bundle entry." §7.2 says the reshape is applied
after the bundle's own drops, and that a value computed in the same bundle
"can be rescued — it just has to be named by the belt position it will occupy."

These two statements conflict. If a same-bundle result can be named by the
position it *will* occupy after the drops, then the reshape's operands are
interpreted against the **post-drop** belt, not the entry belt. If instead the
operands are read against the **entry** belt, a same-bundle result has no
entry-state name and cannot be rescued.

Which is it?

- **(a) Post-drop numbering:** reshape operands/mask are interpreted against
  the belt as it exists after this bundle's §3.1 drops. Same-bundle results
  are reachable. §5.3's "reads at bundle entry" then does not apply to
  reshapes and should be amended.
- **(b) Entry numbering:** reshape operands refer to the entry belt; the
  rewrite is applied after drops but can only select values live at entry.
  Same-bundle results cannot be rescued, and §7.2's example is wrong.

Proposed default: **(a)**, since §7.2 explicitly wants same-bundle results
rescuable and it's the more useful semantics on back-edges. Either answer
needs the PRD text reconciled, and this is the case §7.2 itself flags as
needing a dedicated test.

### A2. In-flight operations across control-flow edges

§3.2 claims belt state is a static property at every instruction boundary,
and §4 has the assembler verify every predecessor edge delivers exactly `k`
values. But nothing says what happens to **in-flight results** (deferred
loads, muls, divs, fills) that have been issued but not yet retired when a
branch is taken:

1. Does an in-flight op still retire, dropping onto the belt, in bundles of
   the *successor* EBB? If yes, the successor's belt contents beyond the
   declared arity are not poison — contradicting §4 ("`bk..b15` are poison on
   entry").
2. If two predecessors of an EBB carry *different* in-flight sets (e.g. one
   has a load retiring 2 bundles after the edge, the other doesn't), the
   successor's belt evolution differs by predecessor — breaking §3.2's
   static-belt guarantee outright.

Proposed default: **in-flight results are cancelled/discarded at any taken or
fall-through EBB transition** — a branch is a scheduling barrier; the
assembler rejects (check class 4-adjacent) any op whose retire time falls
beyond its EBB's exits… except that this makes deferred loads useless across
back-edges, which the `memcpy` done-criterion (§14, "loads deferred ≥ 4
bundles") may or may not require. Alternative: in-flight ops survive the
edge, and the assembler verifies that *all* predecessors of an EBB present an
**identical in-flight schedule** (same retire offsets, same slot ordering) as
part of the arity check. The second option is closer to the real Mill and
keeps `memcpy` with deferred loads spanning the loop back-edge writable.
Please pick — this is the biggest hole in the spec and it directly shapes
assembler check #2 and #4.

### A3. In-flight operations across `call` and `retn` (§6)

The callee runs synchronously and the caller's frame is suspended:

1. Does a caller's in-flight deferred load count callee bundles against its
   delay, or only bundles executed in the caller's own frame? (Presumably the
   latter — delays are frame-local — but §8.2 says "the result retires
   exactly that many bundles later" without qualifying whose bundles.)
2. If a load's remaining delay expires "during" the call bundle, §3.1 already
   orders it against the call's results (bundle index, slot order) — confirm
   the call's returned results sort as slot F of the calling bundle.
3. What happens to the *callee's* own in-flight ops at `retn` — silently
   discarded, or is it an assembler error to return with ops in flight?

Proposed defaults: delays are frame-local (callee time is invisible to the
caller); call results sort as slot F of the call bundle; returning with
in-flight ops is legal and they are discarded (with an assembler warning).

### A4. Who poisons `bk..b15` at EBB entry, and is fall-through an edge?

§4 says positions beyond the entry arity "are poison on entry." Is that:

- **(a)** an architectural action — taking any edge into an EBB actively
  truncates the belt to `k` values, machine-enforced; or
- **(b)** purely an assembler-model convention — the machine's belt is
  untouched by branches, and the assembler simply refuses code that reads
  above `b(k-1)`?

(a) makes traces cleaner and bugs deterministic; (b) is closer to "the
hardware does nothing." Proposed default: **(a)**, since the simulator tracks
poison out-of-band anyway (§8.4) and deterministic failure is a stated goal.

Related: can execution **fall through** from the last bundle of one EBB into
the entry of the next EBB in program order, and if so does that count as a
predecessor edge subject to the same arity check? (§7.1 mentions "the
branch's own fall-through path", implying yes — please confirm, and confirm
that a `brt`/`brf` bundle therefore has *two* successor edges both needing
`k` delivered values.)

### A5. `rescue` mask semantics (§7.1, §10)

`rescue mask16` needs a precise definition:

1. Bit-to-position mapping: does bit *i* select `b_i`? (Assume yes.)
2. "Preserving relative order, compacted into `b0..b(k-1)`": if bits 3 and 7
   are set, which value lands at `b0` — the one from `b3` (younger) or `b7`
   (older)? I.e. does "relative order" mean belt-age order is preserved so
   that the youngest selected value ends up at `b0`?

Proposed default: bit *i* ↔ `b_i`; selected values keep their relative age,
youngest selected → `b0`. Same question applies to `conform`'s listed order
(§6.2's `call` says "last-listed argument is `b0`" — does `conform b_a, b_b`
likewise put `b_b` at `b0`, or is it first-listed → `b0`? §7.1 says "the
listed positions become `b0..b5` in the order given", which reads as
first-listed → `b0` — note that's the *opposite* convention from `call`/
`retn` argument dropping. Intentional?)

Also: how does an encoded `conform` distinguish "reorder 3 values" from
"reorder 6"? The §10 encoding has six 4-bit fields and no count field —
is there a reserved "unused" belt-ref value (e.g. a count in the unused
bits of… there are none) or does the op carry an implicit count via a
different opcode per arity? Needs an encoding answer.

### A6. Scratchpad hazard rules (§8.3)

Store→load ordering is specified (§8.2) but spill→fill is not. Does `fill`
sample the scratchpad at issue time (mirroring loads), observing only spills
from strictly earlier bundles? Can a `spill` and a `fill` of the same slot
share a bundle, and what does the fill see? Proposed default: identical rule
to memory — fill samples at issue, sees spills from strictly earlier bundles
only. (Note `spill` and `fill` share slot M with `load`/`store`, so only one
per bundle anyway — worth stating that scratchpad ops and memory ops compete
for the same slot, since that's real scheduling pressure.)

### A7. Poison propagation rules (§8.4)

"Hard error at `sys`/`store` of a poison-derived value" implies out-of-band
taint tracking. What are the propagation rules?

1. ALU op with one poison input → result poison? (Assume yes.)
2. `pick`: if the *not-selected* input is poison, is the result clean?
   (Assume yes — otherwise `pick` is useless at merge points.) If the
   *condition* is poison?
3. `load` from a poison-derived address → fault, warning, or poison result?
4. `store` *to* a poison-derived address vs storing a poison *value* — §8.4
   mentions the value; is a poison address also a hard error?
5. Branch on a poison condition — warning or hard error?
6. Does poison survive a store/load round-trip through memory? (Proposed: no
   — memory holds bits, taint tracking is belt/scratchpad-only. Simpler and
   loses little.)

### A8. Comparison, branch truth, and arithmetic edge cases

1. What does a compare drop — 0/1 in a 64-bit value? Which compares exist
   (eq, ne, lt/le signed and unsigned)?
2. `brt`/`brf` truth test — any nonzero value is true, or bit 0?
3. `pick` condition — same truth rule as `brt`?
4. Signed vs unsigned variants: div/rem, shifts (logical vs arithmetic
   right), mul (low 64 only, or a mulh op?).
5. `div`/`rem` by zero — fault (halt with diagnostic) or defined result?
   Overflow on `add`/`sub`/`INT_MIN / -1` — wrap silently? Proposed:
   wrap-around for add/sub/mul/shifts, fault on div-by-zero and
   `INT_MIN / -1`.

---

## B. Design decisions needed before coding

### B1. Concrete assembly syntax for bundles and slots

§9.2's example shows one op per line with no slot or bundle markers — but the
whole architecture is "the programmer places every op in a specific slot in a
specific bundle" (§9.2). The text syntax must express that. Options:

- **(a)** explicit slot tags + blank-line or `;;`-terminated bundles:

  ```
      a0  add   b0, b3
      a1  con   1
      m   load  b2, 0, 8, zero, 4
      f   brt   b1, loop
  ```

- **(b)** positional: exactly four ops per bundle line, `|`-separated, `nop`
  written explicitly:

  ```
      add b0, b3 | con 1 | load b2, 0, 8, zero, 4 | brt b1, loop
  ```

- **(c)** implicit greedy packing: consecutive ops pack into the current
  bundle until a slot conflict forces a new one.

(c) contradicts "the programmer places every op" — an op's bundle would
depend on its neighbors — so I'd rule it out. Between (a) and (b): (a) is
more diff-friendly and leaves room for the reference trailing belt-state
comment per bundle; (b) makes bundle boundaries visually undeniable.
Preference?

Also small syntax decisions: comment leader (`;` per the examples?),
`.def` constant syntax, named scratchpad slot syntax, and whether `.func`
/`.ebb` bodies are delimited (does the next directive implicitly end them?).

### B2. Full opcode list

§5.2 gives op classes but not the concrete list ("arithmetic, logical,
shift, compare"). Before M1 I'd need to fix the exact opcode table:
`add sub and or xor shl shr sar mul div rem eq ne lt ltu le leu pick con`
plus M/F ops — roughly 25–30 opcodes. Do you want to enumerate it yourself
in the PRD, or should the implementation session draft the table (with
encodings) as an early deliverable for your review before anything depends
on it? Proposed: the latter, as part of M0.

### B3. Branch target semantics and binary image format (§10)

1. `branch target:20` — "EBB index or PC-relative": pick one. An EBB-index
   table mirrors the call-target table and makes check #2 trivial; proposed
   default: **EBB index into a per-image table**.
2. The image format then needs: function table, EBB table, code section,
   initial-memory/data section, and an entry-point designation. None of this
   is specified. Is a minimal custom binary format with a small header
   acceptable (magic, section table), or do you have a preference (e.g.
   flat code-at-0 plus side tables in a JSON/TOML sidecar for v0)?
3. How does the simulator start — a conventional `.func main(0)`? What
   determines the process exit code: `sys 0` only, or also `retn` from
   main / `halt`?
4. How does a test program get initial data into memory — a `.data`
   directive with a load address? (Needed by M2's "sum an array".)

### B4. `movi` contradicts the no-inference boundary (§8.5 vs §9.2)

`movi imm64` expands to "a constant-pool `load` or a `con`/`shl`/`or`
sequence — assembler's choice". That expansion (i) occupies slots and
bundles the programmer didn't write, (ii) drops intermediate values that
renumber the belt, and (iii) varies by assembler choice — all three break
"the programmer places every op in a specific slot in a specific bundle" and
manual belt tracking. Options:

- **(a)** drop `movi` from v0 entirely; programmers write the sequence by
  hand (in keeping with the project's pain-first philosophy);
- **(b)** keep it but constrain it to a *single-slot* constant-pool `load`
  with a programmer-specified delay, so it's one op in one slot and the
  expansion is deterministic;
- **(c)** keep as specified.

Proposed default: **(a)** for v0, revisit with the symbolic layer.
(If any constant-pool mechanism survives, who lays out the pool and where?)

### B5. `halt` vs `sys 0` (§5.2, §9.5)

Both exist. Is `halt` "abnormal stop, nonzero simulator exit, prints
diagnostics" while `sys 0` is "clean exit with code from `b0`"? Proposed:
yes, keep both with exactly that split; document it.

### B6. `sys` timing and operand convention (§9.5)

`sys` reads fixed positions `b0..b2` (unusual — every other op names its
operands). Confirm that's intended. What is `sys`'s result latency for
codes 1/2 (bytes written/read) — 1, like ALU ops? And is `sys` legal only
in slot F (per §5.2), meaning a bundle can't branch and `sys` together
(fine, just confirming)?

### B7. Encoding details (§10)

1. `load` `delay:4` can encode 0–15 but §8.2 says 3–15: are 0–2 reserved/
   illegal (assembler + disassembler reject)? Proposed: yes.
2. Are `load` `offset:13` and `store` `offset:14` signed? Why the asymmetry
   — is it just field-packing fallout, and is signedness worth aligning
   (e.g. both signed 13-bit)? Proposed: both signed, both 13 bits, store's
   spare bit reserved-zero.
3. "ALU 3-operand" shows two source fields `[b_a:4][b_b:4]` — rename to
   2-operand, or is a third source field intended for some op?
4. Immediate-operand ALU forms (e.g. `shl b0, #3`, `addi`): §10 has none,
   meaning every shift amount and small addend must be `con`'d onto the
   belt first, costing a slot, a bundle, and a belt position. That's a
   real ergonomics/ILP decision worth making deliberately. Proposed for
   v0: no immediate forms except `con` (purity over comfort) — confirm.
5. Function arity: `call` caps at 3 args and `retn` at 3 results by
   encoding. May a `.func` declare arity > 3 (unreachable via `call`) —
   assembler error? Proposed: error. EBB arity may be 0–16? And may an
   EBB declare arity 16, leaving nothing poison? Proposed: yes.

### B8. What exactly does §3.2's "static belt" claim cover?

At a join, the belt's *occupancy and schedule* are static but the *values*
are data-dependent. Suggest rewording §3.2 to claim liveness/positions only
— or confirm you meant something stronger, which then interacts with A2.

---

## C. Toolchain and repo mechanics

Lower stakes — defaults proposed, will proceed with these unless corrected.

1. **Workspace layout:** one cargo workspace; crates `millet-core` (shared
   ISA definitions + config struct + encoding), `millet-asm` (assembler bin
   `mas`), `millet-sim` (simulator bin `msim`), disassembler as a mode of
   `mas` (`mas -d`) or its own bin `mdis`. File extension `.mil`.
2. **Machine config struct** (§9.1): belt size, slot count/mix, scratch
   size, bundle bytes — compile-time constants generic-free struct passed
   at runtime. Confirm runtime (not const-generic) parameterization is
   acceptable; it's simpler and the perf doesn't matter.
3. **Golden traces:** `--trace-json` output committed under `tests/golden/`,
   compared exactly; a `--bless`-style regeneration flag. "Differential"
   testing in §9.1 — differential against what, given there's one
   simulator? Assume it means assembler-model-vs-simulator belt-state
   cross-checking (the assembler's predicted belt per bundle is emitted and
   diffed against the simulator's actual trace). Confirm.
4. **Error-message contract:** §9.3's eight checks each get a stable error
   code (E1..E8) + a test; M5's "point at the scheduling mistake" deferred
   to M5.
5. **Rust:** stable toolchain, 2024 edition, no unsafe, minimal deps
   (`clap`, maybe `serde_json` for trace output). CI via GitHub Actions
   running `cargo test` + golden-trace diff. Confirm.

---

## Suggested resolution order

A1, A2, A4 gate the belt model shared by assembler and simulator — nothing
meaningful can be built before they're answered. A5–A8 and B1–B3 gate M1–M3
but could be answered during M0. Section C needs no answers unless a default
above is wrong.
