# The Mill CPU: semantics of in-flight operations across branches, calls, and returns

Research notes supporting the A2/A3 decisions in `QUESTIONS.md`. Based on
primary sources: Ivan Godard's posts on the millcomputing.com forum (the forum
has migrated to a Flarum instance at `millcomputing.com/forum/d/<id>-<slug>`;
the old `millcomputing.com/topic/...` permalinks are dead, but all posts are
intact), Mill Computing's US patent 9,690,581 "Computer processor with
deferred operations" (Godard et al.), the Mill docs pages, and secondary
writeups. Post numbers below are the forum's stable post IDs.

A key framing fact first: in-flight results on the Mill are not tagged by
EBB — they are tagged by **frame** (the belt "nesting number" created by call)
and by **issue cycle**. Branches do not create a new frame; calls do. That
single fact drives almost every answer below.

---

## Q1: In-flight operations across control transfer (taken branch, entering another EBB)

### 1a. In-flight ops keep counting and retire normally in the successor EBB — with one big caveat about branches that carry belt arguments

The single most authoritative statement is Ivan Godard's post #3316 in the
"Control flow divergence and The Belt" thread
(https://millcomputing.com/forum/d/3314-control-flow-divergence-and-the-belt):

> "In addition, when a branch is taken there may be a functional unit at work
> on a long-latency operation that has not yet retired. **This is not a
> problem for branches without carried args; EBB-A can start an operation
> that will retire in EBB-B**, because the belt in EBB-B is unambiguous both
> before and after the retire. In principle, the same could be done over
> branches that have carried args, but only if every source of an inbound
> branch arranged to have its long-latency operation retire at the same point
> in the target. **Such a coincidence is extremely far-fetched and the
> specializer does not attempt to use it. Instead, it forces all operations
> (except loads) to have retired before any branch with carried args.** Loads
> are the exception, and the Mill has the special Pickup Load form to deal
> with loads that retire across arg-carrying branches. A pickup load
> operation carries a tag and does not retire normally. Instead, sometime
> later the code can issue a Pickup operation with the same tag and get the
> retire dropped to the belt. The Load and its Pickup can be in different
> ebbs; the only requirement is that each Load must eventually be picked up,
> and each pickup must have a previous load with the same tag."
> — Ivan Godard, 2018-10-12

Precisely:

- The Mill has two flavors of branch: **without carried belt arguments**
  (target inherits the branch's belt unchanged) and **with carried
  arguments** (the branch replaces the belt with its argument list,
  "essentially the same way a call does" — same post; also "A carrying
  branch acts like half a call, except there's no new frame and no return,"
  post #3326, same thread).
- Across an **argless branch**, multi-cycle ALU ops and cycle-counted
  deferred loads simply keep counting (in the frame's issue cycles) and drop
  in the successor EBB at their statically scheduled cycle.
- Across an **arg-carrying branch** (which is what a join with non-congruent
  predecessor belts requires), the specializer *forces everything except
  loads to have retired before the branch*. There is no hardware rule
  demanding identical in-flight state on all inbound arcs — instead the
  compiler makes the in-flight state trivial (empty except loads) at such
  joins.
- Loads crossing arg-carrying branches use the **tag + pickup form**
  (retire point determined dynamically by when the `pickup` op executes)
  rather than the cycle-count form. Corroborated by US 9,690,581
  (https://www.freepatentsonline.com/9690581.html): "Another method of
  controlling the schedule latency, applicable in circumstance for which it
  is impossible to statically know the number of machine cycles between the
  desired points of issue and retire of the operation, is to encode a
  statically assigned operation identifier ... At some subsequent point, the
  machine code includes a separate 'pickup' operation carrying the same
  operation identifier, which defines the retire point of the original
  operation."

### 1b. Joins and belt congruence (conform / rescue / branch-carried args)

From "The Belt" thread (https://millcomputing.com/forum/d/250-the-belt),
Ivan post #402:

> "The operation that renumbers the belt is called 'conform' ... Any time you
> have a control-flow join point in which the in-bound arcs do not have
> congruent belts (such as at the top of a loop) then you pick one of the
> arcs, define its belt as canonical, and use conform on the other arcs to
> make everything congruent. For loops you generally want to define the most
> frequently taken backwards branch as the canonical one; that eliminates the
> conform that would be most frequently executed."

Later the design changed — post #3536, same thread:

> "**There isn't any conform op any more; the functionality was moved to
> branch ops so a taken branch can rearrange the belt to match the
> destination.** However, the same question applies: the rearranging takes
> zero cycles and has essentially no cost. Nothing is actually moved; all
> that happens is that the mapping from belt number (to decode) to operand
> location (dynamic) is shuffled."

Congruence is a static invariant: "All inbound arcs must carry the same
number of operands ... Consequently C sees the same belt structure regardless
of how or from where it received control" (post #3328, thread 3314).
`rescue` still exists as the compact keep-these-alive renumbering op
(post #402; also listed among current polyadic flow ops in post #3330).

Note `conform`/`rescue`/branch-carried renumbering only remap
*already-dropped* belt operands; they don't touch in-flight values — the
specializer's forced-retire rule above is what keeps the two mechanisms from
colliding.

### 1c. Loops, back-edges, retire stations, and the `retire` op

- **Deferred loads across loop back-edges are routine and are the intended
  use.** In a software-pipelined loop, loads from several iterations are
  simultaneously in flight; the binding resource is the number of retire
  stations. From "Loop pipelining and aliasing"
  (https://millcomputing.com/forum/d/1174-loop-pipelining-and-aliasing),
  Ivan post #1180: "A load uses the retire station as a blocking resource,
  but the blocking range is only for the period from issue to retire of that
  load ... If there are eight retire stations, then we can have eight loads
  in flight at any one time ... the specializer (when scheduling a pipelined
  loop) wants to use a deferral of at least three [the D$1 latency] ... the
  schedule must add additional cycles to the software-pipeline" if it would
  run out of stations. Also #1181: "only the specializer knows how many
  in-flight deferred loads are possible on the target." Modern numbers in
  "Grab bag of questions"
  (https://millcomputing.com/forum/d/3772-grab-bag-of-questions), post
  #3817: "Deferral is limited by the encoding ... a Copper can defer 11
  cycles and a Silver 20 ... we have six in flight, which helps size the
  retire station count."
- **Deferral is counted in issue cycles, not wall-clock cycles**: "the
  deferral can be zero so that a load issued in this cycle can retire in the
  next *issue* cycle, albeit not in the next *clock* cycle" (post #1180).
  Stall cycles don't advance the count.
- **The `retire` op (Dave's Device)** is a *pipeline-prologue* device, not a
  general in-flight/branch mechanism: in the first iterations of a piped
  loop, `retire(n)` pads the cycle's drops with Nones standing in for
  results "that are in flight and haven't dropped yet," so the belt layout of
  early iterations is congruent with steady state. Ivan post #1251 in
  "Pipelining" (https://millcomputing.com/forum/d/1211-pipelining): "The
  Nones that are dropped by retire are stand-ins for values that are in
  flight and haven't dropped yet ... all the Nones from retire, no matter how
  many there are, will be in a block at the front of the belt, followed by
  any real retires, followed by the rest of the pre-drop belt." This works
  because drops are in canonical latency order.
- **Loop exit**: exiting branches are replaced by `leave` ops with an
  `inner` op at the loop head (post #1545, Pipelining thread); "if the exit
  condition depends on ... all non-speculable operations in the body then the
  leave operation (conditional on the exit condition) replaces the epilog"
  (post #1387). There is an explicit abandon mechanism for outstanding
  in-flight state: "There is an abandon mechanism for pickup loads and other
  in-flight state" (Ivan post #328, "Memory" thread,
  https://millcomputing.com/forum/d/251-memory); the patent calls the
  corresponding op "refuse" (a pickup that discards). Plausible inference:
  `leave` performs exactly this abandonment of the loop's still-in-flight
  speculative state; no public sentence spells that out verbatim.

### 1d. Mispredicted branches — wrong-path results discarded by issue-cycle tag, right-path in-flights replayed

Ivan post #1680 in "Execution"
(https://millcomputing.com/forum/d/634-execution):

> "Big OOO machines ... if they mispredict they throw away everything that is
> in flight and re-execute the instructions; this is *issue replay*. **The
> Mill takes a different approach, marking each in-flight with the cycle that
> it issued in. At a mispredict we let the in-flights drain to the spiller
> buffers, the same way we do for in-flights over a call or interrupt,
> discarding those marked as being from the cycles down the wrong path.
> Meanwhile we are restarting the decoder, and as the new ops start execution
> we replay the former in-flights just as we do after a return. This is
> *result replay*.** In summary: phasing proceeds in the presence of control
> flow just as it would have had all the control flow been inlined."

A correctly-predicted taken branch cancels nothing; a mispredict discards
only results tagged with wrong-path issue cycles, and legitimate pre-branch
in-flights survive and are re-injected with correct timing. For loads
specifically, the retire stations also snoop stores (and the coherence
protocol) and transparently re-request on aliasing (posts #328, #2145,
Memory thread), so an in-flight load is never semantically stale: "the load
is defined to return the value as of retire, not as of issue" (post #3816,
Grab bag thread).

---

## Q2: In-flight operations across CALL and RETURN

### 2a. Verified: calls take zero time in the caller's schedule; pre-call in-flights retire after return as if the call never happened

"The Belt" thread post #3229
(https://millcomputing.com/forum/d/250-the-belt):

> "**Calls (including the body of the called function) have zero latency.
> The FMA drops after the call returns. The Mill spiller not only saves the
> current belt and scratchpad, but also everything that is in-flight. The
> in-flights are replayed when control returns to the caller, timed and
> belted as if the call hadn't happened. That's how we can have traps and
> interrupts be just involuntary calls. The trapped/interrupted code is none
> the wiser.**"

"Execution" thread post #982
(https://millcomputing.com/forum/d/634-execution):

> "As far as the specializer is concerned, a call is just a zero-latency op.
> F(G(), x*y) has the same constraints as F(a+b, x*y) ... It doesn't matter
> how long either function takes to execute; **for scheduling, all calls take
> zero cycles, i.e. they complete in the same instruction that they issue
> in.**"

Mechanism (verified): results physically pop out of the FU *during* the
callee, but carry the caller's frame ID and are captured, not dropped on the
callee's belt. Will Edwards post #971 + Ivan #972/#975 (Execution thread):
"Operations mark their output belt items with the frame id their instruction
was issued in. The spiller takes care of saving and restoring this in the
background." Ivan, "spiller work optimisation" post #2113
(https://millcomputing.com/forum/d/2111-spiller-work-optimisation):

> "it is possible for an FU to produce a result operand that does not belong
> to the current issue frame. For example, if I issue a long-running
> operation like a FP multiply and then immediately do a call, **the program
> sees the multiply result as coming out after the call returns, but in
> reality it comes out as soon as the multiplier is done, in the middle of
> the callee even though the operand belongs to the caller.**"

The general philosophy is result replay, not issue replay (Ivan #972,
Execution thread): "we let all operations run to completion, save the
results, and on restart we inject the saved results with the same timing and
semantics that would have occurred had the interrupt not happened."

### 2b. Deferred loads across calls: callee time is excluded from the deferral count

- **Callee execution does not count against the caller's load delay** —
  verified at the model level by 2a ("timed and belted as if the call hadn't
  happened"; scheduling counts calls as zero cycles), and at the patent
  level: US 9,690,581 explicitly defines both "inclusive" and "exclusive"
  deferral, and notes "Because the occurrence of an interrupt or trap is
  generally unpredictable by the program, inclusive deferral ... can be
  impractical by design. That is, deferrals must generally be saved and
  restored over such events." Since the Mill treats traps/interrupts as
  involuntary calls indistinguishable from real calls (post #3229), the
  deployed semantics is the exclusive one: the count-down timer "does not
  count down during the period executed within a function frame activation."
  (Strong inference — Ivan never uses the word "exclusive" on the forum.)
- **Physically, the load keeps working during the call** — a feature: the
  callee's wall-clock time hides the miss while the logical deferral is
  frozen. Ivan, Grab bag post #3816: "even on a narrow member the load can
  be hoisted over the paint call of the prior iteration. **The load then has
  the full duration of the call to deal with data cache misses.** ... This
  is safe ... because the load is defined to return the value as of retire,
  not as of issue, and retire is after the paint call."
- **Retire-station exhaustion mid-call**: the callee gets a full complement
  of retire stations ("a callee can assume that caller state is spilled and
  later restored in full: everything is in effect caller-saved. Thus it will
  assume that it has a full scratch and retire station complement
  available" — Ivan, Grab bag post #3830). If the call chain needs more
  stations than exist, caller loads are evicted to spiller state and
  *reissued* on return; the patent describes exactly this reissue model
  ("any pending inflight DLOAD operation is aborted and the results (if any)
  are discarded, and only the DLOAD operation and its arguments are saved.
  At the restore time triggered by the RETURN operation, the DLOAD operation
  is then reissued ... no later than the retire step"), and allows a hybrid.
  Which variant ships is per-member and architecturally invisible (post
  #1572, Belt thread).
- Spiller capacity is unbounded (backed by memory): "The spiller has
  unlimited size, because under the buffering is all of memory ... Both
  spiller and the memory hierarchy are able to issue-stall the core if they
  need to catch up" (Ivan, Pipelining thread post #1249).

### 2c. RETURN with the callee's own ops still in flight: legal, results discarded ("dead on creation")

Verified, three independent statements:

- Ivan, spiller thread post #2113: "Likewise, if you start a multiply and
  then do a return op, **there is no way to stop that multiply and you will
  get a result, which belongs to a callee that no longer exists and is dead
  on creation.**"
- Ivan, Belt thread post #1576: "Say frame 17 calls frame 18, which does a
  MULF but returns to 17 before the MULF finishes. **When the result pops
  out of the multiplier it is labeled as belonging to 18, known to be
  exited, and is discarded.**" (Posts #1574–#1578 discuss why this
  frame-numbering makes hardware tail calls tricky: a TCO'd callee reusing
  frame 18 could receive the dead MULF's drop.)
- Will Edwards, post #1596: "we make sure that results don't end up in the
  wrong frame and that **results that are in-flight when a function returns
  are discarded.** How this happens is implementation-specific."

Return does not wait; there is no fault (a forum user's suggestion that
unexpected in-flight values at return should fault was not adopted —
#1592/#1596). The patent covers the load case identically: "the RETURN
operation can be treated as an implicit refuse of the inflight DLOAD
operation such that the inflight DLOAD operation is discarded."

### 2d. Does callee time count against caller latencies? No.

Latency is counted in the issuing frame's own issue cycles. "For scheduling,
all calls take zero cycles" (#982); in-flights are "replayed when control
returns to the caller, timed and belted as if the call hadn't happened"
(#3229). This is also what makes interrupts/traps transparent, and it's
uniform across ALU ops and loads.

---

## Q3: Operand metadata — what a belt item carries, and whether the scratchpad carries it too

Supports the metadata extension (PRD §8.6). Sourcing caveat up front: the
`millcomputing.com/wiki/*` pages that cover this best (Metadata, Scratchpad,
Speculation) are **404 today** and `web.archive.org` was unreachable from here,
so the wiki quotes below come from a search index of those pages and their
wording may differ slightly from the original. The HandWiki quotes were
fetched directly.

### 3a. Verified: the metadata is status, width and vector count — and it *is* the operand descriptor

From the HandWiki mirror of the deleted Wikipedia article
(https://handwiki.org/wiki/Mill_architecture), fetched directly:

> "Depending on the type and success of load operations, the Mill also assigns
> metadata to each belt item, including status, width, and vectorization
> count."

> "**Operations operate on the item described. Thus, the width and vector
> count are not part of the instruction coding.**"

The second sentence is exactly why Millet carries the status tag and drops the
other two: on the Mill an operation is width-polymorphic and reads its width
from the operand, while every Millet op has its width in the encoding, so
those bits would be carried and never read.

### 3b. Verified: NaR and None are metadata rather than bit patterns, and both propagate

HandWiki, directly:

> "If an operation fails, the failure information is hashed, and placed in the
> destination, with its metadata, for use in debugging."

> "The NaR items create a fault only if an attempt occurs to store them or
> perform other non-speculative code on them. If they are never used, no fault
> is ever created."

> "operations where at least one argument is a `None` generally produce a
> `None` as output, and when a `None` is attempted to be stored to memory,
> that store (or portion of a store for vectors where only some elements are
> `None`) is ignored, leaving that memory location undisturbed."

> "This special `None` value is **not implemented as a reserved bit pattern**,
> but by using the extra metadata bits that are associated with each belt
> item."

Precedence and the realizing set, from the wiki/forum via the search index:

> "None has precedence over NaR; a NaR and None is None." — and, on the same
> point, "the None NaR has a higher precedence over all other kinds of NaR, so
> if you perform arithmetic with NaR and None values the result is always
> None; None is used to discard and mask-out speculative execution."

> "If you try and store a NaR, or store to a NaR address, or jump to a NaR
> address, then the CPU faults. When a realizing operation encounters a None,
> it does nothing — a load from a None address produces a None, and a store
> with a None value or address doesn't write anything."

So: the realizing set is stores, control flow and IO; everything else, loads
included, is speculable. Millet implements exactly this, minus vectors, and
adds the `sys` case explicitly because Millet's IO is one op (PRD §8.6).

### 3c. The scratchpad preserves metadata. Memory does not.

The answer is unambiguous, from the (offline) Scratchpad and Metadata wiki
pages via the search index:

> "**The scratch and spill preserves metadata, dealing with belt items and not
> naked bytes**, maintaining all item state." … "**metadata is preserved in the
> Scratchpad but discarded again on store.**"

> "If the size of the scratchpad is exceeded during operation, the spiller
> transparently manages shuffling values into the spill buffer and eventually
> into system memory, which doesn't pollute the caches and **preserves value
> metadata**."

This is also the only self-consistent design. The spiller has to save and
restore the belt and the scratchpad across calls and interrupts *exactly*
(§2a), and a speculative value that gets parked in scratch has to come back
speculative — if a spill dropped the tag it would either lose a None or
silently launder a NaR into a plain value, and speculation across a call
would be unusable. Memory is the opposite case: it is byte-addressed and
shared, there is nowhere to put a tag, and that is precisely why `store` is a
realizing operation. Millet follows both halves.

---

## Summary: conform/rescue and branches vs. in-flight drops

- `conform`/`rescue` (now: branch-carried argument lists, `rescue` retained;
  `br`/`call`/`retn`/`rescue` all share the same bulk-remap machinery in
  decode — post #3336, thread 3314) renumber only belt-resident operands, in
  zero cycles, with no data movement (#3536).
- Branches cancel nothing. In-flight ops are oblivious to control flow
  within their frame; they are tagged with frame + issue cycle and drop when
  their count expires (argless-branch case) or when picked up (pickup case).
  Only two things ever kill an in-flight result: exiting its frame
  (return/tail-call — discarded on emergence) and mispredict recovery
  discarding wrong-path-cycle results (#1680).
- At any join whose belts need reconciliation, the specializer guarantees by
  construction that nothing except pickup-form loads is in flight (#3316) —
  the Mill's real "rule about in-flight state at joins" is not "identical
  in-flight schedules on all paths" but **"empty in-flight state, modulo
  tagged loads, on all paths."**

## Confidence and gaps

**High confidence (multiple direct Godard statements):** zero-latency calls;
spiller saves belt + scratchpad + all in-flight state; result replay after
return/interrupt/mispredict with original timing; callee-frame in-flights
discarded at return; argless branches let ops retire cross-EBB; arg-carrying
branches require everything but loads retired, loads crossing via pickup;
retire stations bound in-flight loads; deferral counted in issue cycles;
loads return value as-of-retire with station snooping.

**Reasoned inference:** that the shipped semantics is the patent's
"exclusive deferral" (callee period excluded from the count-down); that
`leave` abandons the loop's in-flight speculative state; which of
reissue/completion/hybrid spiller models any given member uses.

**Could not determine:** exact current ISA encoding details for `load`
deferral vs. tag operands (the millcomputing.com wiki is offline); whether
cycle-counted (non-pickup) loads cross *taken argless branches* in current
specializer practice or only in principle; any comp.arch detail beyond the
forum reposts; the exact original wording of the wiki's metadata and
scratchpad pages, which are 404 and reachable only through a search index
(§3).

## Sources

- [The Belt thread](https://millcomputing.com/forum/d/250-the-belt) (posts #402, #1574–1596, #3229, #3536)
- [Execution thread](https://millcomputing.com/forum/d/634-execution) (posts #970–982, #1680)
- [Control flow divergence and The Belt](https://millcomputing.com/forum/d/3314-control-flow-divergence-and-the-belt) (posts #3316–3336)
- [Loop pipelining and aliasing](https://millcomputing.com/forum/d/1174-loop-pipelining-and-aliasing) (posts #1180–1181)
- [Pipelining](https://millcomputing.com/forum/d/1211-pipelining) (posts #1249, #1251, #1387, #1545)
- [Memory](https://millcomputing.com/forum/d/251-memory) (posts #324–329, #2142–2145)
- [spiller work optimisation](https://millcomputing.com/forum/d/2111-spiller-work-optimisation) (post #2113)
- [Grab bag of questions](https://millcomputing.com/forum/d/3772-grab-bag-of-questions) (posts #3816, #3817, #3830)
- [Deferred loads across control flow](https://millcomputing.com/forum/d/1909-deferred-loads-across-control-flow)
- [US Patent 9,690,581](https://www.freepatentsonline.com/9690581.html)
- [Mill architecture (HandWiki mirror of Wikipedia)](https://handwiki.org/wiki/Mill_architecture) — the metadata, NaR and None quotes in §3
- Mill wiki pages [Metadata](http://millcomputing.com/wiki/Metadata), [Scratchpad](http://millcomputing.com/wiki/Scratchpad) and [Speculation](http://millcomputing.com/wiki/Speculation) — **404 as of 2026-08**; §3 quotes them through a search index
- [Introduction to the Mill CPU Programming Model](https://millcomputing.com/topic/introduction-to-the-mill-cpu-programming-model-2/) — also 404; same caveat
- [Veedrac, "To reinvent the processor"](https://medium.com/@veedrac/to-reinvent-the-processor-671139a4a034)
