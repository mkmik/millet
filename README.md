# millet

Explorations on the Mill ISA — a minimum-viable Mill-like ISA with an
assembler and a simulator. `PRD.md` is the specification; `docs/ISA.md` is the
opcode table as built; this file is how to drive it.

There are no registers. Every value lives on a 16-position belt and is named
by how recently it was produced. You place every operation in a specific slot
of a specific bundle, and you track the belt yourself — the assembler checks
your offsets but never invents them.

## Build and run

```
cargo build
cargo test

cargo run -p millet-asm --bin mas -- examples/fib.mil -o /tmp/fib.mimg
cargo run -p millet-sim --bin msim -- /tmp/fib.mimg
```

Useful flags:

```
mas  --check prog.mil        run the static checks, write nothing
mas  -d prog.mimg            disassemble (round-trips byte-identically)
mas  --predict p.json        dump the predicted per-bundle belt liveness

msim --trace       prog.mimg per-bundle belt, drops, memory and scratch effects
msim --trace-json  prog.mimg the same, one JSON object per line
msim --stats       prog.mimg cycles, calls, depth, slot occupancy
```

Traces go to stderr, program output to stdout.

## Writing a program

One op per line, prefixed by its slot tag. A blank line ends the bundle;
omitted slots assemble to `NOP`. Comments start with `;`.

```
.data 0x1000
.ascii "hello, millet\n"

.func main(0) -> 0
    a0  con 14                  ; len
    a1  con 0x1000              ; ptr
                                ; belt: b0=ptr b1=len   (a0 drops first)

    a0  con 1                   ; fd
                                ; belt: b0=fd b1=ptr b2=len

    f   sys 1

    f   retn
```

The trailing belt-state comment per bundle is the reference style. You will
want it.

Directives: `.func name(arity) -> nres`, `.ebb name(arity)`, `.data addr`
followed by `.u8/.u16/.u32/.u64/.ascii/.asciiz/.zero`, `.def NAME value`, and
`.entry name`. Immediates accept decimal, `0x`, `0b`, `'c'`, and `.def` names.

### The three things that will bite you

1. **Every drop renumbers the belt.** Two ALU ops in one bundle drop in slot
   order, so `a0`'s result ends up *above* `a1`'s: after `a0 con 3` and
   `a1 con 4` you have `b0=4, b1=3`.
2. **Nothing in a bundle can see anything else in that bundle.** All reads
   observe the belt at bundle entry. The exception is `conform`/`rescue`,
   which read the belt *after* this bundle's drops.
3. **Nothing may still be in flight when control leaves an EBB.** A `mul` is
   3 bundles, a `div` is 8, a load is however many you asked for — all of it
   has to land before the branch.

## Examples

| file | what it shows |
|------|---------------|
| `hello.mil` | `sys 1`, dropping operands into the fixed `b0..b2` positions |
| `pick.mil` | `pick` as a conditional move |
| `scratch.mil` | `spill`/`fill`, `mul` latency |
| `sum_unrolled.mil` | four deferred loads in flight at once, no branches |
| `strlen.mil` | a loop, `conform` on the back edge |
| `arraysum.mil` | the same shape reshaped with `rescue` instead |
| `memcpy.mil` | load deferred 4 bundles inside a 6-bundle body, no spills |
| `divmod.mil` | a function returning two results |
| `fib.mil` | recursive `fib(20)`, plus decimal printing written by hand |
| `ackermann.mil` | `ack(2,3)`, three-way recursion across five EBBs |

## Static checks

Each check has a stable code and a test in `millet-asm/tests/checks.rs`.

| code | check |
|------|-------|
| E1 | a belt reference names a position holding no live value |
| E2 | an edge into an EBB does not deliver its entry arity |
| E3 | the op is not legal in its slot |
| E4 | a read of a position no value has reached yet, with ops in flight |
| E5 | `conform` over 6 positions, or a `rescue` mask past the belt |
| E6 | call/return arity disagrees with the declaration |
| E7 | scratchpad slot out of range |
| E8 | *(warning)* a store lands inside an in-flight load's delay window |
| E9 | an operation is still in flight at a control transfer out of an EBB |
| E0 | syntax, encoding range, and structural errors |

E9 is a warning rather than an error at `retn`, where discarding in-flight
results is legal (PRD §6.3).

## Layout

```
millet-core/   ISA definitions, encoding/decoding, the binary image format
millet-asm/    assembler + static belt model + disassembler   (binary: mas)
millet-sim/    the simulator                                   (binary: msim)
examples/      hand-written .mil programs
tests/golden/  committed --trace-json traces (MILLET_BLESS=1 to regenerate)
docs/ISA.md    opcode table, encodings, image format
```

No external crates: the whole thing is standard-library Rust.

The machine parameters (belt 16, 4 slots, 64 scratchpad slots, 16-byte
bundles) live in one `Config` struct in `millet-core`, so the FPGA-oriented
belt-8 variant is a parameter change rather than a fork.

## Testing

`cargo test` runs four layers:

- encoding and image round-trip unit tests in `millet-core`;
- one test per static-check class in `millet-asm/tests/checks.rs`;
- byte-identical disassembler round-trip over the whole corpus;
- every example assembled, run, and checked for its output — including the
  differential check of PRD §9.1, which compares the assembler's statically
  predicted belt liveness for each bundle against what the simulator actually
  had live when it executed it;
- golden `--trace-json` traces for the small programs.

`millet-sim/tests/semantics.rs` covers the rules the PRD flags as subtle:
post-drop reshape numbering, drop ordering, the store/load window, frame-local
latency across a call, and machine-enforced belt truncation at edges.

## Where this deviates from PRD.md, and why

These came up while building; they are the interesting things to argue about.

- **Load `delay` is a latency on the same scale as everything else**, so
  `delay 4` retires at the end of bundle `t+3`. §5.4 lists it in the latency
  column next to `add = 1`, which only works if the scales match; the "retires
  at +4" comment in §9.2 reads the other way. One convention had to win.
- **Check E2 rejects an edge that delivers *too few* values, and accepts one
  that delivers more.** The standard loop idiom leaves the branch condition
  sitting just above the carried set — the reshape must happen in the bundle
  *before* the branch, so the condition cannot be anywhere else — and
  truncation at entry is machine-enforced anyway (§4). Warning on it would
  have fired on every loop in `examples/`.
- **Check E4 is the decidable half of what §9.3 asks for.** "No read of a
  position occupied by an in-flight result" is not decidable in general: when
  you read `b0` a bundle after issuing a `mul`, `b0` holds a perfectly live
  older value and nothing in the program states which one you meant. What the
  assembler *can* catch — and does — is a read of a position that no value has
  reached yet while something is in flight, which is the shape the bug takes
  at the top of an EBB or with a short live prefix. The general case needs the
  symbolic layer of §9.2.
- **Stores and spills are applied between the M and F slots**, not literally
  at bundle end. This is invisible except when the same bundle also contains a
  `call` or a `sys`, where the alternative is a callee that cannot see a store
  the caller just issued. Loads and fills still sample at issue, so the
  store→load and spill→fill hazard windows of §8.2/§8.3 are unaffected.
- **Empty bundles are written `a0 nop`.** A blank line is a bundle separator,
  so it cannot also denote a bundle. `nop` encodes to the all-zero word, so
  this is free.

## Not done in v0

No belt-state annotation in the disassembler and no trace viewer (PRD M5), and
no symbolic assembly layer (§9.2) — that one is gated on writing enough
raw-offset code to know what it should do. `examples/fib.mil` is the argument
for it.
