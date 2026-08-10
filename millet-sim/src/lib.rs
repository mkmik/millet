//! millet-sim — a bundle-accurate, deterministic Millet simulator.
//!
//! One bundle per cycle, never stalls. All operand reads observe the belt as
//! it exists at bundle entry; all drops happen at bundle end in
//! (issuing bundle, slot) order; `conform`/`rescue` rewrite the post-drop
//! belt last (PRD §3.1, §5.3, §7.2).

use std::collections::HashMap;
use std::io::{Read, Write};

use millet_core::isa::{AluOp, BrKind, Op, Slot, decode, format_op};
use millet_core::{Config, Image};

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
    vals: Vec<u64>,
}

#[derive(Clone, Debug)]
struct Frame {
    belt: Vec<u64>,
    live: u16,
    scratch: Vec<u64>,
    pending: Vec<Pending>,
    /// Bundles executed in this frame; latencies are frame-local (PRD §6.2).
    time: usize,
    pc: usize,
}

impl Frame {
    fn new(cfg: &Config, pc: usize) -> Frame {
        Frame {
            belt: vec![0; cfg.belt],
            live: 0,
            scratch: vec![0; cfg.scratch],
            pending: Vec::new(),
            time: 0,
            pc,
        }
    }

    fn drop_value(&mut self, v: u64) {
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
            self.belt[i] = 0;
        }
        self.live &= mask(k);
    }
}

fn mask(k: usize) -> u16 {
    if k >= 16 { u16::MAX } else { (1u16 << k) - 1 }
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
    fn run_frame(&mut self) -> Result<Vec<u64>, Stop> {
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
    fn step(&mut self) -> Result<Option<Vec<u64>>, Stop> {
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
        if self.opts.trace {
            let ebb = img
                .ebb_containing(pc as u32)
                .map(|i| img.ebb_label(i))
                .unwrap_or_default();
            let inflight = self.frames[depth].pending_summary(t);
            let _ = writeln!(
                self.log,
                "[{:>6}] frame {depth} bundle {pc} in {ebb}{inflight}",
                self.stats.bundles - 1
            );
        }

        let b = |p: u8| entry_belt[p as usize];
        let mut stores: Vec<(u64, u8, u64)> = Vec::new();
        let mut spills: Vec<(usize, u64)> = Vec::new();

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
                Op::Alu { op: a, a: x, b: y } => Some(alu(*a, b(*x), b(*y))?),
                Op::Pick { c, t: x, f: y } => Some(if b(*c) != 0 { b(*x) } else { b(*y) }),
                Op::Con { imm } => Some(*imm as i64 as u64),
                Op::Load {
                    addr,
                    offset,
                    width,
                    sext,
                    ..
                } => {
                    let a = b(*addr).wrapping_add(*offset as i64 as u64);
                    let bytes = self.load_bytes(a, *width as usize)?;
                    Some(assemble_word(&bytes, *sext))
                }
                Op::Store {
                    addr,
                    offset,
                    width,
                    val,
                } => {
                    let a = b(*addr).wrapping_add(*offset as i64 as u64);
                    stores.push((a, *width, b(*val)));
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
        for (s, v) in &spills {
            self.frames[depth].scratch[*s] = *v;
            if self.opts.trace {
                let line = format!("       scr  s{s} <- {v}");
                let _ = writeln!(self.log, "{line}");
            }
        }

        // -- slot F ----------------------------------------------------------
        let mut branch_to: Option<usize> = None;
        let mut returning: Option<Vec<u64>> = None;
        let fop = ops[Slot::F.index()].clone();
        if let Some(op) = &fop {
            self.stats.slot_ops[Slot::F.index()] += 1;
            if self.opts.trace {
                let line = format!("       {:<3} {}", Slot::F.tag(), self.render(op));
                let _ = writeln!(self.log, "{line}");
            }
            match op {
                Op::Br { kind, cond, target } => {
                    let take = match kind {
                        BrKind::Always => true,
                        BrKind::IfTrue => b(*cond) != 0,
                        BrKind::IfFalse => b(*cond) == 0,
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
                    let v = self.syscall(*code, b(0), b(1), b(2))?;
                    if let Some(v) = v {
                        self.frames[depth].pending.push(Pending {
                            retire_at: t,
                            issue: t,
                            slot: Slot::F,
                            vals: vec![v],
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
        for p in &retiring {
            for v in &p.vals {
                self.frames[depth].drop_value(*v);
                if self.opts.trace {
                    let line = format!(
                        "       drop <- {v}  (slot {} issued at +{})",
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
                let mut nb = vec![0u64; old.len()];
                for (i, p) in list.iter().enumerate() {
                    nb[i] = old[*p as usize];
                }
                f.belt = nb;
                f.live = mask(list.len());
            }
            Some(Op::Rescue(m)) => {
                let f = &mut self.frames[depth];
                let old = f.belt.clone();
                let mut nb = vec![0u64; old.len()];
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
                            format!("b{i}={}", f.belt[i] as i64)
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
            let belt: Vec<String> = f.belt.iter().map(|v| v.to_string()).collect();
            let _ = writeln!(
                self.log,
                "{{\"cycle\":{},\"frame\":{depth},\"bundle\":{pc},\"live_in\":{live_in},\"live_out\":{},\"belt\":[{}]}}",
                self.stats.bundles - 1,
                f.live,
                belt.join(",")
            );
        }

        if let Some(res) = returning {
            return Ok(Some(res));
        }
        if matches!(fop, Some(Op::Sys(0))) {
            return Err(Stop::Exit((b(0) & 0xff) as i32));
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

fn alu(op: AluOp, x: u64, y: u64) -> Result<u64, Stop> {
    let (sx, sy) = (x as i64, y as i64);
    Ok(match op {
        AluOp::Add => x.wrapping_add(y),
        AluOp::Sub => x.wrapping_sub(y),
        AluOp::And => x & y,
        AluOp::Or => x | y,
        AluOp::Xor => x ^ y,
        AluOp::Shl => x.wrapping_shl((y & 63) as u32),
        AluOp::Shr => x.wrapping_shr((y & 63) as u32),
        AluOp::Sar => (sx >> (y & 63)) as u64,
        AluOp::Mul => x.wrapping_mul(y),
        AluOp::Div => {
            if y == 0 {
                return Err(Stop::Fault("signed division by zero".into()));
            }
            if sx == i64::MIN && sy == -1 {
                return Err(Stop::Fault(
                    "signed division overflow (INT_MIN / -1)".into(),
                ));
            }
            (sx / sy) as u64
        }
        AluOp::Divu => {
            if y == 0 {
                return Err(Stop::Fault("unsigned division by zero".into()));
            }
            x / y
        }
        AluOp::Rem => {
            if y == 0 {
                return Err(Stop::Fault("signed remainder by zero".into()));
            }
            if sx == i64::MIN && sy == -1 {
                return Err(Stop::Fault(
                    "signed remainder overflow (INT_MIN % -1)".into(),
                ));
            }
            (sx % sy) as u64
        }
        AluOp::Remu => {
            if y == 0 {
                return Err(Stop::Fault("unsigned remainder by zero".into()));
            }
            x % y
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
