//! millet-sim — a bundle-accurate, deterministic Millet simulator.
//!
//! One bundle per cycle, never stalls. All operand reads observe the belt as
//! it exists at bundle entry; all drops happen at bundle end in
//! (issuing bundle, slot) order; `conform`/`rescue` rewrite the post-drop
//! belt last (PRD §3.1, §5.3, §7.2).
//!
//! Every belt and scratchpad value carries its metadata tag (PRD §8.6): a
//! failed operation drops a NaR rather than stopping the machine, and the
//! fault surfaces at the store, branch or `sys` that realizes it.

use std::collections::HashMap;
use std::io::{Read, Write};

use millet_core::isa::{AluOp, BrKind, Op, Slot, decode, format_op};
use millet_core::{Config, Image, NarKind, Tag, Value};

const PAGE: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// `sys 0` or a `retn` from the entry function.
    Exit(i32),
    /// `halt`: abnormal stop.
    Halt,
    Fault(String),
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub bundles: u64,
    pub slot_ops: [u64; 4],
    pub max_depth: usize,
    pub calls: u64,
}

impl Stats {
    pub fn occupancy(&self) -> f64 {
        if self.bundles == 0 {
            return 0.0;
        }
        self.slot_ops.iter().sum::<u64>() as f64 / (self.bundles as f64 * 4.0)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub trace: bool,
    pub trace_json: bool,
    /// Safety net for runaway programs; 0 means unlimited.
    pub max_bundles: u64,
}

#[derive(Clone, Debug)]
struct Pending {
    retire_at: usize,
    issue: usize,
    slot: Slot,
    vals: Vec<Value>,
}

#[derive(Clone, Debug)]
struct Frame {
    belt: Vec<Value>,
    live: u16,
    scratch: Vec<Value>,
    pending: Vec<Pending>,
    /// Bundles executed in this frame; latencies are frame-local (PRD §6.2).
    time: usize,
    pc: usize,
}

impl Frame {
    fn new(cfg: &Config, pc: usize) -> Frame {
        // Nothing has been dropped and nothing spilled, so every position holds
        // a None — the metadata replacement for v0's reads-as-zero (PRD §12.8).
        Frame {
            belt: vec![Value::NONE; cfg.belt],
            live: 0,
            scratch: vec![Value::NONE; cfg.scratch],
            pending: Vec::new(),
            time: 0,
            pc,
        }
    }

    fn drop_value(&mut self, v: Value) {
        self.belt.pop();
        self.belt.insert(0, v);
        self.live = ((self.live << 1) | 1) & mask(self.belt.len());
    }

    /// " (in flight: +2, +5)" — how many bundles until each pending result
    /// lands. Empty when nothing is outstanding.
    fn pending_summary(&self, now: usize) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let mut when: Vec<String> = self
            .pending
            .iter()
            .map(|p| format!("+{}", p.retire_at as i64 - now as i64))
            .collect();
        when.sort();
        format!("  (in flight: {})", when.join(", "))
    }

    fn truncate(&mut self, k: usize) {
        for i in k..self.belt.len() {
            self.belt[i] = Value::NONE;
        }
        self.live &= mask(k);
    }
}

fn mask(k: usize) -> u16 {
    if k >= 16 { u16::MAX } else { (1u16 << k) - 1 }
}

/// The NaR and None masks over a belt's live prefix — dead positions all read
/// as None and liveness already says so.
fn tag_masks(belt: &[Value], live: u16) -> (u16, u16) {
    let (mut nar, mut none) = (0u16, 0u16);
    for (i, v) in belt.iter().enumerate() {
        match v.tag {
            _ if live & (1 << i) == 0 => {}
            Tag::Nar => nar |= 1 << i,
            Tag::None => none |= 1 << i,
            Tag::Val => {}
        }
    }
    (nar, none)
}

/// The diagnostic for a non-speculable op meeting a poisoned operand.
fn realizes(what: &str, pc: usize, v: Value) -> Stop {
    Stop::Fault(format!(
        "the {what} in bundle {pc} consumed {}",
        v.describe()
    ))
}

/// One executed-bundle record, kept for differential testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TracePoint {
    pub bundle: usize,
    pub depth: usize,
    /// Belt liveness at bundle entry.
    pub live: u16,
}

pub struct Machine<'a> {
    cfg: Config,
    img: &'a Image,
    mem: HashMap<u64, Vec<u8>>,
    frames: Vec<Frame>,
    pub stats: Stats,
    pub points: Vec<TracePoint>,
    opts: Options,
    out: &'a mut dyn Write,
    log: &'a mut dyn Write,
    /// Bytes this bundle wrote to fd 1, for the JSON trace.
    // ponytail: fd 1 only; fd 2 shares the terminal with the trace anyway.
    out_log: Vec<u8>,
}

impl<'a> Machine<'a> {
    pub fn new(
        img: &'a Image,
        cfg: Config,
        opts: Options,
        out: &'a mut dyn Write,
        log: &'a mut dyn Write,
    ) -> Machine<'a> {
        let mut m = Machine {
            cfg,
            img,
            mem: HashMap::new(),
            frames: Vec::new(),
            stats: Stats::default(),
            points: Vec::new(),
            opts,
            out,
            log,
            out_log: Vec::new(),
        };
        for seg in &img.data {
            m.store_bytes(seg.addr, &seg.bytes);
        }
        m
    }

    // -- memory -----------------------------------------------------------

    fn store_bytes(&mut self, addr: u64, bytes: &[u8]) {
        for (i, b) in bytes.iter().enumerate() {
            let a = addr.wrapping_add(i as u64);
            let page = self
                .mem
                .entry(a / PAGE)
                .or_insert_with(|| vec![0; PAGE as usize]);
            page[(a % PAGE) as usize] = *b;
        }
    }

    fn load_bytes(&self, addr: u64, len: usize) -> Result<Vec<u8>, Stop> {
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let a = addr.wrapping_add(i as u64);
            match self.mem.get(&(a / PAGE)) {
                Some(p) => out.push(p[(a % PAGE) as usize]),
                None => {
                    return Err(Stop::Fault(format!(
                        "load from unbacked address {a:#018x} (no .data section covers it, and nothing has been stored there)"
                    )));
                }
            }
        }
        Ok(out)
    }

    // -- execution --------------------------------------------------------

    pub fn run(&mut self) -> Stop {
        let f = &self.img.funcs[self.img.entry as usize];
        if f.arity != 0 {
            return Stop::Fault("entry function must take 0 arguments".into());
        }
        let pc = self.img.ebbs[f.ebb as usize].bundle as usize;
        self.frames.push(Frame::new(&self.cfg, pc));
        match self.run_frame() {
            Ok(_) => Stop::Exit(0),
            Err(s) => s,
        }
    }

    /// Run the topmost frame until it returns; pops it on the way out.
    fn run_frame(&mut self) -> Result<Vec<Value>, Stop> {
        loop {
            if self.opts.max_bundles != 0 && self.stats.bundles >= self.opts.max_bundles {
                return Err(Stop::Fault(format!(
                    "bundle limit of {} reached (runaway program?)",
                    self.opts.max_bundles
                )));
            }
            if let Some(res) = self.step()? {
                self.frames.pop();
                return Ok(res);
            }
        }
    }

    /// Execute one bundle of the topmost frame. `Some(results)` means the
    /// frame executed a `retn`.
    fn step(&mut self) -> Result<Option<Vec<Value>>, Stop> {
        let img = self.img;
        let depth = self.frames.len() - 1;
        let pc = self.frames[depth].pc;
        if pc >= img.bundles.len() {
            return Err(Stop::Fault("execution ran off the end of the code".into()));
        }
        let words = img.bundles[pc];
        let mut ops: Vec<Option<Op>> = Vec::with_capacity(4);
        for slot in Slot::ALL {
            let w = words[slot.index()];
            match decode(w) {
                Ok(Op::Nop) => ops.push(None),
                Ok(op) => {
                    if !op.allowed_in(slot) {
                        return Err(Stop::Fault(format!(
                            "bundle {pc}: op in slot {} is not legal there",
                            slot.tag()
                        )));
                    }
                    ops.push(Some(op))
                }
                Err(e) => return Err(Stop::Fault(format!("bundle {pc}: {e}"))),
            }
        }

        let t = self.frames[depth].time;
        let entry_belt = self.frames[depth].belt.clone();
        let live_in = self.frames[depth].live;
        self.points.push(TracePoint {
            bundle: pc,
            depth,
            live: live_in,
        });
        self.stats.bundles += 1;
        // Taken at entry: a bundle containing a `call` finishes after the whole
        // callee has run, so `stats.bundles` is no longer this bundle's cycle.
        let cycle = self.stats.bundles - 1;
        self.out_log.clear();
        if self.opts.trace {
            let ebb = img
                .ebb_containing(pc as u32)
                .map(|i| img.ebb_label(i))
                .unwrap_or_default();
            let inflight = self.frames[depth].pending_summary(t);
            let _ = writeln!(
                self.log,
                "[{cycle:>6}] frame {depth} bundle {pc} in {ebb}{inflight}"
            );
        }

        let b = |p: u8| entry_belt[p as usize];
        let mut stores: Vec<(u64, u8, u64)> = Vec::new();
        let mut skipped: Vec<u64> = Vec::new();
        let mut spills: Vec<(usize, Value)> = Vec::new();

        // -- slots A0, A1, M -------------------------------------------------
        for slot in [Slot::A0, Slot::A1, Slot::M] {
            let Some(op) = ops[slot.index()].clone() else {
                continue;
            };
            self.stats.slot_ops[slot.index()] += 1;
            if self.opts.trace {
                let line = format!("       {:<3} {}", slot.tag(), self.render(&op));
                let _ = writeln!(self.log, "{line}");
            }
            let val = match &op {
                Op::Alu { op: a, a: x, b: y } => Some(match Value::poison(&[b(*x), b(*y)]) {
                    Some(p) => p,
                    None => alu(*a, b(*x).bits, b(*y).bits, pc),
                }),
                // `pick` is what makes speculation pay off: only the selected
                // operand's tag survives, so the path not taken cannot poison.
                Op::Pick { c, t: x, f: y } => Some(match b(*c).tag {
                    Tag::Val if b(*c).bits != 0 => b(*x),
                    Tag::Val => b(*y),
                    _ => b(*c),
                }),
                Op::Con { imm } => Some(Value::val(*imm as i64 as u64)),
                Op::NoneVal => Some(Value::NONE),
                Op::IsNar { a } => Some(Value::val((b(*a).tag == Tag::Nar) as u64)),
                Op::IsNone { a } => Some(Value::val((b(*a).tag == Tag::None) as u64)),
                Op::Load {
                    addr,
                    offset,
                    width,
                    sext,
                    ..
                } => Some(match Value::poison(&[b(*addr)]) {
                    // A load is speculable: a poisoned address propagates and a
                    // load that cannot be satisfied drops a NaR (PRD §8.6).
                    Some(p) => p,
                    None => {
                        let a = b(*addr).bits.wrapping_add(*offset as i64 as u64);
                        match self.load_bytes(a, *width as usize) {
                            Ok(bytes) => Value::val(assemble_word(&bytes, *sext)),
                            Err(_) => Value::nar(NarKind::Unbacked, pc),
                        }
                    }
                }),
                Op::Store {
                    addr,
                    offset,
                    width,
                    val,
                } => {
                    // Realization: a None suppresses the store, a NaR raises the
                    // fault it has been carrying since it was created.
                    let a = b(*addr).bits.wrapping_add(*offset as i64 as u64);
                    match Value::poison(&[b(*addr), b(*val)]) {
                        None => stores.push((a, *width, b(*val).bits)),
                        Some(p) if p.tag == Tag::None => skipped.push(a),
                        Some(p) => return Err(realizes("store", pc, p)),
                    }
                    None
                }
                Op::Spill { slot: s, val } => {
                    spills.push((*s as usize, b(*val)));
                    None
                }
                Op::Fill { slot: s } => Some(self.frames[depth].scratch[*s as usize]),
                other => {
                    return Err(Stop::Fault(format!("bundle {pc}: unexpected op {other:?}")));
                }
            };
            if let Some(v) = val {
                let lat = op.latency().unwrap_or(1) as usize;
                self.frames[depth].pending.push(Pending {
                    retire_at: t + lat - 1,
                    issue: t,
                    slot,
                    vals: vec![v],
                });
            }
        }

        // Stores and spills take effect at the issuing bundle's end; nothing
        // in this bundle can observe them (loads and fills sampled at issue).
        for (a, w, v) in &stores {
            let bytes = v.to_le_bytes();
            self.store_bytes(*a, &bytes[..*w as usize]);
            if self.opts.trace {
                let line = format!("       mem  [{a:#x}] <- {v} ({w}B)");
                let _ = writeln!(self.log, "{line}");
            }
        }
        if self.opts.trace {
            for a in &skipped {
                let _ = writeln!(self.log, "       mem  [{a:#x}] <- (None: store suppressed)");
            }
        }
        // The scratchpad holds operands, not bytes: a spilled NaR fills back as
        // the same NaR (PRD §8.6).
        for (s, v) in &spills {
            self.frames[depth].scratch[*s] = *v;
            if self.opts.trace {
                let line = format!("       scr  s{s} <- {v}");
                let _ = writeln!(self.log, "{line}");
            }
        }

        // -- slot F ----------------------------------------------------------
        let mut branch_to: Option<usize> = None;
        let mut branch_indirect = false;
        let mut returning: Option<Vec<Value>> = None;
        let fop = ops[Slot::F.index()].clone();
        if let Some(op) = &fop {
            self.stats.slot_ops[Slot::F.index()] += 1;
            if self.opts.trace {
                let line = format!("       {:<3} {}", Slot::F.tag(), self.render(op));
                let _ = writeln!(self.log, "{line}");
            }
            // An indirect call resolves to its direct equivalent here, once the
            // belt has been read: the result count written at the call site
            // must match the callee's declaration, because the assembler
            // renumbered the belt by that count.
            let resolved;
            let op = match op {
                Op::CallI { nres, target, args } => {
                    // A target is control flow, and control flow is not
                    // speculable: a poisoned one resolves here or not at all.
                    if b(*target).is_poison() {
                        return Err(realizes("calli", pc, b(*target)));
                    }
                    let idx = b(*target).bits;
                    let t16 = u16::try_from(idx)
                        .map_err(|_| Stop::Fault(format!("calli target {idx} out of range")))?;
                    let f = img
                        .funcs
                        .get(t16 as usize)
                        .ok_or_else(|| Stop::Fault(format!("calli target {idx} out of range")))?;
                    if f.nres != *nres {
                        return Err(Stop::Fault(format!(
                            "calli declares {nres} result(s) but `{}` returns {}",
                            img.func_label(t16 as usize),
                            f.nres
                        )));
                    }
                    resolved = Op::Call {
                        target: t16,
                        args: args.clone(),
                    };
                    &resolved
                }
                other => other,
            };
            match op {
                Op::BrI { target } => {
                    if b(*target).is_poison() {
                        return Err(realizes("branch", pc, b(*target)));
                    }
                    branch_to = Some(b(*target).bits as usize);
                    branch_indirect = true;
                }
                Op::Br { kind, cond, target } => {
                    // Control flow is not speculable: a poisoned condition has
                    // to be resolved here or not at all.
                    if *kind != BrKind::Always && b(*cond).is_poison() {
                        return Err(realizes("branch", pc, b(*cond)));
                    }
                    let take = match kind {
                        BrKind::Always => true,
                        BrKind::IfTrue => b(*cond).bits != 0,
                        BrKind::IfFalse => b(*cond).bits == 0,
                    };
                    if take {
                        branch_to = Some(*target as usize);
                    }
                }
                Op::Call { target, args } => {
                    let f = img
                        .funcs
                        .get(*target as usize)
                        .ok_or_else(|| Stop::Fault("call target out of range".into()))?;
                    let (f_ebb, f_arity, f_nres) = (f.ebb, f.arity, f.nres);
                    if f_arity as usize != args.len() {
                        return Err(Stop::Fault(format!(
                            "call passes {} arguments to a function of arity {}",
                            args.len(),
                            f_arity
                        )));
                    }
                    if self.frames.len() >= 200_000 {
                        return Err(Stop::Fault("call depth exhausted".into()));
                    }
                    let entry = img.ebbs[f_ebb as usize].bundle as usize;
                    let nres = f_nres as usize;
                    let mut callee = Frame::new(&self.cfg, entry);
                    // Arguments drop in listed order: last-listed is b0.
                    for a in args {
                        callee.drop_value(b(*a));
                    }
                    self.frames.push(callee);
                    self.stats.calls += 1;
                    self.stats.max_depth = self.stats.max_depth.max(self.frames.len() - 1);
                    let res = self.run_frame()?;
                    if self.opts.trace {
                        let _ = writeln!(
                            self.log,
                            "[{:>6}] frame {depth} bundle {pc} (resumed)",
                            self.stats.bundles - 1
                        );
                    }
                    if res.len() != nres {
                        return Err(Stop::Fault(format!(
                            "callee returned {} values, declared {nres}",
                            res.len()
                        )));
                    }
                    if !res.is_empty() {
                        self.frames[depth].pending.push(Pending {
                            retire_at: t,
                            issue: t,
                            slot: Slot::F,
                            vals: res,
                        });
                    }
                }
                Op::Retn { res } => {
                    returning = Some(res.iter().map(|r| b(*r)).collect());
                }
                Op::Sys(code) => {
                    // IO is the other non-speculable op: it realizes whatever
                    // it is handed.
                    for p in op.belt_reads() {
                        if b(p).is_poison() {
                            return Err(realizes("sys", pc, b(p)));
                        }
                    }
                    let v = self.syscall(*code, b(0).bits, b(1).bits, b(2).bits)?;
                    if let Some(v) = v {
                        self.frames[depth].pending.push(Pending {
                            retire_at: t,
                            issue: t,
                            slot: Slot::F,
                            vals: vec![Value::val(v)],
                        });
                    }
                }
                Op::Halt => return Err(Stop::Halt),
                Op::Conform(_) | Op::Rescue(_) => {}
                other => return Err(Stop::Fault(format!("bundle {pc}: unexpected op {other:?}"))),
            }
        }

        // -- retire ----------------------------------------------------------
        let mut retiring: Vec<Pending> = Vec::new();
        let f = &mut self.frames[depth];
        let mut i = 0;
        while i < f.pending.len() {
            if f.pending[i].retire_at <= t {
                retiring.push(f.pending.remove(i));
            } else {
                i += 1;
            }
        }
        retiring.sort_by_key(|p| (p.issue, p.slot));
        let mut drops: Vec<(Value, Slot, usize)> = Vec::new();
        for p in &retiring {
            for v in &p.vals {
                self.frames[depth].drop_value(*v);
                drops.push((*v, p.slot, t - p.issue));
                if self.opts.trace {
                    let line = format!(
                        "       drop <- {v}  (slot {} issued at {:+})",
                        p.slot.tag(),
                        p.issue as i64 - t as i64
                    );
                    let _ = writeln!(self.log, "{line}");
                }
            }
        }

        // -- reshape (post-drop, applied last) --------------------------------
        match &fop {
            Some(Op::Conform(list)) => {
                let f = &mut self.frames[depth];
                let old = f.belt.clone();
                let mut nb = vec![Value::NONE; old.len()];
                for (i, p) in list.iter().enumerate() {
                    nb[i] = old[*p as usize];
                }
                f.belt = nb;
                f.live = mask(list.len());
            }
            Some(Op::Rescue(m)) => {
                let f = &mut self.frames[depth];
                let old = f.belt.clone();
                let mut nb = vec![Value::NONE; old.len()];
                let mut k = 0;
                for i in 0..old.len() {
                    if m & (1 << i) != 0 {
                        nb[k] = old[i];
                        k += 1;
                    }
                }
                f.belt = nb;
                f.live = mask(k);
            }
            _ => {}
        }

        if self.opts.trace {
            let f = &self.frames[depth];
            // Only the live prefix is interesting; everything past it is zero.
            let top = (0..f.belt.len()).rev().find(|i| f.live & (1 << i) != 0);
            let cells: Vec<String> = match top {
                None => vec!["(empty)".into()],
                Some(top) => (0..=top)
                    .map(|i| {
                        if f.live & (1 << i) != 0 {
                            format!("b{i}={}", f.belt[i])
                        } else {
                            format!("b{i}=-")
                        }
                    })
                    .collect(),
            };
            let _ = writeln!(self.log, "       belt {}", cells.join(" "));
        }
        if self.opts.trace_json {
            let f = &self.frames[depth];
            let nums = |vs: &[Value]| {
                vs.iter()
                    .map(|v| v.bits.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let mut rec = format!(
                "{{\"cycle\":{cycle},\"frame\":{depth},\"bundle\":{pc},\"live_in\":{live_in},\"live_out\":{},\"belt_in\":[{}],\"belt\":[{}]",
                f.live,
                nums(&entry_belt),
                nums(&f.belt),
            );
            // Everything below is a per-bundle effect; omitted when it did not
            // happen, which is most bundles.
            let list = |name: &str, items: Vec<String>| match items.is_empty() {
                true => String::new(),
                false => format!(",\"{name}\":[{}]", items.join(",")),
            };
            // Metadata as masks over the live prefix, in the shape `live_in` and
            // `live_out` already use. All-zero for a run that never poisons
            // anything, and then omitted entirely.
            let (nar_in, none_in) = tag_masks(&entry_belt, live_in);
            let (nar_out, none_out) = tag_masks(&f.belt, f.live);
            for (name, m) in [
                ("nar_in", nar_in),
                ("none_in", none_in),
                ("nar", nar_out),
                ("none", none_out),
            ] {
                if m != 0 {
                    rec.push_str(&format!(",\"{name}\":{m}"));
                }
            }
            rec.push_str(&list(
                "drops",
                drops
                    .iter()
                    .map(|(v, s, age)| {
                        format!(
                            "{{\"v\":{},\"slot\":\"{}\",\"age\":{age}{}}}",
                            v.bits,
                            s.tag(),
                            tag_field(*v)
                        )
                    })
                    .collect(),
            ));
            rec.push_str(&list(
                "mem",
                stores
                    .iter()
                    .map(|(a, w, v)| format!("{{\"a\":{a},\"w\":{w},\"v\":{v}}}"))
                    .collect(),
            ));
            rec.push_str(&list(
                "scr",
                spills
                    .iter()
                    .map(|(s, v)| format!("{{\"s\":{s},\"v\":{}{}}}", v.bits, tag_field(*v)))
                    .collect(),
            ));
            rec.push_str(&list(
                "flight",
                f.pending
                    .iter()
                    .map(|p| {
                        format!(
                            "{{\"in\":{},\"slot\":\"{}\",\"n\":{}}}",
                            p.retire_at - t,
                            p.slot.tag(),
                            p.vals.len()
                        )
                    })
                    .collect(),
            ));
            if !self.out_log.is_empty() {
                rec.push_str(&format!(",\"out\":{}", json_string(&self.out_log)));
            }
            rec.push('}');
            let _ = writeln!(self.log, "{rec}");
        }

        if let Some(res) = returning {
            return Ok(Some(res));
        }
        if matches!(fop, Some(Op::Sys(0))) {
            return Err(Stop::Exit((b(0).bits & 0xff) as i32));
        }

        // -- control transfer -------------------------------------------------
        let f = &mut self.frames[depth];
        f.time += 1;
        match branch_to {
            Some(target) => {
                let e = img
                    .ebbs
                    .get(target)
                    .ok_or_else(|| Stop::Fault("branch target out of range".into()))?;
                let (bundle, arity) = (e.bundle as usize, e.arity as usize);
                let f = &mut self.frames[depth];
                // E2 cannot see an indirect edge, so the machine checks it.
                if branch_indirect && f.live & mask(arity) != mask(arity) {
                    return Err(Stop::Fault(format!(
                        "indirect branch to `{}` needs {arity} value(s) in b0.., \
                         but the belt's live mask is {:#06x}",
                        img.ebb_label(target),
                        f.live
                    )));
                }
                f.pc = bundle;
                f.truncate(arity);
            }
            None => {
                let next = pc + 1;
                let f = &mut self.frames[depth];
                f.pc = next;
                if let Some(ei) = img.ebb_at(next as u32) {
                    let arity = img.ebbs[ei].arity as usize;
                    self.frames[depth].truncate(arity);
                }
            }
        }
        Ok(None)
    }

    fn render(&self, op: &Op) -> String {
        let img = self.img;
        format_op(op, &|i| img.ebb_label(i as usize), &|i| {
            img.func_label(i as usize)
        })
    }

    fn syscall(&mut self, code: u8, a: u64, b: u64, c: u64) -> Result<Option<u64>, Stop> {
        match code {
            0 => Ok(None), // handled by the caller: exit
            1 => {
                let bytes = self.load_bytes(b, c as usize)?;
                let n = match a {
                    1 => {
                        let _ = self.out.write_all(&bytes);
                        let _ = self.out.flush();
                        if self.opts.trace_json {
                            self.out_log.extend_from_slice(&bytes);
                        }
                        bytes.len()
                    }
                    2 => {
                        let _ = std::io::stderr().write_all(&bytes);
                        bytes.len()
                    }
                    fd => return Err(Stop::Fault(format!("write to unsupported fd {fd}"))),
                };
                Ok(Some(n as u64))
            }
            2 => {
                if a != 0 {
                    return Err(Stop::Fault(format!("read from unsupported fd {a}")));
                }
                let mut buf = vec![0u8; c as usize];
                let n = std::io::stdin().read(&mut buf).unwrap_or(0);
                self.store_bytes(b, &buf[..n]);
                Ok(Some(n as u64))
            }
            other => Err(Stop::Fault(format!("unknown sys code {other}"))),
        }
    }
}

/// A JSON string literal, quotes included. Invalid UTF-8 becomes U+FFFD.
fn json_string(bytes: &[u8]) -> String {
    let mut s = String::from("\"");
    for c in String::from_utf8_lossy(bytes).chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\t' => s.push_str("\\t"),
            '\r' => s.push_str("\\r"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

fn assemble_word(bytes: &[u8], sext: bool) -> u64 {
    let mut v = 0u64;
    for (i, b) in bytes.iter().enumerate() {
        v |= (*b as u64) << (8 * i);
    }
    if sext && bytes.len() < 8 {
        let bits = 8 * bytes.len() as u32;
        let shift = 64 - bits;
        return ((v << shift) as i64 >> shift) as u64;
    }
    v
}

/// `,"t":"nar"` for a tagged value, nothing for a plain one — so a trace of a
/// run that never poisons anything is byte-identical to a pre-metadata trace.
fn tag_field(v: Value) -> &'static str {
    match v.tag {
        Tag::Val => "",
        Tag::None => ",\"t\":\"none\"",
        Tag::Nar => ",\"t\":\"nar\"",
    }
}

/// Arithmetic on values already known to be untagged. A failure drops a NaR
/// carrying its origin rather than stopping the machine (PRD §8.6).
fn alu(op: AluOp, x: u64, y: u64, pc: usize) -> Value {
    let (sx, sy) = (x as i64, y as i64);
    let nar = |k| Value::nar(k, pc);
    Value::val(match op {
        AluOp::Add => x.wrapping_add(y),
        AluOp::Sub => x.wrapping_sub(y),
        AluOp::And => x & y,
        AluOp::Or => x | y,
        AluOp::Xor => x ^ y,
        AluOp::Shl => x.wrapping_shl((y & 63) as u32),
        AluOp::Shr => x.wrapping_shr((y & 63) as u32),
        AluOp::Sar => (sx >> (y & 63)) as u64,
        AluOp::Mul => x.wrapping_mul(y),
        AluOp::Div | AluOp::Rem => {
            if y == 0 {
                return nar(NarKind::DivZero);
            }
            if sx == i64::MIN && sy == -1 {
                return nar(NarKind::Overflow);
            }
            match op {
                AluOp::Div => (sx / sy) as u64,
                _ => (sx % sy) as u64,
            }
        }
        AluOp::Divu | AluOp::Remu => {
            if y == 0 {
                return nar(NarKind::DivZero);
            }
            match op {
                AluOp::Divu => x / y,
                _ => x % y,
            }
        }
        AluOp::Eq => (x == y) as u64,
        AluOp::Ne => (x != y) as u64,
        AluOp::Lt => (sx < sy) as u64,
        AluOp::Le => (sx <= sy) as u64,
        AluOp::Ltu => (x < y) as u64,
        AluOp::Leu => (x <= y) as u64,
    })
}

/// Everything a test wants out of a run.
pub struct Run {
    pub stop: Stop,
    pub out: Vec<u8>,
    pub log: Vec<u8>,
    pub stats: Stats,
    pub points: Vec<TracePoint>,
}

/// Convenience for tests: run an image, capturing stdout and the trace log.
pub fn run_capture(img: &Image, opts: Options) -> Run {
    let mut out: Vec<u8> = Vec::new();
    let mut log: Vec<u8> = Vec::new();
    let (stop, stats, points) = {
        let mut m = Machine::new(img, Config::default(), opts, &mut out, &mut log);
        let stop = m.run();
        (stop, m.stats.clone(), m.points.clone())
    };
    Run {
        stop,
        out,
        log,
        stats,
        points,
    }
}
