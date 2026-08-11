# Millet ISA reference (v0)

The opcode table and encodings, as implemented. This is the M0 deliverable
promised by PRD §5.5 / QUESTIONS B2 — everything below is a decision that was
open before the assembler existed and is expensive to change now.

## Bundle

16 bytes, four 32-bit slots, little-endian:

```
byte  0..3   A0   ALU
byte  4..7   A1   ALU
byte  8..11  M    memory / scratchpad
byte 12..15  F    flow
```

Unused slots hold `NOP` (the all-zero word). Every slot word has its opcode in
the top byte; field positions below read left-to-right from bit 31 down. Bits
marked unused must be zero — the disassembler rejects a word that sets them.

## Opcode table

| opcode | mnemonic | slot | latency | operands |
|--------|----------|------|---------|----------|
| `0x00` | `nop`    | any  | —       | — |
| `0x01` | `add`    | A    | 1       | `b_a, b_b` |
| `0x02` | `sub`    | A    | 1       | `b_a, b_b` |
| `0x03` | `and`    | A    | 1       | `b_a, b_b` |
| `0x04` | `or`     | A    | 1       | `b_a, b_b` |
| `0x05` | `xor`    | A    | 1       | `b_a, b_b` |
| `0x06` | `shl`    | A    | 1       | `b_a, b_b` (shift amount mod 64) |
| `0x07` | `shr`    | A    | 1       | logical right |
| `0x08` | `sar`    | A    | 1       | arithmetic right |
| `0x09` | `mul`    | A    | 3       | low 64 bits |
| `0x0a` | `div`    | A    | 8       | signed |
| `0x0b` | `divu`   | A    | 8       | unsigned |
| `0x0c` | `rem`    | A    | 8       | signed |
| `0x0d` | `remu`   | A    | 8       | unsigned |
| `0x0e` | `eq`     | A    | 1       | drops 0 or 1 |
| `0x0f` | `ne`     | A    | 1       | |
| `0x10` | `lt`     | A    | 1       | signed |
| `0x11` | `le`     | A    | 1       | signed |
| `0x12` | `ltu`    | A    | 1       | unsigned |
| `0x13` | `leu`    | A    | 1       | unsigned |
| `0x14` | `pick`   | A    | 1       | `b_c, b_t, b_f`; nonzero condition selects `b_t` |
| `0x15` | `con`    | A    | 1       | `imm24`, sign-extended to 64 |
| `0x16` | `none`   | A    | 1       | drops a None |
| `0x17` | `isnar`  | A    | 1       | `b_a` → 1 if it is a NaR |
| `0x18` | `isnone` | A    | 1       | `b_a` → 1 if it is a None |
| `0x20` | `load`   | M    | `delay` | `b_addr, offset, width, ext, delay` |
| `0x21` | `store`  | M    | —       | `b_addr, offset, width, b_val` |
| `0x22` | `spill`  | M    | —       | `sN, b_val` |
| `0x23` | `fill`   | M    | 3       | `sN` |
| `0x30` | `br`     | F    | —       | `label` |
| `0x31` | `brt`    | F    | —       | `b_cond, label` |
| `0x32` | `brf`    | F    | —       | `b_cond, label` |
| `0x33` | `call`   | F    | 1       | `func, b_a…` (0–3 args) |
| `0x34` | `retn`   | F    | —       | `b_x…` (0–3 results) |
| `0x35`–`0x3a` | `conform` | F | — | 1–6 positions; arity is in the opcode |
| `0x3b` | `rescue` | F    | —       | `mask16` |
| `0x3c` | `sys`    | F    | 1 (codes 1, 2) | `code` |
| `0x3d` | `halt`   | F    | —       | — |
| `0x3e` | `bri`    | F    | —       | `b_target`; unconditional, EBB-table index from the belt |
| `0x3f` | `calli`  | F    | 1       | `b_target, b_a… -> nres`; function-table index from the belt |

`add`, `sub`, `mul` and the shifts wrap silently. `div`/`rem` fault on a zero
divisor and on signed `INT_MIN / -1`. Any nonzero value is true for `brt`,
`brf` and `pick`.

### Indirect transfers

`bri` and `calli` take their target from the belt as a table index, which is
what `con &label` and `con @func` produce. Two consequences fall out of the
belt being a static property (PRD §3.2):

- **`calli` carries its own result count**, because the belt has to renumber by
  an amount the assembler knows and the callee is not one of those. The
  simulator faults if the callee's declaration disagrees with the call site.
- **E2 cannot check a `bri` edge**, since the successor set is not known. The
  simulator checks the entry arity when the branch lands instead. E9 still
  applies statically: nothing may be in flight at the transfer.

### Latency convention

Latency 1 means "retires at the end of the issuing bundle". An op issued in
bundle `t` with latency `L` retires at the end of bundle `t + L - 1`. A load's
`delay` is a latency on that same scale, so `delay 4` means the result is
readable in bundle `t + 4`. Latencies count bundles of the issuing frame only.

## Slot encodings

```
ALU 2-operand:   [op:8][b_a:4][b_b:4][unused:16]
pick:            [op:8][b_c:4][b_t:4][b_f:4][unused:12]
con:             [op:8][imm:24]                          ; signed
none:            [op:8][unused:24]
isnar/isnone:    [op:8][b_a:4][unused:20]
load:            [op:8][b_addr:4][delay:4][w:2][ext:1][offset:13]
store:           [op:8][b_addr:4][b_val:4][w:2][offset:13][z:1]
spill/fill:      [op:8][slot:6][b_val:4][unused:14]       ; fill: b_val zero
branch:          [op:8][b_cond:4][target:20]              ; EBB-table index
call:            [op:8][nargs:2][b_a:4][b_b:4][b_c:4][target:10]
retn:            [op:8][nres:2][b_x:4][b_y:4][b_z:4][unused:10]
conform1..6:     [op:8][b0:4][b1:4][b2:4][b3:4][b4:4][b5:4]
rescue:          [op:8][mask:16][unused:8]
sys:             [op:8][code:8][unused:16]
halt:            [op:8][unused:24]
bri:             [op:8][b_target:4][unused:20]
calli:           [op:8][nargs:2][nres:2][b_a:4][b_b:4][b_c:4][b_target:4][unused:4]
```

`w` encodes the access width: `0`→1 byte, `1`→2, `2`→4, `3`→8. `ext` is 1 for
sign extension. Both offsets are signed 13-bit; the store word's low bit is
reserved zero. Load `delay` values 0–2 are illegal and rejected by both the
assembler and the disassembler.

## Operand metadata

Every belt and scratchpad value is 64 bits of data **and** a tag: a plain
value, a **None**, or a **NaR**. The tag is not addressable and there is no op
that writes one directly other than `none`; it is produced and consumed by the
rules below (PRD §8.6).

| producer | tag |
|----------|-----|
| `none` | None |
| a `load` whose address is unbacked | NaR |
| `div`/`rem` by zero, or signed `INT_MIN / -1` | NaR |
| any op with a poisoned operand | None if one is a None, else that NaR |
| a belt position nothing has dropped to, or a scratchpad slot nothing has spilled to | None |

A NaR's 64 data bits are its payload: what failed and the bundle it failed in,
which is what the fault diagnostic quotes. Propagation copies the payload, so
the origin survives however far the value travels.

Everything is speculable except the ops below, which **realize** what they are
handed. Control flow is the common thread: a branch that cannot say where it
goes has nowhere to defer the question to.

| op | on a None | on a NaR |
|----|-----------|----------|
| `store` (value or address) | suppressed: memory is not touched | fault |
| `brt`/`brf` (condition) | fault | fault |
| `bri`/`calli` (target) | fault | fault |
| `sys` (any operand it reads) | fault | fault |

`pick` is the way poison is meant to die: only the selected operand's tag
survives, so a value speculated down the path not taken cannot poison the
result. A poisoned *condition* propagates instead — `pick` is speculable too.

`spill`/`fill` are not realizing operations. The scratchpad holds whole
operands, so a spilled NaR fills back as the same NaR; memory holds bytes, and
that asymmetry is the reason `store` has to realize. `isnar`/`isnone` read the
tag without realizing it, and are the only way to observe one without faulting.

Millet carries no width or scalarity metadata: no operation here is
width-polymorphic — widths live in the `load`/`store` encodings — so those bits
would be carried and never read.

## `sys`

`sys` is the only op that does not name its operands — it reads `b0..b2`.

| code | operation | result |
|------|-----------|--------|
| 0 | `exit(b0)` | — (the process exit code is `b0 & 0xff`) |
| 1 | `write(fd=b0, ptr=b1, len=b2)` | bytes written |
| 2 | `read(fd=b0, ptr=b1, len=b2)` | bytes read |

`halt` is the abnormal stop: the simulator prints a diagnostic and exits 3.
`sys 0` and a `retn` from the entry function are the clean exits.

## Binary image

```
magic   "MILT"
version u32 = 1
counts  u32 × 4   bundles, ebbs, funcs, data segments
entry   u32       index into the function table
code    bundles × 4 × u32
ebbs    each: u32 first bundle, u32 arity, u32 name length + name bytes
funcs   each: u32 ebb index, u32 arity, u32 result count, name
data    each: u64 load address, u32 length, bytes
```

Source labels are carried in the image so traces and disassembly can say
`as_loop` rather than `ebb3`. They are debug information only — nothing in
execution depends on them, and a hand-built image may leave them empty.

Execution starts at the entry function, which must take no arguments.
`.entry` names it; without one the assembler looks for `main`.
