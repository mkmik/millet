//! Text (`.mil`) to binary image.
//!
//! Syntax (PRD §9.2): one op per line prefixed by its slot tag (`a0`, `a1`,
//! `m`, `f`); a blank line or a directive ends the bundle. Comments start
//! with `;`. Belt operands are literal positions `b0..b15` — the assembler
//! never invents them, it only checks them (see `check.rs`).

use std::collections::HashMap;

use millet_core::isa::{self, AluOp, BrKind, Op, Slot};
use millet_core::{BELT_MAX, Config, DataSeg, EbbEntry, FuncEntry, Image, SCRATCH_MAX};

use crate::check::{self, Diag, Prediction};

/// An op as written, with its branch/call target still symbolic.
#[derive(Clone, Debug)]
pub struct SrcOp {
    pub op: Op,
    pub target: Target,
    pub line: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    None,
    Ebb(String),
    Func(String),
}

#[derive(Clone, Debug, Default)]
pub struct SrcBundle {
    pub ops: [Option<SrcOp>; 4],
    pub line: usize,
}

impl SrcBundle {
    fn is_empty(&self) -> bool {
        self.ops.iter().all(|o| o.is_none())
    }
}

#[derive(Clone, Debug)]
enum Item {
    Head {
        name: String,
        arity: u8,
        nres: u8,
        is_func: bool,
        line: usize,
    },
    Bundle(SrcBundle),
}

/// A successfully assembled program plus everything the checker and the
/// differential tests need.
pub struct Assembled {
    pub image: Image,
    pub bundles: Vec<SrcBundle>,
    pub ebb_names: Vec<String>,
    pub func_names: Vec<String>,
    pub diags: Vec<Diag>,
    pub prediction: Prediction,
}

#[derive(Debug)]
pub struct AsmError {
    pub diags: Vec<Diag>,
}

/// Assemble `src`. Returns hard errors in `Err`; warnings ride along in
/// `Assembled::diags`.
pub fn assemble(src: &str, cfg: &Config) -> Result<Assembled, AsmError> {
    let mut p = Parser::new(src, cfg);
    let (items, data, entry_name) = match p.parse() {
        Ok(v) => v,
        Err(()) => return Err(AsmError { diags: p.diags }),
    };

    // Pass 1 — lay out bundles and register symbols.
    let mut ebbs: Vec<EbbEntry> = Vec::new();
    let mut funcs: Vec<FuncEntry> = Vec::new();
    let mut ebb_names: Vec<String> = Vec::new();
    let mut func_names: Vec<String> = Vec::new();
    let mut ebb_of: HashMap<String, u32> = HashMap::new();
    let mut func_of: HashMap<String, u16> = HashMap::new();
    let mut bundles: Vec<SrcBundle> = Vec::new();
    let mut n_bundles = 0u32;

    for item in &items {
        match item {
            Item::Head {
                name,
                arity,
                nres,
                is_func,
                line,
            } => {
                if ebb_of.contains_key(name) {
                    p.err("E0", *line, format!("duplicate label `{name}`"));
                    continue;
                }
                let idx = ebbs.len() as u32;
                ebbs.push(EbbEntry {
                    bundle: n_bundles,
                    arity: *arity,
                    name: name.clone(),
                });
                ebb_names.push(name.clone());
                ebb_of.insert(name.clone(), idx);
                if *is_func {
                    if *arity as usize > isa::MAX_ARGS {
                        p.err(
                            "E6",
                            *line,
                            format!("function `{name}` declares arity {arity}; the call encoding allows at most 3"),
                        );
                    }
                    if *nres as usize > isa::MAX_ARGS {
                        p.err(
                            "E6",
                            *line,
                            format!(
                                "function `{name}` declares {nres} results; retn allows at most 3"
                            ),
                        );
                    }
                    let fidx = funcs.len() as u16;
                    funcs.push(FuncEntry {
                        ebb: idx,
                        arity: *arity,
                        nres: *nres,
                        name: name.clone(),
                    });
                    func_names.push(name.clone());
                    func_of.insert(name.clone(), fidx);
                }
            }
            Item::Bundle(b) => {
                bundles.push(b.clone());
                n_bundles += 1;
            }
        }
    }

    for (i, e) in ebbs.iter().enumerate() {
        if e.bundle as usize >= bundles.len() {
            p.err("E0", 0, format!("label `{}` has no bundles", ebb_names[i]));
        } else if ebbs
            .iter()
            .enumerate()
            .any(|(j, o)| j < i && o.bundle == e.bundle)
        {
            p.err(
                "E0",
                0,
                format!(
                    "label `{}` shares an entry bundle with an earlier label",
                    ebb_names[i]
                ),
            );
        }
    }

    // Pass 2 — resolve targets.
    for b in bundles.iter_mut() {
        for slot in Slot::ALL {
            let Some(so) = b.ops[slot.index()].as_mut() else {
                continue;
            };
            match &so.target {
                Target::None => {}
                Target::Ebb(name) => match ebb_of.get(name) {
                    Some(&i) => {
                        if let Op::Br { target, .. } = &mut so.op {
                            *target = i;
                        }
                    }
                    None => {
                        p.diags
                            .push(Diag::err("E0", so.line, format!("unknown label `{name}`")))
                    }
                },
                Target::Func(name) => match func_of.get(name) {
                    Some(&i) => {
                        if let Op::Call { target, .. } = &mut so.op {
                            *target = i;
                        }
                    }
                    None => p.diags.push(Diag::err(
                        "E0",
                        so.line,
                        format!("unknown function `{name}`"),
                    )),
                },
            }
        }
    }

    let entry = match &entry_name {
        Some(n) => func_of.get(n).copied().map(|v| v as u32),
        None => func_of.get("main").copied().map(|v| v as u32),
    };
    let entry = match entry {
        Some(e) => e,
        None => {
            p.err(
                "E0",
                0,
                match &entry_name {
                    Some(n) => format!("entry function `{n}` not found"),
                    None => "no `.func main` and no `.entry` directive".into(),
                },
            );
            0
        }
    };

    if p.diags.iter().any(|d| !d.warning) {
        return Err(AsmError { diags: p.diags });
    }

    // Encode.
    let mut words: Vec<[u32; 4]> = Vec::with_capacity(bundles.len());
    for b in &bundles {
        let mut w = [0u32; 4];
        for slot in Slot::ALL {
            if let Some(so) = &b.ops[slot.index()] {
                match isa::encode(&so.op) {
                    Ok(v) => w[slot.index()] = v,
                    Err(e) => p.err("E0", so.line, e.0),
                }
            }
        }
        words.push(w);
    }

    let image = Image {
        bundles: words,
        ebbs,
        funcs,
        data,
        entry,
    };

    // Static checks (PRD §9.3).
    let (mut diags, prediction) = check::check(&image, &bundles, &ebb_names, &func_names, cfg);
    p.diags.append(&mut diags);

    if p.diags.iter().any(|d| !d.warning) {
        return Err(AsmError { diags: p.diags });
    }

    Ok(Assembled {
        image,
        bundles,
        ebb_names,
        func_names,
        diags: p.diags,
        prediction,
    })
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Parser<'a> {
    src: &'a str,
    cfg: &'a Config,
    diags: Vec<Diag>,
    defs: HashMap<String, i64>,
}

type ParseOut = (Vec<Item>, Vec<DataSeg>, Option<String>);

impl<'a> Parser<'a> {
    fn new(src: &'a str, cfg: &'a Config) -> Self {
        Parser {
            src,
            cfg,
            diags: Vec::new(),
            defs: HashMap::new(),
        }
    }

    fn err(&mut self, code: &'static str, line: usize, msg: String) {
        self.diags.push(Diag::err(code, line, msg));
    }

    fn parse(&mut self) -> Result<ParseOut, ()> {
        let mut items: Vec<Item> = Vec::new();
        let mut data: Vec<DataSeg> = Vec::new();
        let mut entry: Option<String> = None;
        let mut cur = SrcBundle::default();
        let src = self.src.to_string();

        let flush = |cur: &mut SrcBundle, items: &mut Vec<Item>| {
            if !cur.is_empty() {
                items.push(Item::Bundle(std::mem::take(cur)));
            }
        };

        for (i, raw) in src.lines().enumerate() {
            let line = i + 1;
            let text = strip_comment(raw);
            let text = text.trim();
            if text.is_empty() {
                flush(&mut cur, &mut items);
                continue;
            }
            if let Some(rest) = text.strip_prefix('.') {
                flush(&mut cur, &mut items);
                self.directive(rest, line, &mut items, &mut data, &mut entry);
                continue;
            }
            // slot-tagged op
            let (tag, rest) = split_first_word(text);
            let Some(slot) = Slot::from_tag(tag) else {
                self.err(
                    "E0",
                    line,
                    format!("expected a slot tag (a0, a1, m, f) but found `{tag}`"),
                );
                continue;
            };
            if cur.ops[slot.index()].is_some() {
                self.err(
                    "E0",
                    line,
                    format!("slot {} already used in this bundle", tag),
                );
                continue;
            }
            if cur.is_empty() {
                cur.line = line;
            }
            match self.op(rest, line) {
                Ok(so) => cur.ops[slot.index()] = Some(so),
                Err(()) => {}
            }
        }
        flush(&mut cur, &mut items);

        if self.diags.iter().any(|d| !d.warning) {
            return Err(());
        }
        Ok((items, data, entry))
    }

    fn directive(
        &mut self,
        text: &str,
        line: usize,
        items: &mut Vec<Item>,
        data: &mut Vec<DataSeg>,
        entry: &mut Option<String>,
    ) {
        let (word, rest) = split_first_word(text);
        match word {
            "def" => {
                let (name, val) = split_first_word(rest);
                match self.imm(val.trim(), line) {
                    Ok(v) => {
                        self.defs.insert(name.to_string(), v);
                    }
                    Err(()) => {}
                }
            }
            "entry" => *entry = Some(rest.trim().to_string()),
            "func" | "ebb" => {
                let is_func = word == "func";
                match self.head(rest, line, is_func) {
                    Ok(item) => items.push(item),
                    Err(()) => {}
                }
            }
            "data" => match self.imm(rest.trim(), line) {
                Ok(addr) => data.push(DataSeg {
                    addr: addr as u64,
                    bytes: Vec::new(),
                }),
                Err(()) => {}
            },
            "u8" | "u16" | "u32" | "u64" | "ascii" | "asciiz" | "zero" => {
                if data.is_empty() {
                    self.err("E0", line, format!(".{word} outside a .data section"));
                    return;
                }
                let seg_bytes = match self.data_bytes(word, rest.trim(), line) {
                    Ok(b) => b,
                    Err(()) => return,
                };
                data.last_mut().unwrap().bytes.extend_from_slice(&seg_bytes);
            }
            _ => self.err("E0", line, format!("unknown directive `.{word}`")),
        }
    }

    fn data_bytes(&mut self, kind: &str, rest: &str, line: usize) -> Result<Vec<u8>, ()> {
        let mut out = Vec::new();
        match kind {
            "ascii" | "asciiz" => {
                out = parse_string(rest).map_err(|e| self.err("E0", line, e))?;
                if kind == "asciiz" {
                    out.push(0);
                }
            }
            "zero" => {
                let n = self.imm(rest, line)?;
                if n < 0 {
                    self.err("E0", line, ".zero count must be non-negative".into());
                    return Err(());
                }
                out.resize(n as usize, 0);
            }
            _ => {
                let width = match kind {
                    "u8" => 1,
                    "u16" => 2,
                    "u32" => 4,
                    _ => 8,
                };
                for part in rest.split(',') {
                    let v = self.imm(part.trim(), line)?;
                    out.extend_from_slice(&(v as u64).to_le_bytes()[..width]);
                }
            }
        }
        Ok(out)
    }

    /// `.func name(arity) -> nres` / `.ebb name(arity)`
    fn head(&mut self, text: &str, line: usize, is_func: bool) -> Result<Item, ()> {
        let (decl, ret) = match text.split_once("->") {
            Some((a, b)) => (a.trim(), Some(b.trim())),
            None => (text.trim(), None),
        };
        let Some((name, arity)) = decl.strip_suffix(')').and_then(|d| d.split_once('(')) else {
            self.err(
                "E0",
                line,
                format!("expected `name(arity)`, found `{decl}`"),
            );
            return Err(());
        };
        let arity = self.imm(arity.trim(), line)?;
        if arity < 0 || arity as usize > self.cfg.belt {
            self.err(
                "E0",
                line,
                format!("arity {arity} outside 0..{}", self.cfg.belt),
            );
            return Err(());
        }
        let nres = match ret {
            Some(r) => self.imm(r, line)?,
            None => 0,
        };
        if !is_func && ret.is_some() {
            self.err("E0", line, "`.ebb` does not take a result count".into());
            return Err(());
        }
        Ok(Item::Head {
            name: name.trim().to_string(),
            arity: arity as u8,
            nres: nres as u8,
            is_func,
            line,
        })
    }

    fn op(&mut self, text: &str, line: usize) -> Result<SrcOp, ()> {
        let (mnem, rest) = split_first_word(text.trim());
        let args: Vec<&str> = if rest.trim().is_empty() {
            Vec::new()
        } else {
            rest.split(',').map(|s| s.trim()).collect()
        };
        let n = args.len();
        let want = |p: &mut Self, k: usize| -> Result<(), ()> {
            if n != k {
                p.err(
                    "E0",
                    line,
                    format!("`{mnem}` takes {k} operand(s), found {n}"),
                );
                Err(())
            } else {
                Ok(())
            }
        };

        let mut target = Target::None;
        let op = if let Some(a) = AluOp::from_mnemonic(mnem) {
            want(self, 2)?;
            Op::Alu {
                op: a,
                a: self.belt(args[0], line)?,
                b: self.belt(args[1], line)?,
            }
        } else {
            match mnem {
                "nop" => {
                    want(self, 0)?;
                    Op::Nop
                }
                "pick" => {
                    want(self, 3)?;
                    Op::Pick {
                        c: self.belt(args[0], line)?,
                        t: self.belt(args[1], line)?,
                        f: self.belt(args[2], line)?,
                    }
                }
                "con" => {
                    want(self, 1)?;
                    let v = self.imm(args[0], line)?;
                    if !(-(1 << 23)..(1 << 23)).contains(&v) {
                        self.err(
                            "E0",
                            line,
                            format!("con immediate {v} does not fit in 24 signed bits"),
                        );
                        return Err(());
                    }
                    Op::Con { imm: v as i32 }
                }
                "load" => {
                    want(self, 5)?;
                    let addr = self.belt(args[0], line)?;
                    let offset = self.off13(args[1], line)?;
                    let width = self.width(args[2], line)?;
                    let sext = self.ext(args[3], line)?;
                    let delay = self.imm(args[4], line)?;
                    if !(3..=15).contains(&delay) {
                        self.err(
                            "E0",
                            line,
                            format!(
                                "load delay {delay} outside 3..15 (0..2 are illegal encodings)"
                            ),
                        );
                        return Err(());
                    }
                    if sext && width == 8 {
                        self.err(
                            "E0",
                            line,
                            "8-byte load takes no extension; use `zero`".into(),
                        );
                        return Err(());
                    }
                    Op::Load {
                        addr,
                        offset,
                        width,
                        sext,
                        delay: delay as u8,
                    }
                }
                "store" => {
                    want(self, 4)?;
                    Op::Store {
                        addr: self.belt(args[0], line)?,
                        offset: self.off13(args[1], line)?,
                        width: self.width(args[2], line)?,
                        val: self.belt(args[3], line)?,
                    }
                }
                "spill" => {
                    want(self, 2)?;
                    Op::Spill {
                        slot: self.scratch(args[0], line)?,
                        val: self.belt(args[1], line)?,
                    }
                }
                "fill" => {
                    want(self, 1)?;
                    Op::Fill {
                        slot: self.scratch(args[0], line)?,
                    }
                }
                "br" => {
                    want(self, 1)?;
                    target = Target::Ebb(args[0].to_string());
                    Op::Br {
                        kind: BrKind::Always,
                        cond: 0,
                        target: 0,
                    }
                }
                "brt" | "brf" => {
                    want(self, 2)?;
                    target = Target::Ebb(args[1].to_string());
                    Op::Br {
                        kind: if mnem == "brt" {
                            BrKind::IfTrue
                        } else {
                            BrKind::IfFalse
                        },
                        cond: self.belt(args[0], line)?,
                        target: 0,
                    }
                }
                "call" => {
                    if n == 0 {
                        self.err("E0", line, "`call` needs a target".into());
                        return Err(());
                    }
                    if n - 1 > isa::MAX_ARGS {
                        self.err(
                            "E6",
                            line,
                            format!("call passes {} arguments; max 3", n - 1),
                        );
                        return Err(());
                    }
                    target = Target::Func(args[0].to_string());
                    let mut a = Vec::new();
                    for s in &args[1..] {
                        a.push(self.belt(s, line)?);
                    }
                    Op::Call { target: 0, args: a }
                }
                "retn" => {
                    if n > isa::MAX_ARGS {
                        self.err("E6", line, format!("retn returns {n} values; max 3"));
                        return Err(());
                    }
                    let mut r = Vec::new();
                    for s in &args {
                        r.push(self.belt(s, line)?);
                    }
                    Op::Retn { res: r }
                }
                "conform" => {
                    if n == 0 || n > isa::MAX_CONFORM {
                        self.err(
                            "E5",
                            line,
                            format!("conform takes 1..{} positions, found {n}", isa::MAX_CONFORM),
                        );
                        return Err(());
                    }
                    let mut l = Vec::new();
                    for s in &args {
                        l.push(self.belt(s, line)?);
                    }
                    Op::Conform(l)
                }
                "rescue" => {
                    want(self, 1)?;
                    let m = self.imm(args[0], line)?;
                    if !(0..=0xffff).contains(&m) {
                        self.err(
                            "E5",
                            line,
                            format!("rescue mask {m} does not fit in 16 bits"),
                        );
                        return Err(());
                    }
                    if m as u32 >> self.cfg.belt != 0 {
                        self.err(
                            "E5",
                            line,
                            format!(
                                "rescue mask {m:#x} selects positions beyond b{}",
                                self.cfg.belt - 1
                            ),
                        );
                        return Err(());
                    }
                    Op::Rescue(m as u16)
                }
                "sys" => {
                    want(self, 1)?;
                    let c = self.imm(args[0], line)?;
                    if !(0..=2).contains(&c) {
                        self.err("E0", line, format!("unknown sys code {c} (0..2)"));
                        return Err(());
                    }
                    Op::Sys(c as u8)
                }
                "halt" => {
                    want(self, 0)?;
                    Op::Halt
                }
                _ => {
                    self.err("E0", line, format!("unknown mnemonic `{mnem}`"));
                    return Err(());
                }
            }
        };
        Ok(SrcOp { op, target, line })
    }

    fn belt(&mut self, s: &str, line: usize) -> Result<u8, ()> {
        let ok = s
            .strip_prefix('b')
            .and_then(|d| d.parse::<usize>().ok())
            .filter(|v| *v < BELT_MAX);
        match ok {
            Some(v) if v < self.cfg.belt => Ok(v as u8),
            Some(v) => {
                self.err(
                    "E1",
                    line,
                    format!(
                        "belt position b{v} is beyond this machine's belt (b0..b{})",
                        self.cfg.belt - 1
                    ),
                );
                Err(())
            }
            None => {
                self.err(
                    "E0",
                    line,
                    format!("expected a belt reference b0..b15, found `{s}`"),
                );
                Err(())
            }
        }
    }

    fn scratch(&mut self, s: &str, line: usize) -> Result<u8, ()> {
        let Some(rest) = s.strip_prefix('s') else {
            self.err(
                "E0",
                line,
                format!("expected a scratchpad slot sN, found `{s}`"),
            );
            return Err(());
        };
        let v = self.imm(rest, line)?;
        if v < 0 || v as usize >= self.cfg.scratch.min(SCRATCH_MAX) {
            self.err(
                "E7",
                line,
                format!(
                    "scratchpad slot s{v} out of range (0..{})",
                    self.cfg.scratch - 1
                ),
            );
            return Err(());
        }
        Ok(v as u8)
    }

    fn width(&mut self, s: &str, line: usize) -> Result<u8, ()> {
        match self.imm(s, line)? {
            v @ (1 | 2 | 4 | 8) => Ok(v as u8),
            v => {
                self.err("E0", line, format!("width {v} must be 1, 2, 4 or 8"));
                Err(())
            }
        }
    }

    fn ext(&mut self, s: &str, line: usize) -> Result<bool, ()> {
        match s {
            "zero" => Ok(false),
            "sign" => Ok(true),
            _ => {
                self.err(
                    "E0",
                    line,
                    format!("extension must be `zero` or `sign`, found `{s}`"),
                );
                Err(())
            }
        }
    }

    fn off13(&mut self, s: &str, line: usize) -> Result<i16, ()> {
        let v = self.imm(s, line)?;
        if !(-(1 << 12)..(1 << 12)).contains(&v) {
            self.err(
                "E0",
                line,
                format!("offset {v} does not fit in signed 13 bits"),
            );
            return Err(());
        }
        Ok(v as i16)
    }

    fn imm(&mut self, s: &str, line: usize) -> Result<i64, ()> {
        match parse_imm(s, &self.defs) {
            Some(v) => Ok(v),
            None => {
                self.err("E0", line, format!("expected an immediate, found `{s}`"));
                Err(())
            }
        }
    }
}

fn parse_imm(s: &str, defs: &HashMap<String, i64>) -> Option<i64> {
    let s = s.trim();
    if let Some(v) = defs.get(s) {
        return Some(*v);
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b.trim()),
        None => (false, s),
    };
    let v = if let Some(h) = body.strip_prefix("0x") {
        i64::from_str_radix(&h.replace('_', ""), 16).ok()?
    } else if let Some(b) = body.strip_prefix("0b") {
        i64::from_str_radix(&b.replace('_', ""), 2).ok()?
    } else if let Some(c) = body.strip_prefix('\'') {
        let inner = c.strip_suffix('\'')?;
        let bytes = parse_string_body(inner).ok()?;
        if bytes.len() != 1 {
            return None;
        }
        bytes[0] as i64
    } else {
        body.replace('_', "").parse::<i64>().ok()?
    };
    Some(if neg { -v } else { v })
}

fn strip_comment(line: &str) -> &str {
    // `;` starts a comment, except inside a string literal.
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_str = !in_str,
            b'\\' if in_str => i += 1,
            b';' if !in_str => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

fn parse_string(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    let inner = s
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or_else(|| format!("expected a quoted string, found `{s}`"))?;
    parse_string_body(inner)
}

fn parse_string_body(inner: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match it.next() {
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('r') => out.push(b'\r'),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('"') => out.push(b'"'),
            Some('x') => {
                let h: String = it.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&h, 16).map_err(|_| format!("bad \\x escape `{h}`"))?);
            }
            other => return Err(format!("bad escape `\\{}`", other.unwrap_or('?'))),
        }
    }
    Ok(out)
}
