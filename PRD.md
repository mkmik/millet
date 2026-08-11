# Millet — PRD / v0 Specification

**A minimum-viable Mill-like ISA, with an assembler and a simulator.**

Status: first pass. Every number in here is a *decision*, not a suggestion — but all of them are cheap to change before the assembler exists and expensive after. Section 12 lists the ones I'd expect to revisit.

---

## 1. Goal and non-goals

### Goal

Build a complete, executable, hand-writable ISA that preserves the parts of the Mill architecture that make it *interesting*, while cutting everything that makes it *large*. Success is: a person can hand-write a recursive Fibonacci and a byte-copy loop in Millet assembly, run them on the simulator, and in doing so develop real intuition for belt scheduling, EBB control flow, and static latency.

This is a learning vehicle and a foundation for later RTL work — not a product, not a compiler target, not a faithful Mill clone.

### Explicit non-goals for v0

| Cut | Rationale |
| --- | --- |
| Split-stream bidirectional encoding | Orthogonal to execution semantics; pure decode-throughput optimization. Fixed-width bundles instead. |
| ~~Operand metadata (NaR, None, width tags, vectors)~~ | **NaR and None are in** — the designated first extension, added after v0; see §8.6. Width tags and vectors stay out: no Millet op is width-polymorphic. |
| Turfs, portals, grants, PLB | OS-level. No protection model at all in v0. |
| Virtual memory | Flat physical byte-addressed space. |
| Genasm / member specialization / family compatibility | One concrete member, fixed widths, fixed slot count. |
| Phasing (full Mill model) | Replaced by a simplified single-cycle read/write rule, §5.3. |
| Compiler, LLVM backend | Assembler only. |
| Spiller as an architected mechanism | Frames are simulator data structures; unbounded depth. |
| Interrupts, exceptions, traps as recoverable events | Faults halt the simulator with a diagnostic. |

### In scope, deliberately

Belt · EBBs · explicit static scheduling · multiple slots per instruction · call/return with belt frames · scratchpad · deferred loads · variable static latencies · `pick` · `conform`/`rescue`.

---

## 2. Machine parameters (fixed)

| Parameter | Value | Note |
| --- | --- | --- |
| Belt positions | **16** | 4-bit belt references fit an operand field. Deliberately larger than the 8 planned for the FPGA milestone — hand-written code needs the headroom. |
| Slots per instruction | **4** | `A0`, `A1` (ALU), `M` (memory), `F` (flow) |
| Datapath width | **64 bits** | All belt values are 64-bit. No width metadata; every value carries a status tag (§8.6). |
| Scratchpad | **64 slots × 8 bytes** per frame | Statically addressed, frame-local |
| Address space | 2⁶⁴ flat, little-endian | Sparse-backed in the simulator |
| Instruction bundle | **16 bytes**, 4 × 32-bit slots | Fixed width, no variable-length encoding |
| Register file | none | There are no registers. That is the point. |

Issue is strictly **in-order, one bundle per cycle, never stalls.** If an operand isn't ready, the program is wrong — the assembler is responsible for catching it, not the machine.

---

## 3. The belt

The belt is a fixed-size queue of 16 values. `b0` is the most recently dropped value; `b15` is the oldest live one. Every operand field in every instruction is a belt reference `b0..b15` — there is no other way to name a live value.

Operands are consumed non-destructively; reading `b3` does not remove it. Values leave the belt only by falling off the far end as newer values are dropped.

**Falling off is silent.** There is no fault for a value expiring. Reading a belt position that has never been written since frame entry yields a **None** (§8.4, §8.6) — but reading a position holding an unintended stale value is simply a bug the assembler tries to catch statically.

### 3.1 Drop ordering

All results retiring at the end of a given bundle are dropped in a single deterministic order:

> Sort all retiring results by **(issuing bundle index ascending, then slot index ascending: A0, A1, M, F)**. Drop in that order. The last one dropped becomes `b0`.

This rule is the whole contract. It means a load issued three bundles ago retires *before* an ALU result computed in the current bundle, and it means the assembler can compute the exact belt state at every program point.

### 3.2 Belt state is a static property

At every instruction boundary, the belt's *occupancy* — which positions are live, which op produced each, and the full retire schedule — is fully determined by the instruction stream; only the values themselves are data-dependent. The assembler models this exactly, which is what lets it *verify* hand-written belt offsets (§9.3) even though it never *invents* them (§9.2).

---

## 4. Extended basic blocks (EBBs)

An EBB has **one entry point and any number of exits.** All control flow targets are EBB entries; you cannot branch into the middle of an EBB.

Each EBB declares an **entry arity** `k` — the number of live belt values it expects on entry, occupying `b0..b(k-1)`. Taking any edge into an EBB truncates the belt: positions `bk..b15` read as zero afterwards. This truncation is machine-enforced, not merely an assembler convention. Falling through into the next EBB's entry is a legal edge, subject to the same checks — a conditional-branch bundle therefore has two successor edges.

```
.ebb loop_body(3)      ; expects 3 live values at b0,b1,b2
```

The assembler verifies that **every** predecessor edge delivers exactly `k` values. This is the phi-node problem made explicit and checkable — and it's why `conform`/`rescue` exist.

**No in-flight results may cross an edge.** Every operation issued in an EBB must retire before any control transfer out of it, taken or fall-through; the assembler rejects violations (§9.3 check 9). This mirrors the Mill, where the specializer forces all operations except loads to retire before any belt-carrying branch — and every Millet EBB entry is arity-declaring. The Mill's tag-and-pickup load form, which lets loads cross such edges, is a designated future extension (see `MILL-NOTES.md`).

Functions are EBBs with an additional property: they start a new frame (§6).

```
.func fib(1)           ; one argument, arrives at b0
```

---

## 5. Instruction format and execution

### 5.1 Bundle layout

```
byte  0..3   slot A0   (ALU)
byte  4..7   slot A1   (ALU)
byte  8..11  slot M    (memory)
byte 12..15  slot F    (flow)
```

Every slot is 32 bits. Unused slots hold `NOP` (opcode 0x00). Fixed width wastes space; density is explicitly not a v0 concern.

### 5.2 Slot capabilities

| Slot | Ops |
| --- | --- |
| A0, A1 | arithmetic, logical, shift, compare, `pick`, `con`, `none`, `isnar`, `isnone` |
| M | `load`, `store`, `spill`, `fill` |
| F | `br`, `brt`, `brf`, `call`, `retn`, `conform`, `rescue`, `sys`, `halt` |

Two ALU slots means genuine ILP scheduling pressure without combinatorial explosion in the assembler.

### 5.3 Intra-bundle semantics (simplified phasing)

> **All operand reads in a bundle observe the belt state as it exists at bundle entry. All drops occur at bundle end, in the order of §3.1.**

Consequences worth internalizing:

- Two ops in the same bundle cannot communicate. `A1` cannot read `A0`'s result.
- Exception: a `conform` or `rescue` in slot `F` interprets its operands against the **post-drop** belt — the state after this bundle's §3.1 drops — and its rewrite is applied last (§7.2).
- Only one branch may be present per bundle (slot `F` is singular), which sidesteps Mill's exit-priority rules entirely.

### 5.4 Latency

Every operation has a **fixed, architecturally specified latency** measured in bundles. Latency 1 means "retires at the end of the issuing bundle."

| Op class | Latency |
| --- | --- |
| add, sub, logical, shift, compare, `pick`, `con`, `none`, `isnar`, `isnone` | 1 |
| mul | 3 |
| div, rem | 8 |
| load | **programmer-specified**, 3–15 (§8.2) |
| store, branches, `conform`, `rescue` | n/a (no result) |
| `sys` (codes with a result) | 1 |
| call | see §6.2 |

Latencies count bundles of the issuing frame only; a callee's execution consumes none of them (§6.2).

The machine does not check readiness. The assembler does, and refuses to emit code that reads a belt position that a not-yet-retired op will occupy.

### 5.5 Arithmetic semantics

Compares (`eq ne lt le ltu leu`; signed unless `u`-suffixed) drop 64-bit `0` or `1`. `brt`/`brf` and `pick` treat any nonzero value as true. Shifts are `shl`, `shr` (logical right), `sar` (arithmetic right). `mul` drops the low 64 bits. `add`/`sub`/`mul` and shifts wrap silently. `div`/`rem` come in signed and unsigned forms; division by zero and signed `INT_MIN / -1` are faults. The full opcode table (mnemonics + encodings) is an M0 deliverable, reviewed before anything depends on it.

---

## 6. Calls, frames, and returns

### 6.1 Frames

A call creates a new frame containing:

- a **fresh belt** — 16 positions, all zero except the arguments
- a **fresh scratchpad** — 64 slots, all zero
- a return address (not architecturally visible; no way to read or write it)

The caller's belt and scratchpad are entirely inaccessible to the callee. This is the single most important structural property of the Mill call model and the reason the belt is workable at all.

### 6.2 `call`

```
call target, b_a, b_b, ...          ; 0..3 arguments
```

Arguments are dropped onto the callee's fresh belt **in listed order**, so the last-listed argument is `b0` in the callee. The callee's declared arity must match the argument count.

From the caller's perspective a call is a single operation with **unbounded latency**: results retire at the end of the calling bundle, and no op issued in the same bundle observes them (§5.3). The simulator executes the callee to completion synchronously.

Callee execution is invisible to caller timing: an in-flight caller operation (e.g. a deferred load issued before the call) does not count callee bundles against its latency — delays are frame-local. This is exactly the Mill's rule, where calls take zero cycles in the caller's schedule and in-flights retire "as if the call hadn't happened" (`MILL-NOTES.md` §2).

Argument count is capped at 3 by encoding (§10); declaring a `.func` arity above 3 is an assembler error. Beyond that, pass a pointer to a memory block.

### 6.3 `retn`

```
retn b_x, b_y, ...                  ; 0..3 results
```

Destroys the current frame; results are dropped onto the *caller's* belt in listed order, subject to §3.1 ordering relative to anything else retiring in the caller's call bundle.

Returning while the frame's own operations are still in flight is legal; their results are silently discarded — the Mill's "dead on creation" rule. The assembler warns.

The number of results a function returns is declared and checked:

```
.func divmod(2) -> 2
```

Recursion depth is bounded only by host memory. Tail calls are not special-cased in v0.

---

## 7. Belt reshaping: `conform` and `rescue`

These exist because EBBs have declared entry arities and predecessors rarely have their live values sitting in exactly `b0..b(k-1)`.

### 7.1 The two ops

```
rescue  mask16
```
Retains the belt positions selected by a 16-bit mask (bit *i* selects `b_i`), **preserving their relative age**, compacted into `b0..b(k-1)` — the youngest selected value lands at `b0`. Everything else becomes zero. Cheap to encode, and it's the natural loop-back operation where live values are already in the right relative order.

```
conform b_a, b_b, b_c, b_d, b_e, b_f
```
Explicitly reorders: the listed positions become `b0..b5` in the order given — first-listed → `b0`, which is deliberately the *opposite* convention from `call`/`retn`, where the last-listed argument becomes `b0`. Everything else becomes zero. **Capped at 6 positions** by the encoding — a real constraint; arity is carried by six distinct opcodes `conform1..conform6` (§10). If you need to reorder more than six live values across an edge, use the scratchpad.

`conform` is the general operation; `rescue` is the compact special case. Both live in slot `F`, which means **a bundle containing a reshape cannot also contain a branch.** Reshapes therefore occupy the bundle immediately preceding a branch, or the branch's own fall-through path.

> *Direction settled, §12:* the Mill itself later fused `conform` into its branch ops — but on a variable-width encoding. A fused form needs 48–56 bits against Millet's 32-bit slot, so v0 stays unfused; the future shape is a two-slot "long F" op (see §12 item 2 and `MILL-NOTES.md` §1b).

### 7.2 Ordering

A reshape's operands — `conform`'s list, `rescue`'s mask — are interpreted against the **post-drop** belt: the state after this bundle's own §3.1 drops. The rewrite is applied last. A value computed in the same bundle can therefore be rescued; it is named by the position it occupies after the drops, which the assembler knows exactly.

This is subtle and is the highest-risk semantic rule in the document. It needs a dedicated test.

---

## 8. Memory, scratchpad, constants

### 8.1 Memory model

Flat, byte-addressed, little-endian, 2⁶⁴ bytes, sparsely backed by the simulator. No alignment requirement (unaligned access is legal and works). No caches modelled in v0 — load latency is whatever the programmer declared, always.

Loading from an unbacked page is a **fault**: simulator halts with a diagnostic. There is no page table and no fault handler.

### 8.2 Loads and stores

```
load  b_addr, offset, width, ext, delay    → 1 result
store b_addr, offset, width, b_value       → no result
```

- `width` ∈ {1, 2, 4, 8} bytes
- `ext` ∈ {zero, sign} for widths < 8
- `delay` ∈ 3..15 — **the programmer chooses the latency**, and the result retires exactly that many bundles later, per §3.1. Encoded delay values 0–2 are illegal; assembler and disassembler both reject them.
- `offset` is a signed 13-bit byte offset, for both `load` and `store` (the store encoding's spare bit is reserved-zero)

Deferred loads are the most distinctively Mill idea surviving the cut. The programmer hoists the load far above its use and states how far. The simulator honours the declared delay exactly (no early completion, no stall) — which makes mis-scheduled code fail deterministically rather than accidentally working.

Store-to-load ordering within the delay window: **stores take effect at their issuing bundle's end; a load observes all stores issued in strictly earlier bundles.** A load whose delay window spans a store to the same address returns the *pre-store* value. This is a real hazard, it is the programmer's problem, and the assembler warns when it can prove overlap.

### 8.3 Scratchpad

Frame-local, 64 slots of 8 bytes, statically addressed.

```
spill sN, b_x        ; no result, latency n/a
fill  sN             ; → 1 result, latency 3
```

`N` is a literal 0..63; there is no computed scratchpad addressing. This is where long-lived locals go when 16 belt positions aren't enough — which, with two ALU slots retiring per cycle, is constantly.

Spill-to-fill ordering mirrors §8.2 exactly: a spill takes effect at its issuing bundle's end; a fill samples the scratchpad at issue and observes spills from strictly earlier bundles only. Note that `load`/`store`/`spill`/`fill` all compete for the single M slot — real scheduling pressure.

### 8.4 Uninitialized values

An uninitialized belt position or scratchpad slot reads as a **None** (§8.6) — a value that is not there, rather than a zero pretending to be one. Reading one is still a bug the assembler tries to catch statically via the liveness and arity checks (§9.3); the machine does not fault at the read, but the None propagates and the first store, branch or `sys` that meets it says so.

*(v0 read these as zero. That was always a stand-in for metadata, and it is the one behaviour the metadata extension changed rather than added to — §12.8.)*

### 8.5 Constants

```
con imm24            ; sign-extended to 64 bits → 1 result
```

24-bit signed immediate. There is no wider-constant pseudo-op in v0: a multi-op expansion would occupy slots and drop belt values the programmer didn't write, crossing the no-inference line of §9.2. Wider constants are built by hand from `con`/`shl`/`or`, or loaded from a constant the programmer lays out in a `.data` section (§10).

### 8.6 Operand metadata

*Added after v0; this is the extension §11 designated.*

Every belt and scratchpad value carries a tag alongside its 64 bits: a plain value, a **None**, or a **NaR**. This is what makes speculation safe, and speculation is the Mill's headline result.

- A **NaR** ("not a result") is what a failed operation drops instead of stopping the machine: a `load` whose address is unbacked, a `div`/`rem` by zero or signed `INT_MIN / -1`. Its 64 data bits are its payload — what failed and the bundle it failed in — so a diagnostic can name the origin however far the value propagated.
- A **None** is a value that is not there: `none` drops one, and so does every position nothing has reached (§8.4).

**Propagation.** An operation with any poisoned operand computes nothing and drops poison instead: a None if any operand is a None, otherwise that NaR, payload intact. **None wins over NaR** — a None means the operation was never meant to happen, so it must not report a fault. This is the Mill's rule (`MILL-NOTES.md` §3).

**Realization.** Three operations are not speculable and raise what they are handed:

| op | on a None | on a NaR |
| --- | --- | --- |
| `store` (value or address) | suppressed — memory is not touched | fault |
| `brt`/`brf` (condition) | fault | fault |
| `sys` (any operand it reads) | fault | fault |

Everything else is speculable. `pick` is where poison is meant to die: only the selected operand's tag survives, so a value speculated down the path not taken cannot poison the result — that is what lets a load be hoisted above the test that guards it, and it is why `pick` stops being merely a conditional move. A poisoned *condition* propagates rather than faulting.

**The scratchpad carries metadata; memory does not.** `spill`/`fill` move whole operands, so a spilled NaR fills back as the same NaR, and a speculative value can be parked across a call. Memory holds bytes, which is exactly why `store` has to realize: a suppressed store is how a store gets hoisted above its guard, and a NaR that reaches memory has nowhere left to hide. Verified against the Mill: "the scratch and spill preserves metadata, dealing with belt items and not naked bytes … metadata is preserved in the Scratchpad but discarded again on store" (`MILL-NOTES.md` §3).

**Observing a tag.** `isnar b_x` and `isnone b_x` drop 0/1 and never realize; they are the only way to look at a tag without faulting.

**Not carried:** width and scalarity. On the Mill an operation takes its width and vector count from the operand, so those bits are load-bearing; no Millet operation is width-polymorphic — widths live in the `load`/`store` encodings — so carrying them would be carrying bits nothing reads. Vectors remain out of scope entirely.

**What the assembler checks:** nothing new. A tag is a dynamic property — whether a load faulted is not knowable from the instruction stream — so metadata adds no static check. E1/E4 already reject the reads that would produce an accidental None.

---

## 9. Toolchain

### 9.1 Components

| Component | Language | Notes |
| --- | --- | --- |
| Assembler | Rust | text → binary image; raw belt offsets; belt model used for *checking only* |
| Simulator | Rust | bundle-accurate, deterministic |
| Disassembler | Rust | round-trip verified against assembler |
| Test suite | — | golden traces + differential |

One repo, one crate workspace: `millet-core` (ISA definitions, encoding, machine config), `millet-asm` (binary `mas`; the disassembler is `mas -d`), `millet-sim` (binary `msim`). Source files use the `.mil` extension. The simulator's machine parameters (§2) live in a single config struct so the FPGA-oriented belt-8 / 4-slot variant is a parameter change, not a fork.

### 9.2 Raw belt offsets — no symbolic naming in v0

**The assembler accepts literal belt positions only.** Every operand is written as `b0..b15`, exactly as encoded. The programmer tracks belt state manually, including the renumbering caused by every drop.

**Concrete syntax:** one op per line, prefixed by its slot tag (`a0`, `a1`, `m`, `f`); a blank line ends the bundle. Unused slots are omitted and assemble to `NOP`. Comments start with `;`; the reference style is a trailing belt-state comment per bundle.

```
.ebb loop(3)                        ; b0=i  b1=n  b2=p
    m   load    b2, 0, 8, zero, 4   ; retires at +4
    a0  add     b0, b3              ; b3 = the constant 1, dropped earlier
                                    ; after this bundle: b0=sum', b1=i, b2=n, b3=p, ...
```

This is deliberate and it is the point of the exercise. The renumbering *is* the belt; automating it away before understanding it would defeat the purpose of building this at all. Manual tracking will be painful, and the shape of that pain is a primary output of the project (§13, M6) — it's the empirical input to designing the layer that eventually removes it.

**No symbolic naming, no SSA-style values, no `%name = ...`, no `-> %result` destinations.** Ops name their inputs by position and drop their results implicitly per §3.1. There are no destination fields in the architecture and none in the assembly syntax.

The assembler does not compute belt offsets. It **verifies** them: it maintains an exact belt model and reports when a written offset names an expired or not-yet-retired value (§9.3). Verification without inference — that's the whole boundary.

Permitted conveniences (these do not cross the line): symbolic labels for EBBs and functions, named scratchpad slots, `.def` constants for immediates, and comments. The reference assembly style is a trailing comment on each bundle recording the resulting belt state, as above.

**Future: a second assembly layer.** A higher-level assembler with symbolic belt tracking — SSA-style names resolved to per-instruction offsets — is a planned separate tool that emits v0 assembly as its output. It is explicitly out of scope until the manual layer is complete and the pain points are documented. Keeping it as a distinct front end rather than a mode of the same assembler preserves the low-level layer as the ground truth.

**Neither layer is a compiler.** No register allocation, no scheduling, no instruction selection, no reordering. The programmer places every op in a specific slot in a specific bundle. That line is worth defending explicitly, because it will be tempting to cross it — especially once the second layer exists.

### 9.3 Static checks the assembler must perform

1. Belt reference names a value that has retired and not yet expired
2. EBB entry arity matches on every predecessor edge
3. Slot capability (op is legal in its slot)
4. Latency: no read of a position occupied by an in-flight result
5. `conform` ≤ 6 positions; `rescue` mask population ≤ 16
6. Call/return arity against declarations
7. Scratchpad slot in range
8. Warn on provable store/deferred-load overlap
9. No operation still in flight at any control transfer out of an EBB (§4); warn on in-flight ops at `retn` (§6.3)

Each check carries a stable error code (`E1`–`E9`) and a dedicated test. Checks 1 and 4 together are the assembler's whole reason to exist.

### 9.4 Simulator output

- `--trace`: per-bundle belt state, retiring drops, scratchpad deltas
- `--trace-json`: machine-readable, for differential testing
- Cycle count, bundle count, slot occupancy (a crude ILP metric worth watching)

### 9.5 I/O

A single `sys` op, minimal by design:

| code | operation |
| --- | --- |
| 0 | `exit(b0)` |
| 1 | `write(fd=b0, ptr=b1, len=b2)` → bytes written |
| 2 | `read(fd=b0, ptr=b1, len=b2)` → bytes read |

`sys` is the one op that takes its operands from fixed belt positions (`b0..b2`) rather than naming them. It is legal only in slot F; codes 1 and 2 drop their result with latency 1. IO is not speculable: a poisoned operand faults (§8.6).

Enough to write tests that print. Nothing more.

---

## 10. Encoding sketch

32-bit slot, 8-bit opcode, 4-bit belt references.

```
ALU 2-operand:   [op:8][b_a:4][b_b:4][unused:16]
pick:            [op:8][b_c:4][b_t:4][b_f:4][unused:12]
con:             [op:8][imm:24]
load:            [op:8][b_addr:4][delay:4][w:2][ext:1][offset:13]   ; offset signed
store:           [op:8][b_addr:4][b_val:4][w:2][offset:13][z:1]     ; offset signed, z reserved-zero
spill/fill:      [op:8][slot:6][b_val:4][unused:14]
branch:          [op:8][b_cond:4][target:20]      ; EBB-table index
call:            [op:8][nargs:2][b_a:4][b_b:4][b_c:4][target:10]
retn:            [op:8][nres:2][b_x:4][b_y:4][b_z:4][unused:10]
conform1..6:     [op:8][b0:4][b1:4][b2:4][b3:4][b4:4][b5:4]         ; arity in opcode; unused refs zero
rescue:          [op:8][mask:16][unused:8]
sys:             [op:8][code:8][unused:16]
```

`call` target at 10 bits implies a function table rather than a direct offset — a reasonable v0 simplification (indirect calls come free later via a table index on the belt). Fields marked unused must be zero; the disassembler checks.

**Binary image format:** minimal and custom — a small header (magic, version) plus sections: code, EBB table, function table, and `.data` blobs each with a load address. Execution starts at `.func main(0) -> 0`; `sys 0` sets the process exit code, and `retn` from `main` exits 0. Initial memory contents come solely from `.data` sections.

---

## 11. What we lose, and why it's acceptable

*v0 dropped metadata, and with it NaR, None and **speculation** — the architecture's headline result. It was the designated first extension, and §8.6 is it: loads fault silently, `pick` stops being a conditional move, and `examples/speculate.mil` dereferences a null pointer without a branch in sight. The prediction that it would double the semantic surface was wrong; it cost three opcodes, a tag on the value type, and three ops that had to stop being speculable.*

What is still out:

- **Vectors and width metadata.** On the Mill an operation reads its width and element count from the operand; here widths are in the encoding, so the bits would be carried and never read (§8.6).
- **Full phasing**, which is what makes single-bundle loops possible. Now the second designated extension is the first.
- **Pickup-form loads** (`MILL-NOTES.md` §1a), which would let a load cross an arg-carrying edge. §12.9.

Separately, and on the tooling rather than the architecture side: the symbolic assembly layer described in §9.2, gated on having written enough raw-offset code to know what it should actually do.

---

## 12. Decisions I expect to revisit

1. **Belt 16 vs 8.** 16 for writability; 8 is the FPGA target. Parameterize from day one.
2. **Fused reshape+branch.** Would remove a bundle from every back-edge. The Mill itself ended up fusing (`MILL-NOTES.md` §1b), but on variable-width encoding; a fused form needs 48–56 bits against our 32-bit slot. The designated future shape is a two-slot "long F" op — the F opcode claims the M slot's word as an extension — which also lifts the `conform` cap (item 4). §7.1.
3. **Reshape ordering relative to same-bundle drops.** Decided: post-drop numbering (§7.2). Still the subtlest rule here; revisit after real code exists.
4. **`conform` capped at 6.** Falls out of a 32-bit slot; a 2-slot `conform` would lift it.
5. **Slot mix.** 2×ALU + M + F is a guess. Real code may want 2 memory slots, or a dedicated `con` slot.
6. **Fixed 16-byte bundles.** Wasteful; a slot-mask header is the obvious density fix if bundle fetch ever matters.
7. **Call as unbounded latency.** Fine for a simulator, meaningless for RTL. Revisit before any hardware work.
8. ~~**Uninitialized-reads-as-zero.**~~ *Settled: it was a stand-in for NaR, and it disappeared when metadata arrived. Uninitialized reads are Nones (§8.4, §8.6).*
9. **In-flight ops barred from crossing edges.** The strictest reading of the Mill's join rule (§4). The Mill's tag-and-pickup load form would relax it for loads; add it if deferred loads across back-edges turn out to matter in practice.

---

## 13. Milestones

**M0 — skeleton.** Repo, workspace, machine config struct, binary image format, disassembler stub.

**M1 — straight-line.** ALU ops, `con`, belt model in both assembler and simulator, `sys exit`/`write`. Test: compute and print a constant expression.

**M2 — memory + scratchpad.** Loads with declared delay, stores, spill/fill. Test: sum an array with a fully unrolled body (no branches yet).

**M3 — control flow.** EBBs, branches, `conform`/`rescue`, arity checking. Test: loops — array sum, `strlen`, `memcpy`.

**M4 — calls.** Frames, `call`/`retn`, recursion. Test: recursive `fib`, `ackermann(2,3)`, a two-result `divmod`.

**M5 — usability.** Trace viewer, belt-state annotation in disassembler output (so the machine can print the comments you'd otherwise write by hand), error messages that point at the *scheduling* mistake rather than the symptom, hand-written benchmark set with slot-occupancy reporting. Still no symbolic naming.

**M6 — retrospective.** Write down what hurt. That document is the input to the metadata/NaR extension and to the FPGA subset decision.

---

## 14. Definition of done for v0

- `fib(20)` runs recursively and prints the right answer
- A `memcpy` loop runs with loads deferred ≥ 4 bundles and no belt spills
- Assembler rejects every one of the eight error classes in §9.3, with a test per class
- Disassembler round-trips the full test corpus byte-identically
- Trace output is legible enough to debug a belt-scheduling error without reading simulator source