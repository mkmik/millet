//! `mview` — a terminal viewer for `msim --trace-json` traces.
//!
//! The trace is a full per-bundle state record, so the whole run is in memory
//! and time is just an index: every key moves the cursor, and the screen is
//! redrawn from the record it lands on. Memory, scratch and program output are
//! the only accumulated state, and those are replayed from the start.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::sync::OnceLock;

use millet_core::isa::{decode, format_op, Op, Slot};
use millet_core::{Image, BELT_MAX};
use millet_sim::{run_capture, Options, Stop};

const USAGE: &str = "\
mview — Millet trace viewer

usage:
  mview [options] <image.mimg> [trace.jsonl]

With no trace file mview runs the image itself. `-` reads the trace from stdin
(`msim --trace-json prog.mimg 2>trace.jsonl` writes one there).

options:
  --max-bundles <n>   cap for the built-in run (default 200000)
  -h, --help          this message
";

const KEYS: &[(&str, &str)] = &[
    ("\u{2190} \u{2192}  h l", "step one bundle"),
    ("\u{2191} \u{2193}  H L", "ten bundles"),
    ("PgUp/PgDn", "a hundred bundles"),
    ("g / G", "jump to the first / last bundle of the trace"),
    ("n / p", "next / previous run of this same bundle"),
    (
        "o",
        "step over a call (next bundle at this depth or shallower)",
    ),
    ("u", "step out (next bundle in the caller)"),
    (
        "[ / ]",
        "scroll the memory window; = follows the last store again",
    ),
    ("x", "belt values as hex or signed decimal"),
    ("?", "this help"),
    ("q", "quit"),
];

// ---------------------------------------------------------------------------
// A minimal JSON reader. The trace is our own output, but a half-written last
// line is normal (the sim was killed), so parsing stays fallible.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum J {
    Num(i128),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
    Bool(bool),
    Null,
}

impl J {
    fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(fields) => fields.iter().find(|(n, _)| n == k).map(|(_, v)| v),
            _ => None,
        }
    }
    fn num(&self) -> i128 {
        match self {
            J::Num(n) => *n,
            _ => 0,
        }
    }
    fn arr(&self) -> &[J] {
        match self {
            J::Arr(v) => v,
            _ => &[],
        }
    }
    fn text(&self) -> &str {
        match self {
            J::Str(s) => s,
            _ => "",
        }
    }
    /// Field lookups are total: absent means "this bundle had no such effect",
    /// which is how the writer omits empty lists.
    fn at(&self, k: &str) -> &J {
        static NULL: J = J::Null;
        self.get(k).unwrap_or(&NULL)
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        self.ws();
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected `{}` at byte {}", c as char, self.i))
        }
    }

    fn value(&mut self) -> Result<J, String> {
        self.ws();
        match self.b.get(self.i) {
            None => Err("unexpected end of input".into()),
            Some(b'{') => {
                self.i += 1;
                let mut fields = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(J::Obj(fields));
                }
                loop {
                    self.ws();
                    let key = match self.value()? {
                        J::Str(s) => s,
                        _ => return Err(format!("object key at byte {} is not a string", self.i)),
                    };
                    self.eat(b':')?;
                    fields.push((key, self.value()?));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(J::Obj(fields));
                        }
                        _ => return Err(format!("unterminated object at byte {}", self.i)),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(J::Arr(items));
                }
                loop {
                    items.push(self.value()?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(J::Arr(items));
                        }
                        _ => return Err(format!("unterminated array at byte {}", self.i)),
                    }
                }
            }
            Some(b'"') => {
                self.i += 1;
                let mut s = String::new();
                loop {
                    let c = *self.b.get(self.i).ok_or("unterminated string")?;
                    self.i += 1;
                    match c {
                        b'"' => return Ok(J::Str(s)),
                        b'\\' => {
                            let e = *self.b.get(self.i).ok_or("unterminated escape")?;
                            self.i += 1;
                            match e {
                                b'n' => s.push('\n'),
                                b't' => s.push('\t'),
                                b'r' => s.push('\r'),
                                b'b' => s.push('\u{8}'),
                                b'f' => s.push('\u{c}'),
                                b'u' => {
                                    let hex = self
                                        .b
                                        .get(self.i..self.i + 4)
                                        .ok_or("truncated \\u escape")?;
                                    let n = u32::from_str_radix(
                                        std::str::from_utf8(hex).map_err(|e| e.to_string())?,
                                        16,
                                    )
                                    .map_err(|e| e.to_string())?;
                                    self.i += 4;
                                    s.push(char::from_u32(n).unwrap_or('\u{fffd}'));
                                }
                                other => s.push(other as char),
                            }
                        }
                        // The writer emits valid UTF-8; rebuild it byte by byte.
                        _ => {
                            let start = self.i - 1;
                            while self.b.get(self.i).is_some_and(|c| c & 0xc0 == 0x80) {
                                self.i += 1;
                            }
                            s.push_str(&String::from_utf8_lossy(&self.b[start..self.i]));
                        }
                    }
                }
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let start = self.i;
                self.i += 1;
                while self
                    .b
                    .get(self.i)
                    .is_some_and(|c| c.is_ascii_digit() || *c == b'.' || *c == b'e' || *c == b'-')
                {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
                t.parse::<i128>()
                    .map(J::Num)
                    .map_err(|_| format!("`{t}` is not an integer"))
            }
            Some(_) if self.b[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(J::Bool(true))
            }
            Some(_) if self.b[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(J::Bool(false))
            }
            Some(_) if self.b[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(J::Null)
            }
            Some(c) => Err(format!("unexpected byte `{}` at {}", *c as char, self.i)),
        }
    }
}

fn parse_json(s: &str) -> Result<J, String> {
    let mut p = Parser {
        b: s.as_bytes(),
        i: 0,
    };
    let v = p.value()?;
    p.ws();
    match p.i == p.b.len() {
        true => Ok(v),
        false => Err(format!("trailing junk at byte {}", p.i)),
    }
}

// ---------------------------------------------------------------------------
// Trace records
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Rec {
    cycle: u64,
    frame: usize,
    bundle: usize,
    live_in: u16,
    live_out: u16,
    belt_in: [u64; BELT_MAX],
    belt: [u64; BELT_MAX],
    /// value, slot, bundles since it was issued.
    drops: Vec<(u64, Slot, usize)>,
    /// address, width, value.
    mem: Vec<(u64, u8, u64)>,
    scr: Vec<(u8, u64)>,
    /// bundles until it lands, slot, number of values.
    flight: Vec<(usize, Slot, usize)>,
    out: String,
}

fn belt_of(j: &J) -> [u64; BELT_MAX] {
    let mut b = [0u64; BELT_MAX];
    for (i, v) in j.arr().iter().take(BELT_MAX).enumerate() {
        b[i] = v.num() as u64;
    }
    b
}

fn slot_of(j: &J) -> Slot {
    Slot::from_tag(j.text()).unwrap_or(Slot::A0)
}

fn parse_trace(text: &str) -> (Vec<Rec>, Vec<String>) {
    let mut recs = Vec::new();
    let mut errs = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let j = match parse_json(line) {
            Ok(j) => j,
            Err(e) => {
                errs.push(format!("line {}: {e}", n + 1));
                continue;
            }
        };
        if j.get("bundle").is_none() {
            errs.push(format!("line {}: not a trace record", n + 1));
            continue;
        }
        recs.push(Rec {
            cycle: j.at("cycle").num() as u64,
            frame: j.at("frame").num() as usize,
            bundle: j.at("bundle").num() as usize,
            live_in: j.at("live_in").num() as u16,
            live_out: j.at("live_out").num() as u16,
            belt_in: belt_of(j.at("belt_in")),
            belt: belt_of(j.at("belt")),
            drops: j
                .at("drops")
                .arr()
                .iter()
                .map(|d| {
                    (
                        d.at("v").num() as u64,
                        slot_of(d.at("slot")),
                        d.at("age").num() as usize,
                    )
                })
                .collect(),
            mem: j
                .at("mem")
                .arr()
                .iter()
                .map(|m| {
                    (
                        m.at("a").num() as u64,
                        m.at("w").num() as u8,
                        m.at("v").num() as u64,
                    )
                })
                .collect(),
            scr: j
                .at("scr")
                .arr()
                .iter()
                .map(|s| (s.at("s").num() as u8, s.at("v").num() as u64))
                .collect(),
            flight: j
                .at("flight")
                .arr()
                .iter()
                .map(|f| {
                    (
                        f.at("in").num() as usize,
                        slot_of(f.at("slot")),
                        f.at("n").num() as usize,
                    )
                })
                .collect(),
            out: j.at("out").text().to_string(),
        });
    }
    // A bundle containing a call finishes only once the callee has returned, so
    // records arrive in completion order; `cycle` is the real execution order.
    recs.sort_by_key(|r| r.cycle);
    (recs, errs)
}

// ---------------------------------------------------------------------------
// Accumulated state: everything that is not in a single record
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Acc {
    /// Number of records applied; state is "after record `applied - 1`".
    applied: usize,
    mem: BTreeMap<u64, u8>,
    last_store: Option<u64>,
    /// Per frame depth — the scratchpad is frame-local (PRD §6.2).
    scratch: Vec<BTreeMap<u8, u64>>,
    /// Bundle currently executing at each depth: the call stack.
    stack: Vec<usize>,
    out: String,
    prev_frame: Option<usize>,
}

impl Acc {
    fn reset(&mut self, img: &Image) {
        *self = Acc::default();
        for seg in &img.data {
            for (i, b) in seg.bytes.iter().enumerate() {
                self.mem.insert(seg.addr.wrapping_add(i as u64), *b);
            }
        }
    }

    fn apply(&mut self, r: &Rec) {
        if self.stack.len() <= r.frame {
            self.stack.resize(r.frame + 1, 0);
            self.scratch.resize(r.frame + 1, BTreeMap::new());
        }
        // Deeper than the last record means a fresh frame: its scratchpad and
        // everything below it are gone.
        if self.prev_frame.is_none_or(|p| p < r.frame) {
            self.scratch[r.frame].clear();
        }
        self.stack.truncate(r.frame + 1);
        self.stack[r.frame] = r.bundle;
        self.prev_frame = Some(r.frame);

        for (a, w, v) in &r.mem {
            for i in 0..*w as u64 {
                self.mem.insert(a.wrapping_add(i), (v >> (8 * i)) as u8);
            }
            self.last_store = Some(*a);
        }
        for (s, v) in &r.scr {
            self.scratch[r.frame].insert(*s, *v);
        }
        self.out.push_str(&r.out);
    }

    /// State as of record `i`. Stepping forward is incremental; stepping back
    /// replays from the start.
    // ponytail: replay is O(n) per backward seek — fine at trace sizes that fit
    // in memory. Snapshot every 1k records if that ever stops being true.
    fn seek(&mut self, img: &Image, recs: &[Rec], i: usize) {
        if self.applied > i + 1 {
            self.reset(img);
        }
        while self.applied < i + 1 {
            self.apply(&recs[self.applied]);
            self.applied += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Image queries
// ---------------------------------------------------------------------------

/// The function a bundle belongs to: the last one whose entry EBB starts at or
/// before it.
fn func_at(img: &Image, bundle: usize) -> Option<usize> {
    img.funcs
        .iter()
        .enumerate()
        .filter(|(_, f)| img.ebbs[f.ebb as usize].bundle as usize <= bundle)
        .max_by_key(|(_, f)| img.ebbs[f.ebb as usize].bundle)
        .map(|(i, _)| i)
}

fn ops_at(img: &Image, bundle: usize) -> Vec<(Slot, Op)> {
    let Some(words) = img.bundles.get(bundle) else {
        return Vec::new();
    };
    Slot::ALL
        .iter()
        .filter_map(|s| match decode(words[s.index()]) {
            Ok(Op::Nop) | Err(_) => None,
            Ok(op) => Some((*s, op)),
        })
        .collect()
}

fn render_op(img: &Image, op: &Op) -> String {
    format_op(op, &|i| img.ebb_label(i as usize), &|i| {
        img.func_label(i as usize)
    })
}

/// Where the value dropping now was issued: walk back over records of this same
/// frame, stopping if we leave it.
fn issuer(img: &Image, recs: &[Rec], i: usize, age: usize, slot: Slot) -> String {
    let depth = recs[i].frame;
    let mut seen = 0;
    for j in (0..i).rev() {
        if recs[j].frame < depth {
            break;
        }
        if recs[j].frame != depth {
            continue;
        }
        seen += 1;
        if seen == age {
            return match ops_at(img, recs[j].bundle)
                .into_iter()
                .find(|(s, _)| *s == slot)
            {
                Some((_, op)) => render_op(img, &op),
                None => String::new(),
            };
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Screen plumbing
// ---------------------------------------------------------------------------

static COLOR: OnceLock<bool> = OnceLock::new();

fn sgr(code: &str, s: &str) -> String {
    match COLOR.get() {
        Some(false) => s.to_string(),
        _ => format!("\x1b[{code}m{s}\x1b[0m"),
    }
}

const DIM: &str = "2";
const BOLD: &str = "1";
const HEAD: &str = "1;36";
const NEW: &str = "1;32";
const READ: &str = "33";
const HERE: &str = "1;35";
const BAR: &str = "7";

fn clip(s: &str, n: usize) -> String {
    match s.chars().count() > n {
        true if n > 1 => s.chars().take(n - 1).chain("…".chars()).collect(),
        true => String::new(),
        false => s.to_string(),
    }
}

/// A screen row built from styled pieces, tracking its own visible width so the
/// escape codes never confuse the padding.
#[derive(Default, Clone)]
struct Line {
    s: String,
    w: usize,
}

impl Line {
    fn raw(mut self, t: &str) -> Line {
        self.w += t.chars().count();
        self.s.push_str(t);
        self
    }
    fn put(mut self, code: &str, t: &str) -> Line {
        self.w += t.chars().count();
        self.s.push_str(&sgr(code, t));
        self
    }
    fn pad(mut self, n: usize) -> Line {
        while self.w < n {
            self.s.push(' ');
            self.w += 1;
        }
        self
    }
    /// Pad to a column, but never on top of what is already there.
    fn gap(self, n: usize) -> Line {
        let n = n.max(self.w + 1);
        self.pad(n)
    }
}

fn line() -> Line {
    Line::default()
}

fn header(t: &str) -> Line {
    line().put(HEAD, t)
}

struct Term {
    tty: File,
}

fn stty(args: &[&str]) {
    if let Ok(tty) = File::open("/dev/tty") {
        let _ = Command::new("stty")
            .args(args)
            .stdin(Stdio::from(tty))
            .status();
    }
}

impl Term {
    fn new() -> std::io::Result<Term> {
        let tty = File::open("/dev/tty")?;
        stty(&["raw", "-echo"]);
        print!("\x1b[?1049h\x1b[?25l");
        let _ = std::io::stdout().flush();
        Ok(Term { tty })
    }

    /// Re-asked on every redraw, which is how resizing works without a signal
    /// handler. `stty` costs a millisecond; a keystroke costs a hundred.
    fn size(&self) -> (usize, usize) {
        let out = File::open("/dev/tty")
            .ok()
            .and_then(|tty| {
                Command::new("stty")
                    .arg("size")
                    .stdin(Stdio::from(tty))
                    .output()
                    .ok()
            })
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let mut it = out.split_whitespace().filter_map(|n| n.parse().ok());
        match (it.next(), it.next()) {
            (Some(h), Some(w)) => (w, h),
            _ => (80, 24),
        }
    }

    fn key(&mut self) -> Option<Key> {
        let mut b = [0u8; 1];
        let mut next = |t: &mut File| t.read(&mut b).ok().filter(|n| *n == 1).map(|_| b[0]);
        match next(&mut self.tty)? {
            0x1b => match next(&mut self.tty)? {
                b'[' | b'O' => match next(&mut self.tty)? {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    b'H' => Some(Key::Home),
                    b'F' => Some(Key::End),
                    c @ (b'5' | b'6') => {
                        let _ = next(&mut self.tty); // the trailing `~`
                        Some(if c == b'5' { Key::PgUp } else { Key::PgDn })
                    }
                    _ => Some(Key::Other),
                },
                _ => Some(Key::Other),
            },
            0x7f => Some(Key::Left),
            c => Some(Key::Char(c as char)),
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        print!("\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
        stty(&["-raw", "echo"]);
    }
}

enum Key {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PgUp,
    PgDn,
    Other,
}

// ---------------------------------------------------------------------------
// Panes
// ---------------------------------------------------------------------------

struct View {
    hex: bool,
    help: bool,
    /// Base address of the memory window, or None while it follows stores.
    mem_at: Option<u64>,
}

fn fmt_val(v: u64, hex: bool) -> String {
    match hex {
        true => format!("{v:#x}"),
        false => format!("{}", v as i64),
    }
}

/// One text line per source line of the disassembly, tagged with its bundle.
fn listing(img: &Image) -> (Vec<(usize, bool, String)>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut first = vec![0usize; img.bundles.len()];
    for b in 0..img.bundles.len() {
        if let Some(ei) = img.ebb_at(b as u32) {
            let e = &img.ebbs[ei];
            let text = match img.funcs.iter().position(|f| f.ebb as usize == ei) {
                Some(fi) => format!(
                    ".func {}({}) -> {}",
                    img.func_label(fi),
                    e.arity,
                    img.funcs[fi].nres
                ),
                None => format!(".ebb {}({})", img.ebb_label(ei), e.arity),
            };
            lines.push((b, true, text));
        }
        first[b] = lines.len();
        let ops = ops_at(img, b);
        if ops.is_empty() {
            lines.push((b, false, format!("{b:>4}  nop")));
            continue;
        }
        for (n, (slot, op)) in ops.iter().enumerate() {
            let num = match n {
                0 => format!("{b:>4}"),
                _ => "    ".into(),
            };
            lines.push((
                b,
                false,
                format!("{num}  {:<3} {}", slot.tag(), render_op(img, op)),
            ));
        }
    }
    (lines, first)
}

fn code_pane(
    img: &Image,
    lines: &[(usize, bool, String)],
    first: &[usize],
    recs: &[Rec],
    i: usize,
    seen: &[Option<usize>],
    w: usize,
    h: usize,
) -> Vec<Line> {
    let cur = recs[i].bundle;
    let anchor = first.get(cur).copied().unwrap_or(0);
    let start = anchor
        .saturating_sub(h / 3)
        .min(lines.len().saturating_sub(h));
    let mut out = vec![header(&clip(
        &format!(
            "CODE  {}",
            func_at(img, cur)
                .map(|f| img.func_label(f))
                .unwrap_or_default()
        ),
        w,
    ))];
    for (b, is_header, text) in lines.iter().skip(start).take(h.saturating_sub(1)) {
        let visited = seen[*b].is_some_and(|f| f <= i);
        let l = match (*b == cur && !is_header, visited) {
            (true, _) => line().put(HERE, "\u{25b8} "),
            (_, true) => line().put(DIM, "\u{00b7} "),
            _ => line().raw("  "),
        };
        let body = clip(text, w.saturating_sub(2));
        out.push(match (*b == cur, is_header) {
            (true, false) => l.put(BOLD, &body),
            (_, true) => l.put(HEAD, &body),
            _ => l.put(DIM, &body),
        });
    }
    out
}

fn belt_pane(img: &Image, recs: &[Rec], i: usize, v: &View, w: usize, h: usize) -> Vec<Line> {
    let r = &recs[i];
    let ops = ops_at(img, r.bundle);

    // Which slots read which entry position (all reads see the entry belt).
    let mut reads: Vec<Vec<&str>> = vec![Vec::new(); BELT_MAX];
    for (slot, op) in &ops {
        for p in op.belt_reads() {
            if (p as usize) < BELT_MAX && !reads[p as usize].contains(&slot.tag()) {
                reads[p as usize].push(slot.tag());
            }
        }
    }

    // `conform`/`rescue` rewrite the belt last, so each exit position names
    // where it was picked up from (PRD §7.2).
    let reshape: Vec<Option<usize>> = match ops.iter().find(|(_, op)| op.is_reshape()) {
        Some((_, Op::Conform(list))) => list.iter().map(|p| Some(*p as usize)).collect(),
        Some((_, Op::Rescue(m))) => (0..BELT_MAX)
            .filter(|i| m & (1 << i) != 0)
            .map(Some)
            .collect(),
        _ => Vec::new(),
    };

    let top = |live: u16| (0..BELT_MAX).rev().find(|p| live & (1 << p) != 0);
    let shown = top(r.live_in)
        .into_iter()
        .chain(top(r.live_out))
        .max()
        .map(|t| t + 1)
        .unwrap_or(1)
        .min(BELT_MAX)
        .min(h.saturating_sub(2).max(1));

    let vw = (0..shown)
        .flat_map(|p| [fmt_val(r.belt_in[p], v.hex), fmt_val(r.belt[p], v.hex)])
        .map(|s| s.len())
        .max()
        .unwrap_or(4)
        .clamp(4, 20);

    let mut out = vec![header(&format!("BELT  frame {}", r.frame))
        .gap(vw)
        .put(DIM, "entry")
        .gap(5 + vw + 7 + vw - 4)
        .put(DIM, "exit")];
    for p in 0..shown {
        let mut l = line().put(DIM, &format!(" b{p:<2} "));
        l = match r.live_in & (1 << p) != 0 {
            true => l.raw(&format!("{:>vw$}", fmt_val(r.belt_in[p], v.hex))),
            false => l.put(DIM, &format!("{:>vw$}", "\u{00b7}")),
        };
        l = match reads[p].is_empty() {
            true => l.raw("      "),
            false => l.put(
                READ,
                &format!("{:>6}", format!("\u{2190}{}", reads[p].join(","))),
            ),
        };
        // Drops land at the bottom of the belt in issue order: the last one
        // dropped is b0.
        let from_drop = (r.drops.len() > p).then(|| r.drops.len() - 1 - p);
        l = match (r.live_out & (1 << p) != 0, from_drop) {
            (true, Some(_)) => l.put(NEW, &format!(" {:>vw$}", fmt_val(r.belt[p], v.hex))),
            (true, None) => l.raw(&format!(" {:>vw$}", fmt_val(r.belt[p], v.hex))),
            (false, _) => l.put(DIM, &format!(" {:>vw$}", "\u{00b7}")),
        };
        if let Some(d) = from_drop {
            let (_, slot, age) = r.drops[d];
            let op = match age {
                0 => ops
                    .iter()
                    .find(|(s, _)| *s == slot)
                    .map(|(_, op)| render_op(img, op))
                    .unwrap_or_default(),
                _ => issuer(img, recs, i, age, slot),
            };
            let note = match age {
                0 => format!("  {} {op}", slot.tag()),
                _ => format!("  {} {op}  (-{age})", slot.tag()),
            };
            let room = w.saturating_sub(l.w);
            l = l.put(NEW, &clip(&note, room));
        } else if let Some(Some(src)) = reshape.get(p) {
            // Reshape reads the post-drop belt, which is the entry belt only
            // when nothing landed this bundle.
            let when = match r.drops.is_empty() {
                true => "",
                false => "  (post-drop)",
            };
            let room = w.saturating_sub(l.w);
            l = l.put(READ, &clip(&format!("  \u{2190} b{src}{when}"), room));
        }
        out.push(l);
    }
    out
}

fn stacked_panes(img: &Image, recs: &[Rec], i: usize, acc: &Acc, v: &View, w: usize) -> Vec<Line> {
    let r = &recs[i];
    let mut out = Vec::new();

    if !r.flight.is_empty() {
        out.push(line());
        out.push(header("IN FLIGHT"));
        let mut f = r.flight.clone();
        f.sort();
        for (bundles, slot, n) in f {
            let vals = match n {
                1 => String::new(),
                n => format!(" ({n} values)"),
            };
            out.push(line().raw(&clip(
                &format!(" {:<3} lands in {bundles}{vals}", slot.tag()),
                w,
            )));
        }
    }

    out.push(line());
    out.push(header(&format!("STACK  depth {}", acc.stack.len() - 1)));
    // Deep recursion is mostly the same frame over and over; keep the ends.
    let hidden = acc.stack.len().saturating_sub(7);
    for (d, bundle) in acc.stack.iter().enumerate().rev() {
        if hidden > 0 && d == acc.stack.len() - 6 {
            out.push(line().put(DIM, &format!("  \u{2026} {hidden} more")));
        }
        if hidden > 0 && d > 0 && d <= acc.stack.len() - 6 {
            continue;
        }
        let name = func_at(img, *bundle)
            .map(|f| img.func_label(f))
            .unwrap_or_default();
        let ebb = img
            .ebb_containing(*bundle as u32)
            .map(|e| img.ebb_label(e))
            .filter(|e| *e != name)
            .map(|e| format!(" in {e}"))
            .unwrap_or_default();
        let text = format!(" #{d} {name}  bundle {bundle}{ebb}");
        out.push(match d == r.frame {
            true => line().put(BOLD, &clip(&text, w)),
            false => line().put(DIM, &clip(&text, w)),
        });
    }

    let scratch = acc.scratch.get(r.frame);
    if scratch.is_some_and(|s| !s.is_empty()) {
        out.push(line());
        out.push(header("SCRATCH"));
        let touched: Vec<String> = scratch
            .unwrap()
            .iter()
            .map(|(s, val)| format!("s{s}={}", fmt_val(*val, v.hex)))
            .collect();
        for chunk in touched.chunks(4) {
            out.push(line().raw(&clip(&format!(" {}", chunk.join("  ")), w)));
        }
    }
    out
}

/// Where the memory window sits: pinned, else the last store, else whatever
/// `.data` put there.
fn mem_base(acc: &Acc, v: &View) -> Option<u64> {
    v.mem_at
        .or_else(|| acc.last_store.map(|a| a & !0xf))
        .or_else(|| acc.mem.keys().next().map(|a| a & !0xf))
}

fn mem_pane(acc: &Acc, v: &View, w: usize, rows: usize) -> Vec<Line> {
    let bpr: u64 = if w >= 78 { 16 } else { 8 };
    let Some(base) = mem_base(acc, v) else {
        return Vec::new();
    };
    let follow = match v.mem_at {
        Some(_) => "",
        None => "  (following stores)",
    };
    let mut out = vec![header(&clip(&format!("MEMORY{follow}"), w))];
    for row in 0..rows {
        let addr = base.wrapping_add(bpr * row as u64);
        let bytes: Vec<Option<u8>> = (0..bpr)
            .map(|k| acc.mem.get(&(addr + k)).copied())
            .collect();
        if bytes.iter().all(|b| b.is_none()) {
            continue;
        }
        let hex: Vec<String> = bytes
            .iter()
            .map(|b| match b {
                Some(b) => format!("{b:02x}"),
                None => "..".into(),
            })
            .collect();
        let ascii: String = bytes
            .iter()
            .map(|b| match b {
                Some(c) if c.is_ascii_graphic() || *c == b' ' => *c as char,
                _ => '.',
            })
            .collect();
        let hit = acc.last_store.is_some_and(|a| a >= addr && a < addr + bpr);
        let text = format!(" {addr:#010x}  {}  {ascii}", hex.join(" "));
        out.push(match hit {
            true => line().raw(&clip(&text, w)),
            false => line().put(DIM, &clip(&text, w)),
        });
    }
    out
}

fn out_pane(acc: &Acc, w: usize, rows: usize) -> Vec<Line> {
    if acc.out.is_empty() {
        return Vec::new();
    }
    let mut out = vec![header("OUTPUT")];
    let text: Vec<&str> = acc.out.split('\n').collect();
    for l in text.iter().rev().take(rows).rev() {
        out.push(line().raw(&clip(&format!(" {l}"), w)));
    }
    out
}

/// Frame depth over the whole run, one column per screen cell, with the cursor
/// showing where in it we are.
fn timeline(recs: &[Rec], i: usize, w: usize) -> Line {
    let bars = [
        ' ', '\u{2581}', '\u{2583}', '\u{2585}', '\u{2586}', '\u{2588}',
    ];
    let maxd = recs.iter().map(|r| r.frame).max().unwrap_or(0);
    let n = recs.len();
    let mut cols = vec![0usize; w];
    for (c, col) in cols.iter_mut().enumerate() {
        let lo = c * n / w;
        let hi = (((c + 1) * n).div_ceil(w)).clamp(lo + 1, n);
        *col = recs[lo..hi].iter().map(|r| r.frame + 1).max().unwrap_or(0);
    }
    let here = i * w / n;
    let mut l = line();
    for (c, d) in cols.iter().enumerate() {
        // Depth 0 is a low track rather than a full block, so the bar reads as
        // a scrubber even for a program that never calls anything.
        let step = match maxd {
            0 => 1,
            _ => 1 + d.saturating_sub(1) * (bars.len() - 2) / maxd,
        };
        let ch = bars[step.min(bars.len() - 1)];
        let s = ch.to_string();
        l = match c == here {
            true => l.put(BAR, &s),
            false => l.put(DIM, &s),
        };
    }
    l
}

fn help_pane(w: usize) -> Vec<Line> {
    let mut out = vec![header("KEYS"), line()];
    for (k, what) in KEYS {
        out.push(line().put(BOLD, &format!("  {k:<12}")).raw(&clip(what, w)));
    }
    out.push(line());
    out.push(line().put(DIM, "  any other key returns to the trace"));
    out
}

fn draw(
    img: &Image,
    recs: &[Rec],
    i: usize,
    acc: &Acc,
    v: &View,
    lines: &[(usize, bool, String)],
    first: &[usize],
    seen: &[Option<usize>],
    title: &str,
    w: usize,
    h: usize,
) -> String {
    let r = &recs[i];
    let ebb = img
        .ebb_containing(r.bundle as u32)
        .map(|e| img.ebb_label(e))
        .unwrap_or_default();
    let bar = format!(
        " mview  {title}   cycle {}/{}   bundle {} in {ebb}   frame {} ",
        r.cycle,
        recs.last().map(|r| r.cycle).unwrap_or(0),
        r.bundle,
        r.frame,
    );
    let mut screen = vec![line().put(BAR, &format!("{:<w$}", clip(&bar, w)))];

    if w < 50 || h < 8 {
        return format!("\x1b[H\x1b[2J mview needs a bigger window ({w}x{h})\r\n");
    }
    let body = h.saturating_sub(3);
    let left_w = (w * 46 / 100).clamp(24, 60).min(w.saturating_sub(20));
    let right_w = w.saturating_sub(left_w + 3);

    let (mut left, mut right) = if v.help {
        (help_pane(left_w), Vec::new())
    } else {
        let mem = mem_pane(acc, v, left_w, 4);
        let outp = out_pane(acc, left_w, 3);
        let code_h = body.saturating_sub(mem.len() + outp.len() + 2);
        let mut left = code_pane(img, lines, first, recs, i, seen, left_w, code_h);
        for pane in [mem, outp] {
            if !pane.is_empty() {
                left.push(line());
                left.extend(pane);
            }
        }
        let mut right = belt_pane(img, recs, i, v, right_w, body * 2 / 3);
        right.extend(stacked_panes(img, recs, i, acc, v, right_w));
        (left, right)
    };
    left.resize(body, line());
    right.resize(body, line());

    for (l, rt) in left.into_iter().zip(right) {
        screen.push(
            l.pad(left_w)
                .put(DIM, " \u{2502} ")
                .raw(&rt.s)
                .pad(w.saturating_sub(1)),
        );
    }
    screen.push(timeline(recs, i, w));
    screen.push(line().put(
        DIM,
        &clip(
            " \u{2190}\u{2192} step  \u{2191}\u{2193} x10  n/p same bundle  o over  u out  g/G first/last  ? keys  q quit",
            w,
        ),
    ));

    // Home, then erase each line as it is written and the rest at the end: a
    // full clear first would flicker. No newline after the last row, which on a
    // full screen would scroll the title off the top.
    let mut s = String::from("\x1b[H");
    for (n, l) in screen.iter().take(h).enumerate() {
        if n > 0 {
            s.push_str("\r\n");
        }
        s.push_str(&l.s);
        s.push_str("\x1b[K");
    }
    s.push_str("\x1b[J");
    s
}

// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let mut path = None;
    let mut trace_path = None;
    let mut max_bundles = 200_000u64;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--max-bundles" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse().ok()) {
                    Some(n) => max_bundles = n,
                    None => {
                        eprintln!("mview: --max-bundles needs a number");
                        return ExitCode::from(2);
                    }
                }
            }
            a if a.starts_with("--") => {
                eprintln!("mview: unknown option `{a}`\n{USAGE}");
                return ExitCode::from(2);
            }
            a if path.is_none() => path = Some(a.to_string()),
            a => trace_path = Some(a.to_string()),
        }
        i += 1;
    }
    let Some(path) = path else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    COLOR.set(std::env::var_os("NO_COLOR").is_none()).ok();

    let img = match std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|b| Image::from_bytes(&b).map_err(|e| e.to_string()))
    {
        Ok(i) => i,
        Err(e) => {
            eprintln!("mview: {path}: {e}");
            return ExitCode::from(2);
        }
    };

    let (text, title) = match trace_path.as_deref() {
        Some("-") => {
            let mut s = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut s) {
                eprintln!("mview: stdin: {e}");
                return ExitCode::from(2);
            }
            (s, format!("{path} + stdin"))
        }
        Some(t) => match std::fs::read_to_string(t) {
            Ok(s) => (s, format!("{path} + {t}")),
            Err(e) => {
                eprintln!("mview: {t}: {e}");
                return ExitCode::from(2);
            }
        },
        None => {
            let run = run_capture(
                &img,
                Options {
                    trace_json: true,
                    max_bundles,
                    ..Default::default()
                },
            );
            let stop = match &run.stop {
                Stop::Exit(c) => format!("exit {c}"),
                Stop::Halt => "halted".into(),
                Stop::Fault(m) => format!("fault: {m}"),
            };
            (
                String::from_utf8_lossy(&run.log).into_owned(),
                format!("{path}  [{stop}]"),
            )
        }
    };

    let (recs, errs) = parse_trace(&text);
    for e in errs.iter().take(5) {
        eprintln!("mview: {e}");
    }
    if recs.is_empty() {
        eprintln!("mview: no trace records (did you pass --trace-json?)");
        return ExitCode::from(2);
    }

    let (lines, first) = listing(&img);
    let mut seen: Vec<Option<usize>> = vec![None; img.bundles.len()];
    for (j, r) in recs.iter().enumerate() {
        if r.bundle < seen.len() && seen[r.bundle].is_none() {
            seen[r.bundle] = Some(j);
        }
    }

    let mut term = match Term::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mview: no terminal: {e}");
            return ExitCode::from(2);
        }
    };
    let mut acc = Acc::default();
    acc.reset(&img);
    let mut v = View {
        hex: false,
        help: false,
        mem_at: None,
    };
    let mut i = 0usize;
    let last = recs.len() - 1;

    loop {
        acc.seek(&img, &recs, i);
        let (w, h) = term.size();
        let s = draw(
            &img, &recs, i, &acc, &v, &lines, &first, &seen, &title, w, h,
        );
        print!("{s}");
        let _ = std::io::stdout().flush();

        let Some(k) = term.key() else { break };
        let step = |n: isize| (i as isize + n).clamp(0, last as isize) as usize;
        let scan = |pred: &dyn Fn(&Rec) -> bool, back: bool| match back {
            false => (i + 1..=last).find(|j| pred(&recs[*j])).unwrap_or(i),
            true => (0..i).rev().find(|j| pred(&recs[*j])).unwrap_or(i),
        };
        if v.help {
            v.help = false;
            if matches!(k, Key::Char('q')) {
                break;
            }
            continue;
        }
        i = match k {
            Key::Char('q') | Key::Char('\u{3}') => break,
            Key::Char('?') => {
                v.help = true;
                i
            }
            Key::Char('x') => {
                v.hex = !v.hex;
                i
            }
            Key::Char('[') => {
                v.mem_at = Some(mem_base(&acc, &v).unwrap_or(0).wrapping_sub(16));
                i
            }
            Key::Char(']') => {
                v.mem_at = Some(mem_base(&acc, &v).unwrap_or(0).wrapping_add(16));
                i
            }
            Key::Char('=') => {
                v.mem_at = None;
                i
            }
            Key::Right | Key::Char('l') | Key::Char(' ') => step(1),
            Key::Left | Key::Char('h') => step(-1),
            Key::Down | Key::Char('L') => step(10),
            Key::Up | Key::Char('H') => step(-10),
            Key::PgDn => step(100),
            Key::PgUp => step(-100),
            Key::Home | Key::Char('g') => 0,
            Key::End | Key::Char('G') => last,
            Key::Char('n') => {
                let b = recs[i].bundle;
                scan(&|r| r.bundle == b, false)
            }
            Key::Char('p') => {
                let b = recs[i].bundle;
                scan(&|r| r.bundle == b, true)
            }
            Key::Char('o') => {
                let d = recs[i].frame;
                scan(&|r| r.frame <= d, false)
            }
            Key::Char('u') => {
                let d = recs[i].frame;
                scan(&|r| r.frame < d, false)
            }
            _ => i,
        };
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use millet_core::{DataSeg, EbbEntry, FuncEntry};

    #[test]
    fn json_reads_what_the_simulator_writes() {
        let line = r#"{"cycle":11,"frame":0,"bundle":2,"live_in":7,"live_out":15,"belt_in":[1,4096,14],"belt":[14,1,4096,14],"drops":[{"v":14,"slot":"f","age":0}],"mem":[{"a":4096,"w":8,"v":255}],"scr":[{"s":3,"v":9}],"flight":[{"in":2,"slot":"a0","n":1}],"out":"hi \"x\"\n"}"#;
        let (recs, errs) = parse_trace(line);
        assert!(errs.is_empty(), "{errs:?}");
        let r = &recs[0];
        assert_eq!((r.cycle, r.frame, r.bundle), (11, 0, 2));
        assert_eq!(r.belt_in[1], 4096);
        assert_eq!(r.belt[0], 14);
        assert_eq!(r.drops, vec![(14, Slot::F, 0)]);
        assert_eq!(r.mem, vec![(4096, 8, 255)]);
        assert_eq!(r.scr, vec![(3, 9)]);
        assert_eq!(r.flight, vec![(2, Slot::A0, 1)]);
        assert_eq!(r.out, "hi \"x\"\n");
        // A half-written last line is skipped, not fatal.
        let (recs, errs) = parse_trace(&format!("{line}\n{{\"cycle\":12,\"bun"));
        assert_eq!(recs.len(), 1);
        assert_eq!(errs.len(), 1);
    }

    fn demo_image() -> Image {
        Image {
            bundles: vec![[0, 0, 0, 0]; 4],
            ebbs: vec![EbbEntry {
                bundle: 0,
                arity: 0,
                name: "main".into(),
            }],
            funcs: vec![FuncEntry {
                ebb: 0,
                arity: 0,
                nres: 0,
                name: "main".into(),
            }],
            data: vec![DataSeg {
                addr: 0x1000,
                bytes: vec![7, 7],
            }],
            entry: 0,
        }
    }

    #[test]
    fn state_replays_forwards_and_backwards() {
        let recs = parse_trace(
            r#"{"cycle":0,"frame":0,"bundle":0,"live_in":0,"live_out":0,"belt_in":[],"belt":[],"scr":[{"s":1,"v":5}]}
{"cycle":1,"frame":0,"bundle":1,"live_in":0,"live_out":0,"belt_in":[],"belt":[],"mem":[{"a":4096,"w":2,"v":513}],"out":"a"}
{"cycle":2,"frame":1,"bundle":2,"live_in":0,"live_out":0,"belt_in":[],"belt":[],"scr":[{"s":1,"v":9}]}
{"cycle":3,"frame":0,"bundle":3,"live_in":0,"live_out":0,"belt_in":[],"belt":[],"out":"b"}"#,
        )
        .0;
        let img = demo_image();
        let mut acc = Acc::default();
        acc.reset(&img);

        acc.seek(&img, &recs, 3);
        assert_eq!(acc.out, "ab");
        assert_eq!(acc.mem[&0x1000], 1, ".data seeds memory, the store wins");
        assert_eq!(acc.mem[&0x1001], 2);
        assert_eq!(acc.scratch[0][&1], 5, "the callee's spill is its own");
        assert_eq!(acc.stack, vec![3], "the callee frame is gone");

        // Seeking back must not leave the forward state behind.
        acc.seek(&img, &recs, 0);
        assert_eq!(acc.out, "");
        assert_eq!(acc.mem[&0x1000], 7);
        assert_eq!(acc.scratch[0][&1], 5);
        acc.seek(&img, &recs, 2);
        assert_eq!(acc.stack, vec![1, 2]);
        assert_eq!(acc.scratch[1][&1], 9);
    }

    #[test]
    fn a_screen_is_drawn_within_its_width() {
        let img = demo_image();
        let recs = parse_trace(
            r#"{"cycle":0,"frame":0,"bundle":0,"live_in":0,"live_out":3,"belt_in":[0,0],"belt":[42,7],"drops":[{"v":7,"slot":"a0","age":0},{"v":42,"slot":"a1","age":0}],"mem":[{"a":4096,"w":1,"v":9}],"out":"hi\n"}"#,
        )
        .0;
        let (lines, first) = listing(&img);
        let mut acc = Acc::default();
        acc.reset(&img);
        acc.seek(&img, &recs, 0);
        let v = View {
            hex: false,
            help: false,
            mem_at: None,
        };
        for w in [80usize, 100, 200] {
            let s = draw(
                &img,
                &recs,
                0,
                &acc,
                &v,
                &lines,
                &first,
                &[Some(0); 4],
                "t.mimg",
                w,
                24,
            );
            let rows: Vec<&str> = s.split("\r\n").collect();
            assert_eq!(rows.len(), 24, "one row per terminal line, no more");
            for row in rows {
                let visible = strip_ansi(row);
                assert!(
                    visible.chars().count() <= w,
                    "row `{visible}` is {} wide, over {w}",
                    visible.chars().count()
                );
            }
            assert!(strip_ansi(&s).contains("42"), "the belt is shown");
        }
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            for c in it.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        out
    }

    /// The whole screen for a real program, so a broken pane shows up as a
    /// diff here. `MVIEW_DUMP=1 cargo test -p millet-sim --bin mview -- --nocapture`
    /// prints it.
    #[test]
    fn a_real_program_draws_every_pane() {
        let src = std::fs::read_to_string("../examples/arraysum.mil").unwrap();
        let a = millet_asm::asm::assemble(&src, &millet_core::Config::default()).unwrap();
        let run = run_capture(
            &a.image,
            Options {
                trace_json: true,
                ..Default::default()
            },
        );
        let (recs, errs) = parse_trace(&String::from_utf8_lossy(&run.log));
        assert!(errs.is_empty(), "{errs:?}");
        let img = a.image;
        let (lines, first) = listing(&img);
        let seen: Vec<Option<usize>> = (0..img.bundles.len())
            .map(|b| recs.iter().position(|r| r.bundle == b))
            .collect();
        let mut acc = Acc::default();
        acc.reset(&img);
        let at = recs.iter().position(|r| r.bundle == 9).unwrap();
        acc.seek(&img, &recs, at);
        let v = View {
            hex: false,
            help: false,
            mem_at: None,
        };
        let s = draw(
            &img, &recs, at, &acc, &v, &lines, &first, &seen, "arraysum", 118, 34,
        );
        if std::env::var_os("MVIEW_DUMP").is_some() {
            println!("{}", s.replace("\x1b[K", ""));
        }
        let plain = strip_ansi(&s);
        for want in [
            "bundle 9 in as_loop",  // the title tracks the record
            "f   rescue 0x0013",    // the code pane found the bundle
            "\u{2190} b4",          // rescue provenance on the exit belt
            "#1 sum  bundle 9",     // the call stack was reconstructed
            "0x00001000  0a 00 00", // .data seeded the memory pane
        ] {
            assert!(
                plain.contains(want),
                "the screen is missing `{want}`:\n{plain}"
            );
        }
    }
}
