# Backlog

Tooling work, ranked by value per line. Nothing here changes the ISA — the
encoding, the machine model and `PRD.md` stay as they are; this is all about
the assembler, the simulator and the tests being nicer to use.

Rough order to do them in: 2 → 1 (they compound: better errors, then those
errors and the annotations both carrying the names `%n` already gives them),
then 3 → 4 → 5 as an independent debugging-visibility track. 6 is a net
deletion and fits anywhere.

## 1. `mas --annotate` — regenerate the belt comments

PRD M5. The checker knows the belt at the entry of every bundle, and nothing
checks the hand-written comments that duplicate it. `mas --annotate f.mil`
rewrites them in place, carrying the `%name`s where a bundle has them and the
producing line (`b0=@37`) where it does not. The same code path annotates
`mas -d` output.

Roughly 80 lines, and it kills stale-comment rot permanently.

## 2. Diagnostics that show the source line and the belt

Today:

```
bad.mil:5: error[E1]: `add` reads b7, which holds no live value (never dropped
since this EBB's entry, or truncated away by the entry arity)
```

No source line, no caret, and no statement of what *is* live — even though the
checker is holding exactly that.

```
bad.mil:5: error[E1]: `add` reads b7, which holds no live value
  5 |     a0  add b0, b7
    |                 ^^
    = belt here: b0=7 (con, line 3)  b1=5 (con, line 2);  b2..b15 unreached
```

Roughly 60 lines: carry the source text into `AsmError`, add a column span to
`Diag`.

## 3. A bundle -> source-line map in the image

Everything downstream speaks bundle numbers: faults (`the store in bundle 42
consumed a NaR from bundle 39`), traces, and `mview` — whose code pane is
built by `listing(&img)` (`millet-view/src/main.rs:603`) from the
*disassembly*, so handing it a `.mil` still hides your comments.

One optional `u32`-per-bundle section in the image, written by `mas` and
absent for hand-built images. Unlocks:

- `msim: fib.mil:87 in fib_rec: the store consumed a NaR from fib.mil:81
  (load from an unbacked address)`
- `mview` showing the real source, comments and all

Roughly 40 lines in `image.rs` plus a field on `SrcBundle`.

## 4. `msim --trace-on-fault <n>`

`--trace` on `fib.mil` is 29MB, and what you want is the 30 bundles before the
fault. Keep a `VecDeque` of the last n rendered records and dump it on
`Stop::Fault`. Roughly 25 lines. The same plumbing gives `--trace-func fib`
and `--trace-from <cycle>`.

## 5. `--stats` that answers the question this repo exists to ask

Current stats: bundles, calls, max depth, slot occupancy. Missing, and all of
it just counters on the issue path:

- **a histogram of belt read offsets** — a direct read on "does the belt-8
  variant work for this program?", which `README.md` calls out as *the*
  interesting parameter change. Necessary but not sufficient (shrinking the
  belt also changes which values survive to a branch), and still the first
  number you want.
- max live belt positions, scratchpad high-water mark, max ops in flight
- bundles per function and per EBB — where the cycles went

Roughly 60 lines, and the best insight-per-line available here.

## 6. Data-driven example tests

`millet-sim/tests/programs.rs` carries one hand-written `#[test]` per example.
Move the expectation into the file itself —

```
; expect: exit 42
; expect-out: "6765\n"
```

— and glob `examples/*.mil`. Adding an example stops requiring a Rust edit,
and it deletes about 70 lines.

## 7. Encoding round-trip fuzz

`millet-asm/tests/roundtrip.rs` covers the twelve committed examples. A
deterministic LCG generating random legal bundles through
encode -> decode -> disasm -> asm broadens that a lot for about 50 lines and
no new dependency. Worth doing the next time the encoding changes, not before.

## Deliberately not doing

- **An interactive breakpoint debugger in `msim`.** `mview` already time-travels
  over the whole run; this duplicates it. Revisit when traces stop fitting in
  memory.
- **Trace snapshots or delta encoding in `mview`.** `README.md` already scopes
  this: add it for a program an order of magnitude longer than `fib`.
- **A macro or include preprocessor for `.mil`.** `cpp` exists; pipe it.
