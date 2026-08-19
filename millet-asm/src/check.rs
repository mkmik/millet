//! The static belt model (PRD §3.2) and the checks of PRD §9.3.
//!
//! Belt occupancy is a static property of the instruction stream, so each EBB
//! can be checked independently starting from its declared entry arity. The
//! model never *invents* belt offsets — it only verifies the ones written.
//!
//! Error codes:
//!
//! | code | check |
//! |------|-------|
//! | E1 | belt reference names a position holding no live value, or a `%name` \
//!        that is not on the belt here |
//! | E2 | EBB entry arity not satisfied on a predecessor edge |
//! | E3 | op not legal in its slot |
//! | E4 | read of a position no value has reached yet, with ops still in flight |
//! | E5 | `conform` > 6 positions / bad `rescue` mask |
//! | E6 | call/return arity disagrees with the declaration |
//! | E7 | scratchpad slot out of range |
//! | E8 | (warning) store overlaps an in-flight load's delay window |
//! | E9 | operation still in flight at a control transfer out of an EBB |

use std::collections::HashMap;

use millet_core::isa::{BrKind, Op, Slot};
use millet_core::{Config, Image};

use crate::asm::{NAMED, SrcOp};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diag {
    pub code: &'static str,
    pub line: usize,
    pub msg: String,
    pub warning: bool,
}

impl Diag {
    pub fn err(code: &'static str, line: usize, msg: String) -> Diag {
        Diag {
            code,
            line,
            msg,
            warning: false,
        }
    }
    pub fn warn(code: &'static str, line: usize, msg: String) -> Diag {
        Diag {
            code,
            line,
            msg,
            warning: true,
        }
    }
    pub fn render(&self, path: &str) -> String {
        let kind = if self.warning { "warning" } else { "error" };
        if self.line == 0 {
            format!("{path}: {kind}[{}]: {}", self.code, self.msg)
        } else {
            format!("{path}:{}: {kind}[{}]: {}", self.line, self.code, self.msg)
        }
    }
}

/// The assembler's prediction of belt liveness at the entry of each bundle,
/// used for differential testing against the simulator's trace.
#[derive(Clone, Debug, Default)]
pub struct Prediction {
    pub live: Vec<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    id: u32,
    /// Index into `Ctx::names`, for a value written `-> name`.
    name: Option<u32>,
}

#[derive(Clone, Debug)]
struct Pend {
    retire_at: usize,
    issue: usize,
    slot: Slot,
    n: usize,
    line: usize,
    what: String,
    /// Names for the values this op will drop, in drop order.
    names: Vec<Option<u32>>,
    /// For loads: (address cell id, low byte offset, high byte offset).
    load_range: Option<(u32, i64, i64)>,
}

struct Ctx<'a> {
    cfg: &'a Config,
    image: &'a Image,
    ebb_names: &'a [String],
    func_names: &'a [String],
    diags: Vec<Diag>,
    next_id: u32,
    /// Interned value names; a `Cell` carries an index into this.
    names: Vec<String>,
    name_ids: HashMap<String, u32>,
}

impl Ctx<'_> {
    fn fresh(&mut self, name: Option<u32>) -> Cell {
        self.next_id += 1;
        Cell {
            id: self.next_id,
            name,
        }
    }
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.name_ids.get(s) {
            return i;
        }
        self.names.push(s.to_string());
        let i = self.names.len() as u32 - 1;
        self.name_ids.insert(s.to_string(), i);
        i
    }
    fn err(&mut self, code: &'static str, line: usize, msg: String) {
        self.diags.push(Diag::err(code, line, msg));
    }
    fn warn(&mut self, code: &'static str, line: usize, msg: String) {
        self.diags.push(Diag::warn(code, line, msg));
    }
}

type Belt = Vec<Option<Cell>>;

fn live_mask(belt: &Belt) -> u16 {
    let mut m = 0u16;
    for (i, c) in belt.iter().enumerate() {
        if c.is_some() {
            m |= 1 << i;
        }
    }
    m
}

/// Checks the belt discipline of every EBB, and — the same walk, since the
/// answer is the same one — rewrites `%name` operands in `bundles` to the
/// positions they stand for.
pub fn check(
    image: &Image,
    bundles: &mut [crate::asm::SrcBundle],
    ebb_names: &[String],
    ebb_args: &[Vec<String>],
    func_names: &[String],
    cfg: &Config,
) -> (Vec<Diag>, Prediction) {
    let mut ctx = Ctx {
        cfg,
        image,
        ebb_names,
        func_names,
        diags: Vec::new(),
        next_id: 0,
        names: Vec::new(),
        name_ids: HashMap::new(),
    };
    let mut pred = Prediction {
        live: vec![0; bundles.len()],
    };

    // Which function each EBB belongs to: the most recent function entry.
    let mut func_of_ebb = vec![usize::MAX; image.ebbs.len()];
    let mut cur = usize::MAX;
    for e in 0..image.ebbs.len() {
        if let Some(f) = image.funcs.iter().position(|f| f.ebb as usize == e) {
            cur = f;
        }
        func_of_ebb[e] = cur;
    }

    for (ei, ebb) in image.ebbs.iter().enumerate() {
        let start = ebb.bundle as usize;
        let end = image
            .ebbs
            .iter()
            .map(|e| e.bundle as usize)
            .filter(|b| *b > start)
            .min()
            .unwrap_or(bundles.len());
        let args = ebb_args.get(ei).map(Vec::as_slice).unwrap_or(&[]);
        check_ebb(
            &mut ctx,
            ei,
            start,
            end,
            bundles,
            args,
            &func_of_ebb,
            &mut pred,
        );
    }

    (ctx.diags, pred)
}

#[allow(clippy::too_many_arguments)]
fn check_ebb(
    ctx: &mut Ctx,
    ei: usize,
    start: usize,
    end: usize,
    bundles: &mut [crate::asm::SrcBundle],
    args: &[String],
    func_of_ebb: &[usize],
    pred: &mut Prediction,
) {
    let cfg = *ctx.cfg;
    let arity = ctx.image.ebbs[ei].arity as usize;
    let mut belt: Belt = vec![None; cfg.belt];
    for i in 0..arity.min(cfg.belt) {
        let name = args.get(i).map(|n| ctx.intern(n));
        belt[i] = Some(ctx.fresh(name));
    }
    let mut pending: Vec<Pend> = Vec::new();
    let ebb_name = ctx.ebb_names[ei].clone();
    // Bundle in which each name was last shifted off the belt, so a read that
    // just missed it can say by how much.
    let mut gone: HashMap<u32, usize> = HashMap::new();

    for t in start..end {
        pred.live[t] = live_mask(&belt);
        let mut retiring: Vec<Pend> = Vec::new();
        let entry_names = live_names(&belt);
        // Named by an op in *this* bundle: no read can see those yet, and
        // saying that beats "no such name".
        let dropped_here: Vec<u32> = bundles[t]
            .ops
            .iter()
            .flatten()
            .flat_map(|so| so.results.iter())
            .map(|n| ctx.intern(n))
            .collect();

        for slot in Slot::ALL {
            let Some(so) = bundles[t].ops[slot.index()].as_mut() else {
                continue;
            };
            // Reshapes read the post-drop belt (PRD §7.2), so their names
            // resolve down with the rest of the reshape.
            let named_ok =
                so.op.is_reshape() || resolve_names(ctx, &belt, so, &gone, &dropped_here, t);
            let (op, line) = (&so.op, so.line);

            if !op.allowed_in(slot) {
                ctx.err(
                    "E3",
                    line,
                    format!("`{}` is not legal in slot {}", mnemonic(op), slot.tag()),
                );
            }

            // Reshapes read the post-drop belt (PRD §7.2); handled below.
            if !op.is_reshape() && named_ok {
                for r in op.belt_reads() {
                    read_check(ctx, &belt, &pending, r, line, op);
                }
            }

            // Op-specific static checks.
            let mut callee_nres = None;
            match op {
                Op::Spill { slot: s, .. } | Op::Fill { slot: s } => {
                    if *s as usize >= cfg.scratch {
                        ctx.err(
                            "E7",
                            line,
                            format!("scratchpad slot s{s} out of range (0..{})", cfg.scratch - 1),
                        );
                    }
                }
                Op::Call { target, args } => match ctx.image.funcs.get(*target as usize) {
                    Some(f) => {
                        if f.arity as usize != args.len() {
                            ctx.err(
                                "E6",
                                line,
                                format!(
                                    "`{}` declares arity {} but the call passes {} argument(s)",
                                    ctx.func_names[*target as usize],
                                    f.arity,
                                    args.len()
                                ),
                            );
                        }
                        callee_nres = Some(f.nres as usize);
                    }
                    None => ctx.err("E6", line, "call target index out of range".into()),
                },
                // The callee is unknown, so the arity/nres agreement is the
                // simulator's to enforce; statically, `nres` is simply taken
                // at its word so the belt stays predictable.
                Op::CallI { nres, args, .. } => {
                    if args.len() > millet_core::isa::MAX_ARGS
                        || *nres as usize > millet_core::isa::MAX_ARGS
                    {
                        ctx.err(
                            "E6",
                            line,
                            format!(
                                "`calli` passes {} argument(s) and takes {nres} result(s); max 3 each",
                                args.len()
                            ),
                        );
                    }
                    callee_nres = Some(*nres as usize);
                }
                Op::Retn { res } => {
                    let fi = func_of_ebb[ei];
                    if fi == usize::MAX {
                        ctx.err("E6", line, "`retn` outside any function".into());
                    } else {
                        let want = ctx.image.funcs[fi].nres as usize;
                        if want != res.len() {
                            ctx.err(
                                "E6",
                                line,
                                format!(
                                    "`{}` declares {} result(s) but this `retn` returns {}",
                                    ctx.func_names[fi],
                                    want,
                                    res.len()
                                ),
                            );
                        }
                    }
                }
                Op::Conform(list) => {
                    if list.is_empty() || list.len() > millet_core::isa::MAX_CONFORM {
                        ctx.err(
                            "E5",
                            line,
                            format!("conform takes 1..6 positions, found {}", list.len()),
                        );
                    }
                }
                Op::Rescue(mask) => {
                    if (*mask as u32) >> cfg.belt != 0 {
                        ctx.err(
                            "E5",
                            line,
                            format!(
                                "rescue mask {mask:#06x} selects positions beyond b{}",
                                cfg.belt - 1
                            ),
                        );
                    }
                }
                Op::Store {
                    addr,
                    offset,
                    width,
                    ..
                } => {
                    // E8: does this store land inside an in-flight load's window?
                    let sid = belt.get(*addr as usize).and_then(|c| *c).map(|c| c.id);
                    if let Some(sid) = sid {
                        let (slo, shi) = (*offset as i64, *offset as i64 + *width as i64);
                        let hits: Vec<(usize, String)> = pending
                            .iter()
                            .filter_map(|p| {
                                let (lid, llo, lhi) = p.load_range?;
                                if lid == sid && slo < lhi && llo < shi {
                                    Some((p.line, p.what.clone()))
                                } else {
                                    None
                                }
                            })
                            .collect();
                        for (lline, what) in hits {
                            ctx.warn(
                                "E8",
                                line,
                                format!(
                                    "this store overlaps the delay window of the {what} issued at line {lline}; \
                                     that load sampled memory at issue and will not see it"
                                ),
                            );
                        }
                    }
                }
                _ => {}
            }

            // Schedule the results.
            let n = op.n_results(callee_nres);
            let names: Vec<Option<u32>> = match so.results.len() {
                0 => vec![None; n],
                k if k == n => so.results.iter().map(|r| Some(ctx.intern(r))).collect(),
                k => {
                    ctx.err(
                        "E0",
                        line,
                        format!(
                            "`{}` drops {n} value(s) but {k} name(s) follow `->`",
                            mnemonic(op)
                        ),
                    );
                    vec![None; n]
                }
            };
            if n > 0 {
                let lat = op.latency().unwrap_or(1) as usize;
                let load_range = match op {
                    Op::Load {
                        addr,
                        offset,
                        width,
                        ..
                    } => belt
                        .get(*addr as usize)
                        .and_then(|c| *c)
                        .map(|c| (c.id, *offset as i64, *offset as i64 + *width as i64)),
                    _ => None,
                };
                let p = Pend {
                    retire_at: t + lat - 1,
                    issue: t,
                    slot,
                    n,
                    line,
                    what: mnemonic(op).to_string(),
                    names,
                    load_range,
                };
                if p.retire_at == t {
                    retiring.push(p);
                } else {
                    pending.push(p);
                }
            }
        }

        // Retire: everything scheduled for the end of this bundle, in
        // (issuing bundle, slot) order (PRD §3.1).
        let mut i = 0;
        while i < pending.len() {
            if pending[i].retire_at == t {
                retiring.push(pending.remove(i));
            } else {
                i += 1;
            }
        }
        retiring.sort_by_key(|p| (p.issue, p.slot));
        for p in &retiring {
            for k in 0..p.n {
                let c = ctx.fresh(p.names.get(k).copied().flatten());
                belt.pop();
                belt.insert(0, Some(c));
            }
        }

        // Reshape, applied last against the post-drop belt (PRD §7.2) — which
        // is also the belt its `%name` operands resolve against.
        if let Some(so) = bundles[t].ops[Slot::F.index()].as_mut()
            && so.op.is_reshape()
            && !resolve_names(ctx, &belt, so, &gone, &[], t)
        {
            return;
        }
        if let Some(so) = &bundles[t].ops[Slot::F.index()] {
            match &so.op {
                Op::Conform(list) => {
                    let mut nb: Belt = vec![None; cfg.belt];
                    for (i, p) in list.iter().enumerate() {
                        match belt.get(*p as usize).and_then(|c| *c) {
                            Some(c) => nb[i] = Some(c),
                            None => ctx.err(
                                "E1",
                                so.line,
                                format!("conform names b{p}, which holds no live value after this bundle's drops"),
                            ),
                        }
                    }
                    belt = nb;
                }
                Op::Rescue(mask) => {
                    let mut nb: Belt = vec![None; cfg.belt];
                    let mut k = 0;
                    for i in 0..cfg.belt {
                        if mask & (1 << i) == 0 {
                            continue;
                        }
                        match belt[i] {
                            Some(c) => {
                                nb[k] = Some(c);
                                k += 1;
                            }
                            None => {
                                ctx.err(
                                    "E1",
                                    so.line,
                                    format!("rescue selects b{i}, which holds no live value after this bundle's drops"),
                                );
                                k += 1;
                            }
                        }
                    }
                    belt = nb;
                }
                _ => {}
            }
        }

        for id in entry_names {
            if !belt.iter().flatten().any(|c| c.name == Some(id)) {
                gone.insert(id, t);
            }
        }

        // Control transfer out of this EBB.
        let fop = bundles[t].ops[Slot::F.index()].as_ref();
        let is_last = t + 1 == end;
        let mut terminated = false;

        if let Some(so) = fop {
            match &so.op {
                Op::Br { kind, target, .. } => {
                    edge_check(
                        ctx,
                        &belt,
                        &pending,
                        Some(*target as usize),
                        so.line,
                        &ebb_name,
                    );
                    terminated = *kind == BrKind::Always;
                }
                // The target set of a `bri` is not known statically, so E2
                // cannot run on this edge — the simulator enforces the entry
                // arity instead. E9 still applies.
                Op::BrI { .. } => {
                    edge_check(ctx, &belt, &pending, None, so.line, &ebb_name);
                    terminated = true;
                }
                Op::Retn { .. } => {
                    if !pending.is_empty() {
                        let what = describe_pending(&pending);
                        ctx.warn(
                            "E9",
                            so.line,
                            format!(
                                "`retn` with {what} still in flight; their results are discarded"
                            ),
                        );
                    }
                    terminated = true;
                }
                Op::Halt => terminated = true,
                Op::Sys(0) => terminated = true,
                _ => {}
            }
        }

        if terminated {
            if !is_last {
                ctx.warn(
                    "E0",
                    bundles[t + 1].line,
                    "unreachable bundle: the previous bundle ends control flow".into(),
                );
            }
            return;
        }

        if is_last {
            // Fall-through into the next EBB is a real edge (PRD §4).
            if t + 1 >= bundles.len() {
                ctx.err(
                    "E0",
                    bundles[t].line,
                    format!(
                        "EBB `{ebb_name}` runs off the end of the program without a terminator"
                    ),
                );
                return;
            }
            let target = ctx
                .image
                .ebb_at(t as u32 + 1)
                .expect("bundle after an EBB must start an EBB");
            let line = fop.map(|o| o.line).unwrap_or(bundles[t].line);
            edge_check(ctx, &belt, &pending, Some(target), line, &ebb_name);
            return;
        }
    }
}

fn live_names(belt: &Belt) -> Vec<u32> {
    belt.iter().flatten().filter_map(|c| c.name).collect()
}

/// The most recently dropped live value carrying `id`; a name written twice
/// shadows the earlier one.
fn find_name(belt: &Belt, id: u32) -> Option<usize> {
    belt.iter()
        .enumerate()
        .filter_map(|(i, c)| (*c).filter(|c| c.name == Some(id)).map(|c| (i, c.id)))
        .max_by_key(|(_, cid)| *cid)
        .map(|(i, _)| i)
}

/// Rewrite this op's `%name` operands into the positions they hold right now.
/// False if one of them did not resolve, in which case the caller skips the
/// position checks — they would be about a position nobody wrote.
fn resolve_names(
    ctx: &mut Ctx,
    belt: &Belt,
    so: &mut SrcOp,
    gone: &HashMap<u32, usize>,
    dropped_here: &[u32],
    t: usize,
) -> bool {
    if so.names.is_empty() {
        return true;
    }
    let (line, names) = (so.line, &so.names);
    let mut ok = true;
    each_read(&mut so.op, &mut |p: &mut u8| {
        if *p < NAMED {
            return;
        }
        let name = &names[(*p & !NAMED) as usize];
        let id = ctx.intern(name);
        if let Some(i) = find_name(belt, id) {
            *p = i as u8;
            return;
        }
        ok = false;
        let mut msg = if dropped_here.contains(&id) {
            format!(
                "`%{name}` is dropped by this same bundle; every read sees the belt \
                 as it stood at bundle entry"
            )
        } else if let Some(g) = gone.get(&id) {
            format!("`%{name}` fell off the belt {} bundle(s) ago", t - g)
        } else {
            format!("no value named `%{name}` is in scope here")
        };
        let live: Vec<String> = live_names(belt)
            .iter()
            .map(|n| format!("%{}", ctx.names[*n as usize]))
            .collect();
        if !live.is_empty() {
            msg += &format!("; live here: {}", live.join(" "));
        }
        ctx.err("E1", line, msg);
    });
    ok
}

/// Every belt read operand, mutably. `conform`'s list counts as reads: they
/// are just taken after this bundle's drops.
fn each_read(op: &mut Op, visit: &mut dyn FnMut(&mut u8)) {
    match op {
        Op::Alu { a, b, .. } => {
            visit(a);
            visit(b);
        }
        Op::Pick { c, t, f } => {
            visit(c);
            visit(t);
            visit(f);
        }
        Op::IsNar { a } | Op::IsNone { a } => visit(a),
        Op::Load { addr, .. } => visit(addr),
        Op::Store { addr, val, .. } => {
            visit(addr);
            visit(val);
        }
        Op::Spill { val, .. } => visit(val),
        Op::Br { cond, .. } => visit(cond),
        Op::BrI { target } => visit(target),
        Op::Call { args, .. } => {
            for a in args {
                visit(a);
            }
        }
        Op::CallI { target, args, .. } => {
            visit(target);
            for a in args {
                visit(a);
            }
        }
        Op::Retn { res } => {
            for r in res {
                visit(r);
            }
        }
        Op::Conform(list) => {
            for p in list {
                visit(p);
            }
        }
        _ => {}
    }
}

fn mnemonic(op: &Op) -> String {
    millet_core::isa::format_op(op, &|i| format!("ebb{i}"), &|i| format!("fn{i}"))
        .split_whitespace()
        .next()
        .unwrap_or("?")
        .to_string()
}

fn describe_pending(pending: &[Pend]) -> String {
    let names: Vec<String> = pending
        .iter()
        .map(|p| format!("`{}` from line {}", p.what, p.line))
        .collect();
    names.join(", ")
}

fn read_check(ctx: &mut Ctx, belt: &Belt, pending: &[Pend], pos: u8, line: usize, op: &Op) {
    let p = pos as usize;
    if p >= belt.len() {
        ctx.err(
            "E1",
            line,
            format!("b{p} is beyond this machine's {}-position belt", belt.len()),
        );
        return;
    }
    if belt[p].is_some() {
        return;
    }
    if let Some(pend) = pending.first() {
        ctx.err(
            "E4",
            line,
            format!(
                "`{}` reads b{p}, which holds no value yet — `{}` from line {} is still in flight \
                 and retires at the end of bundle +{}",
                mnemonic(op),
                pend.what,
                pend.line,
                pend.retire_at - pend.issue
            ),
        );
    } else {
        ctx.err(
            "E1",
            line,
            format!(
                "`{}` reads b{p}, which holds no live value (never dropped since this EBB's entry, \
                 or truncated away by the entry arity)",
                mnemonic(op)
            ),
        );
    }
}

/// `target` is `None` for an indirect branch, whose successor set is unknown:
/// only the in-flight check (E9) applies.
fn edge_check(
    ctx: &mut Ctx,
    belt: &Belt,
    pending: &[Pend],
    target: Option<usize>,
    line: usize,
    from: &str,
) {
    if !pending.is_empty() {
        let what = describe_pending(pending);
        ctx.err(
            "E9",
            line,
            format!(
                "control leaves EBB `{from}` with {what} still in flight; \
                 every operation must retire before a control transfer"
            ),
        );
    }
    let Some(target) = target else { return };
    let Some(te) = ctx.image.ebbs.get(target) else {
        ctx.err("E2", line, "branch target index out of range".into());
        return;
    };
    let k = te.arity as usize;
    let name = ctx.ebb_names[target].clone();
    for i in 0..k.min(belt.len()) {
        if belt[i].is_none() {
            ctx.err(
                "E2",
                line,
                format!(
                    "edge to `{name}` must deliver {k} value(s) in b0..b{}, but b{i} holds no live value",
                    k - 1
                ),
            );
            return;
        }
    }
    // Values above the arity are *not* an error: truncation is
    // machine-enforced (PRD §4), and the standard loop idiom deliberately
    // leaves the branch condition sitting just above the carried set —
    // the reshape has to happen in the bundle before the branch, so the
    // condition cannot be anywhere else.
    let _ = name;
}
