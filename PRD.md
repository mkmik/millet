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
| Operand metadata (NaR, None, width tags, vectors) | Big semantic surface. Costs us speculation (see §11), accepted. First thing to add back. |
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
| Datapath width | **64 bits** | All belt values are 64-bit. No widths, no metadata. |
| Scratchpad | **64 slots × 8 bytes** per frame | Statically addressed, frame-local |
| Address space | 2⁶⁴ flat, little-endian | Sparse-backed in the simulator |
| Instruction bundle | **16 bytes**, 4 × 32-bit slots | Fixed width, no variable-length encoding |
| Register file | none | There are no registers. That is the point. |

Issue is strictly **in-order, one bundle per cycle, never stalls.** If an operand isn't ready, the program is wrong — the assembler is responsible for catching it, not the machine.

---

## 3. The belt

The belt is a fixed-size queue of 16 values. `b0` is the most recently dropped value; `b15` is the oldest live one. Every operand field in every instruction is a belt reference `b0..b15` — there is no other way to name a live value.

Operands are consumed non-destructively; reading `b3` does not remove it. Values leave the belt only by falling off the far end as newer values are dropped.

**Falling off is silent.** There is no fault for a value expiring. Reading a belt position that has never been written since frame entry yields a defined poison value (§8.4) — but reading a position holding an unintended stale value is simply a bug the assembler tries to catch statically.

### 3.1 Drop ordering

All results retiring at the end of a given bundle are dropped in a single deterministic order:

> Sort all retiring results by **(issuing bundle index ascending, then slot index ascending: A0, A1, M, F)**. Drop in that order. The last one dropped becomes `b0`.

This rule is the whole contract. It means a load issued three bundles ago retires *before* an ALU result computed in the current bundle, and it means the assembler can compute the exact belt state at every program point.

### 3.2 Belt state is a static property

At every instruction boundary, the belt's contents are fully determined by the instruction stream — no data-dependent variation. The assembler models the belt exactly, which is what lets it *verify* hand-written belt offsets (§9.3) even though it never *invents* them (§9.2).

---

## 4. Extended basic blocks (EBBs)

An EBB has **one entry point and any number of exits.** All control flow targets are EBB entries; you cannot branch into the middle of an EBB.

Each EBB declares an **entry arity** `k` — the number of live belt values it expects on entry, occupying `b0..b(k-1)`. Belt positions `bk..b15` are poison on entry.

```
.ebb loop_body(3)      ; expects 3 live values at b0,b1,b2
```

The assembler verifies that **every** predecessor edge delivers exactly `k` values. This is the phi-node problem made explicit and checkable — and it's why `conform`/`rescue` exist.

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
| A0, A1 | arithmetic, logical, shift, compare, `pick`, `con` |
| M | `load`, `store`, `spill`, `fill` |
| F | `br`, `brt`, `brf`, `call`, `retn`, `conform`, `rescue`, `sys`, `halt` |

Two ALU slots means genuine ILP scheduling pressure without combinatorial explosion in the assembler.

### 5.3 Intra-bundle semantics (simplified phasing)

> **All operand reads in a bundle observe the belt state as it exists at bundle entry. All drops occur at bundle end, in the order of §3.1.**

Consequences worth internalizing:

- Two ops in the same bundle cannot communicate. `A1` cannot read `A0`'s result.
- A `conform` or `rescue` in slot `F` reads the *entry* belt state, and its rewrite is applied **after** this bundle's own drops (§7.2).
- Only one branch may be present per bundle (slot `F` is singular), which sidesteps Mill's exit-priority rules entirely.

### 5.4 Latency

Every operation has a **fixed, architecturally specified latency** measured in bundles. Latency 1 means "retires at the end of the issuing bundle."

| Op class | Latency |
| --- | --- |
| add, sub, logical, shift, compare, `pick`, `con` | 1 |
| mul | 3 |
| div, rem | 8 |
| load | **programmer-specified**, 3–15 (§8.2) |
| store, branches, `conform`, `rescue` | n/a (no result) |
| call | see §6.2 |

The machine does not check readiness. The assembler does, and refuses to emit code that reads a belt position that a not-yet-retired op will occupy.

---

## 6. Calls, frames, and returns

### 6.1 Frames

A call creates a new frame containing:

- a **fresh belt** — 16 positions, all poison except the arguments
- a **fresh scratchpad** — 64 slots, all poison
- a return address (not architecturally visible; no way to read or write it)

The caller's belt and scratchpad are entirely inaccessible to the callee. This is the single most important structural property of the Mill call model and the reason the belt is workable at all.

### 6.2 `call`

```
call target, b_a, b_b, ...          ; 0..3 arguments
```

Arguments are dropped onto the callee's fresh belt **in listed order**, so the last-listed argument is `b0` in the callee. The callee's declared arity must match the argument count.

From the caller's perspective a call is a single operation with **unbounded latency**: results retire at the end of the calling bundle, and no op issued in the same bundle observes them (§5.3). The simulator executes the callee to completion synchronously.

Argument count is capped at 3 by encoding (§10). Beyond that, pass a pointer to a memory block.

### 6.3 `retn`

```
retn b_x, b_y, ...                  ; 0..3 results
```

Destroys the current frame; results are dropped onto the *caller's* belt in listed order, subject to §3.1 ordering relative to anything else retiring in the caller's call bundle.

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
Retains the belt positions selected by a 16-bit mask, **preserving their relative order**, compacted into `b0..b(k-1)`. Everything else becomes poison. Cheap to encode, and it's the natural loop-back operation where live values are already in the right relative order.

```
conform b_a, b_b, b_c, b_d, b_e, b_f
```
Explicitly reorders: the listed positions become `b0..b5` in the order given, everything else poison. **Capped at 6 positions** by the encoding — a real constraint. If you need to reorder more than six live values across an edge, use the scratchpad.

`conform` is the general operation; `rescue` is the compact special case. Both live in slot `F`, which means **a bundle containing a reshape cannot also contain a branch.** Reshapes therefore occupy the bundle immediately preceding a branch, or the branch's own fall-through path.

> *Open question, §12:* whether to fuse reshape into the branch encoding instead. It costs a wider `F` slot but removes a bundle from every loop back-edge. Deferring, because the unfused version is easier to reason about first.

### 7.2 Ordering

A reshape reads the belt as of bundle entry (§5.3), but is applied **after** the bundle's own drops. So a value computed in the same bundle *can* be rescued — it just has to be named by the belt position it will occupy, which the assembler knows.

This is subtle and is the highest-risk semantic decision in the document. It needs a dedicated test.

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
- `delay` ∈ 3..15 — **the programmer chooses the latency**, and the result retires exactly that many bundles later, per §3.1

Deferred loads are the most distinctively Mill idea surviving the cut. The programmer hoists the load far above its use and states how far. The simulator honours the declared delay exactly (no early completion, no stall) — which makes mis-scheduled code fail deterministically rather than accidentally working.

Store-to-load ordering within the delay window: **stores take effect at their issuing bundle's end; a load observes all stores issued in strictly earlier bundles.** A load whose delay window spans a store to the same address returns the *pre-store* value. This is a real hazard, it is the programmer's problem, and the assembler warns when it can prove overlap.

### 8.3 Scratchpad

Frame-local, 64 slots of 8 bytes, statically addressed.

```
spill sN, b_x        ; no result, latency n/a
fill  sN             ; → 1 result, latency 3
```

`N` is a literal 0..63; there is no computed scratchpad addressing. This is where long-lived locals go when 16 belt positions aren't enough — which, with two ALU slots retiring per cycle, is constantly.

### 8.4 Poison

Uninitialized belt positions and scratchpad slots hold a distinguished poison value. Reading it is **not** a fault (that would require metadata). Instead the simulator tracks poison out-of-band and reports a warning at first use plus a hard error at `sys`/`store` of a poison-derived value. Cheap, catches real bugs, and doesn't smuggle metadata into the architecture.

### 8.5 Constants

```
con imm24            ; sign-extended to 64 bits → 1 result
```

24-bit signed immediate. Wider constants come from the assembler pseudo-op `movi imm64`, which expands to a constant-pool `load` or a `con`/`shl`/`or` sequence — assembler's choice, documented in output.

---

## 9. Toolchain

### 9.1 Components

| Component | Language | Notes |
| --- | --- | --- |
| Assembler | Rust | text → binary image; raw belt offsets; belt model used for *checking only* |
| Simulator | Rust | bundle-accurate, deterministic |
| Disassembler | Rust | round-trip verified against assembler |
| Test suite | — | golden traces + differential |

One repo, one crate workspace. The simulator's machine parameters (§2) live in a single config struct so the FPGA-oriented belt-8 / 4-slot variant is a parameter change, not a fork.

### 9.2 Raw belt offsets — no symbolic naming in v0

**The assembler accepts literal belt positions only.** Every operand is written as `b0..b15`, exactly as encoded. The programmer tracks belt state manually, including the renumbering caused by every drop.

```
.ebb loop(3)                        ; b0=i  b1=n  b2=p
    load    b2, 0, 8, zero, 4       ; retires at +4
    add     b0, b3                  ; b3 = the constant 1, dropped earlier
    ...                             ; after this bundle: b0=sum', b1=i, b2=n, b3=p, ...
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

Checks 1 and 4 together are the assembler's whole reason to exist.

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

Enough to write tests that print. Nothing more.

---

## 10. Encoding sketch

32-bit slot, 8-bit opcode, 4-bit belt references.

```
ALU 3-operand:   [op:8][b_a:4][b_b:4][unused:16]
pick:            [op:8][b_c:4][b_t:4][b_f:4][unused:12]
con:             [op:8][imm:24]
load:            [op:8][b_addr:4][delay:4][w:2][ext:1][offset:13]
store:           [op:8][b_addr:4][b_val:4][w:2][offset:14]
spill/fill:      [op:8][slot:6][b_val:4][unused:14]
branch:          [op:8][b_cond:4][target:20]      ; EBB index or PC-relative
call:            [op:8][nargs:2][b_a:4][b_b:4][b_c:4][target:10]
retn:            [op:8][nres:2][b_x:4][b_y:4][b_z:4][unused:10]
conform:         [op:8][b0:4][b1:4][b2:4][b3:4][b4:4][b5:4]
rescue:          [op:8][mask:16][unused:8]
sys:             [op:8][code:8][unused:16]
```

`call` target at 10 bits implies a function table rather than a direct offset — a reasonable v0 simplification (indirect calls come free later via a table index on the belt). Fields marked unused must be zero; the disassembler checks.

---

## 11. What we lose, and why it's acceptable

Dropping metadata drops NaR and None, and dropping those drops **speculation**. In real Mill, that's what lets `pick` replace branches wholesale and lets loops run flow-free with speculative loads hoisted above their guarding conditions. Without it, `pick` is just a conditional move and inner loops keep their branches.

That's a genuine loss of the architecture's headline result. It's acceptable for v0 because metadata touches every operand path and would double the semantic surface before anything runs. **NaR is the designated first extension** — the belt entry gains a tag bit, loads can fault-silently, and `pick` becomes interesting.

Second designated extension: full phasing, which is what makes single-bundle loops possible.

Separately, and on the tooling rather than the architecture side: the symbolic assembly layer described in §9.2, gated on having written enough raw-offset code to know what it should actually do.

---

## 12. Decisions I expect to revisit

1. **Belt 16 vs 8.** 16 for writability; 8 is the FPGA target. Parameterize from day one.
2. **Fused reshape+branch.** Would remove a bundle from every back-edge at the cost of a wider `F` slot. §7.1.
3. **Reshape ordering relative to same-bundle drops.** §7.2 is the subtlest rule here and may want inverting.
4. **`conform` capped at 6.** Falls out of a 32-bit slot; a 2-slot `conform` would lift it.
5. **Slot mix.** 2×ALU + M + F is a guess. Real code may want 2 memory slots, or a dedicated `con` slot.
6. **Fixed 16-byte bundles.** Wasteful; a slot-mask header is the obvious density fix if bundle fetch ever matters.
7. **Call as unbounded latency.** Fine for a simulator, meaningless for RTL. Revisit before any hardware work.
8. **Poison as out-of-band tracking.** A pragmatic stand-in for NaR; it disappears when metadata arrives.

---

## 13. Milestones

**M0 — skeleton.** Repo, workspace, machine config struct, binary image format, disassembler stub.

**M1 — straight-line.** ALU ops, `con`, belt model in both assembler and simulator, `sys exit`/`write`. Test: compute and print a constant expression.

**M2 — memory + scratchpad.** Loads with declared delay, stores, s
