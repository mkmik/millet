//! `mview` — a terminal viewer for `msim --trace-json` traces.
//!
//! The trace is a full per-bundle state record, so the whole run is in memory
//! and time is just an index: every key moves the cursor, and the screen is
//! redrawn from the record it lands on. Memory, scratch and program output are
//! the only accumulated state, and those are replayed from the start.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::ExitCode;
use std::sync::OnceLock;

use millet_core::isa::{decode, format_op, Op, Slot};
use millet_core::{Image, BELT_MAX};
use millet_sim::{run_capture, Options, Stop};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, List, ListItem, ListState, Padding, Paragraph, Row, Table, Wrap,
};
use ratatui::Frame;

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
// Screen
// ---------------------------------------------------------------------------

static COLOR: OnceLock<bool> = OnceLock::new();

/// Every style goes through here so `NO_COLOR` can flatten it.
fn st(s: Style) -> Style {
    match COLOR.get() {
        Some(false) => Style::new(),
        _ => s,
    }
}

fn dim() -> Style {
    st(Style::new().add_modifier(Modifier::DIM))
}
fn bold() -> Style {
    st(Style::new().add_modifier(Modifier::BOLD))
}
fn head() -> Style {
    st(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}
fn new_val() -> Style {
    st(Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))
}
fn read_mark() -> Style {
    st(Style::new().fg(Color::Yellow))
}
fn here() -> Style {
    st(Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD))
}
fn bar() -> Style {
    st(Style::new().add_modifier(Modifier::REVERSED))
}

fn pane(title: impl Into<String>) -> Block<'static> {
    Block::new().title(Span::styled(title.into(), head()))
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

/// Everything the screen is drawn from. Time is `i`; the rest is what the user
/// has toggled.
struct App {
    img: Image,
    recs: Vec<Rec>,
    acc: Acc,
    lines: Vec<(usize, bool, String)>,
    first: Vec<usize>,
    seen: Vec<Option<usize>>,
    title: String,
    i: usize,
    hex: bool,
    help: bool,
    /// Base address of the memory window, or None while it follows stores.
    mem_at: Option<u64>,
    code: ListState,
}

impl App {
    fn rec(&self) -> &Rec {
        &self.recs[self.i]
    }

    fn last(&self) -> usize {
        self.recs.len() - 1
    }

    /// Where the memory window sits: pinned, else the last store, else
    /// whatever `.data` put there.
    fn mem_base(&self) -> Option<u64> {
        self.mem_at
            .or_else(|| self.acc.last_store.map(|a| a & !0xf))
            .or_else(|| self.acc.mem.keys().next().map(|a| a & !0xf))
    }

    fn title_bar(&self) -> Paragraph<'static> {
        let r = self.rec();
        let ebb = self
            .img
            .ebb_containing(r.bundle as u32)
            .map(|e| self.img.ebb_label(e))
            .unwrap_or_default();
        Paragraph::new(format!(
            " mview  {}   cycle {}/{}   bundle {} in {ebb}   frame {} ",
            self.title,
            r.cycle,
            self.recs[self.last()].cycle,
            r.bundle,
            r.frame,
        ))
        .style(bar())
    }

    fn code(&self) -> List<'static> {
        let cur = self.rec().bundle;
        let items = self.lines.iter().map(|(b, is_header, text)| {
            let visited = self.seen[*b].is_some_and(|f| f <= self.i);
            let gutter = match (*b == cur && !is_header, visited) {
                (true, _) => Span::styled("\u{25b8} ", here()),
                (_, true) => Span::styled("\u{00b7} ", dim()),
                _ => Span::raw("  "),
            };
            let style = match (*b == cur, is_header) {
                (true, false) => bold(),
                (_, true) => head(),
                _ => dim(),
            };
            ListItem::new(Line::from(vec![gutter, Span::styled(text.clone(), style)]))
        });
        let name = func_at(&self.img, cur)
            .map(|f| self.img.func_label(f))
            .unwrap_or_default();
        List::new(items).block(pane(format!("CODE  {name}")))
    }

    /// Both belts side by side: what the ops read, and what they left behind.
    fn belt(&self) -> (Table<'static>, usize) {
        let r = self.rec();
        let ops = ops_at(&self.img, r.bundle);

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
            .unwrap_or(1);

        let cell = |s: String, style: Style| Cell::from(Line::from(s).right_aligned().style(style));
        let rows = (0..shown).map(|p| {
            let entry = match r.live_in & (1 << p) != 0 {
                true => cell(fmt_val(r.belt_in[p], self.hex), Style::new()),
                false => cell("\u{00b7}".into(), dim()),
            };
            let read = match reads[p].is_empty() {
                true => Cell::default(),
                false => Cell::from(format!("\u{2190}{}", reads[p].join(","))).style(read_mark()),
            };
            // Drops land at the bottom of the belt in issue order: the last one
            // dropped is b0.
            let from_drop = (r.drops.len() > p).then(|| r.drops.len() - 1 - p);
            let exit = match (r.live_out & (1 << p) != 0, from_drop) {
                (true, Some(_)) => cell(fmt_val(r.belt[p], self.hex), new_val()),
                (true, None) => cell(fmt_val(r.belt[p], self.hex), Style::new()),
                (false, _) => cell("\u{00b7}".into(), dim()),
            };
            let note = match (from_drop, reshape.get(p)) {
                (Some(d), _) => {
                    let (_, slot, age) = r.drops[d];
                    let op = match age {
                        0 => ops
                            .iter()
                            .find(|(s, _)| *s == slot)
                            .map(|(_, op)| render_op(&self.img, op))
                            .unwrap_or_default(),
                        _ => issuer(&self.img, &self.recs, self.i, age, slot),
                    };
                    let when = match age {
                        0 => String::new(),
                        _ => format!("  (-{age})"),
                    };
                    Cell::from(format!("{} {op}{when}", slot.tag())).style(new_val())
                }
                // Reshape reads the post-drop belt, which is the entry belt
                // only when nothing landed this bundle.
                (None, Some(Some(src))) => Cell::from(match r.drops.is_empty() {
                    true => format!("\u{2190} b{src}"),
                    false => format!("\u{2190} b{src}  (post-drop)"),
                })
                .style(read_mark()),
                _ => Cell::default(),
            };
            Row::new(vec![
                Cell::from(format!("b{p}")).style(dim()),
                entry,
                read,
                exit,
                note,
            ])
        });

        let vw = (0..shown)
            .flat_map(|p| {
                [
                    fmt_val(r.belt_in[p], self.hex),
                    fmt_val(r.belt[p], self.hex),
                ]
            })
            .map(|s| s.len() as u16)
            .max()
            .unwrap_or(4)
            .clamp(4, 20);
        let widths = [
            Constraint::Length(4),
            Constraint::Length(vw),
            Constraint::Length(7),
            Constraint::Length(vw),
            Constraint::Min(0),
        ];
        let title = format!("BELT  frame {}", r.frame);
        let table = Table::new(rows, widths).block(
            pane(title).title(Span::styled("entry \u{2192} exit", dim()).into_right_aligned_line()),
        );
        (table, shown + 1)
    }

    fn flight(&self) -> Paragraph<'static> {
        let mut f = self.rec().flight.clone();
        f.sort();
        let text: Vec<Line> = f
            .iter()
            .map(|(bundles, slot, n)| {
                let vals = match n {
                    1 => String::new(),
                    n => format!(" ({n} values)"),
                };
                Line::raw(format!(" {:<3} lands in {bundles}{vals}", slot.tag()))
            })
            .collect();
        Paragraph::new(text).block(pane("IN FLIGHT"))
    }

    fn stack(&self) -> (Paragraph<'static>, usize) {
        let depth = self.rec().frame;
        // Deep recursion is mostly the same frame over and over; keep the ends.
        let hidden = self.acc.stack.len().saturating_sub(7);
        let mut text = Vec::new();
        for (d, bundle) in self.acc.stack.iter().enumerate().rev() {
            if hidden > 0 && d == self.acc.stack.len() - 6 {
                text.push(Line::styled(format!("  \u{2026} {hidden} more"), dim()));
            }
            if hidden > 0 && d > 0 && d <= self.acc.stack.len() - 6 {
                continue;
            }
            let name = func_at(&self.img, *bundle)
                .map(|f| self.img.func_label(f))
                .unwrap_or_default();
            let ebb = self
                .img
                .ebb_containing(*bundle as u32)
                .map(|e| self.img.ebb_label(e))
                .filter(|e| *e != name)
                .map(|e| format!(" in {e}"))
                .unwrap_or_default();
            let line = format!(" #{d} {name}  bundle {bundle}{ebb}");
            text.push(match d == depth {
                true => Line::styled(line, bold()),
                false => Line::styled(line, dim()),
            });
        }
        let n = text.len();
        let title = format!("STACK  depth {}", self.acc.stack.len() - 1);
        (Paragraph::new(text).block(pane(title)), n + 1)
    }

    fn scratch(&self) -> (Paragraph<'static>, usize) {
        let touched: Vec<String> = self
            .acc
            .scratch
            .get(self.rec().frame)
            .into_iter()
            .flatten()
            .map(|(s, val)| format!("s{s}={}", fmt_val(*val, self.hex)))
            .collect();
        let text: Vec<Line> = touched
            .chunks(4)
            .map(|c| Line::raw(format!(" {}", c.join("  "))))
            .collect();
        // Nothing spilled in this frame yet: no pane at all, not an empty one.
        let h = match text.is_empty() {
            true => 0,
            false => text.len() + 1,
        };
        (Paragraph::new(text).block(pane("SCRATCH")), h)
    }

    fn memory(&self, rows: usize, wide: bool) -> Paragraph<'static> {
        let bpr: u64 = if wide { 16 } else { 8 };
        let text: Vec<Line> = self
            .mem_base()
            .into_iter()
            .flat_map(|base| (0..rows as u64).map(move |row| base.wrapping_add(bpr * row)))
            .filter_map(|addr| {
                let bytes: Vec<Option<u8>> = (0..bpr)
                    .map(|k| self.acc.mem.get(&(addr + k)).copied())
                    .collect();
                if bytes.iter().all(|b| b.is_none()) {
                    return None;
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
                let hit = self
                    .acc
                    .last_store
                    .is_some_and(|a| a >= addr && a < addr + bpr);
                let line = format!(" {addr:#010x}  {}  {ascii}", hex.join(" "));
                Some(match hit {
                    true => Line::raw(line),
                    false => Line::styled(line, dim()),
                })
            })
            .collect();
        let follow = match self.mem_at {
            Some(_) => "",
            None => "  (following stores)",
        };
        Paragraph::new(text).block(pane(format!("MEMORY{follow}")))
    }

    fn output(&self) -> Paragraph<'static> {
        Paragraph::new(self.acc.out.clone())
            .wrap(Wrap { trim: false })
            .block(pane("OUTPUT"))
    }

    /// Frame depth over the whole run, one column per screen cell, with the
    /// cursor showing where in it we are.
    fn timeline(&self, w: usize) -> Line<'static> {
        let bars = [
            ' ', '\u{2581}', '\u{2583}', '\u{2585}', '\u{2586}', '\u{2588}',
        ];
        let maxd = self.recs.iter().map(|r| r.frame).max().unwrap_or(0);
        let n = self.recs.len();
        let here_col = self.i * w / n;
        let spans = (0..w).map(|c| {
            let lo = c * n / w;
            let hi = (((c + 1) * n).div_ceil(w)).clamp(lo + 1, n);
            let d = self.recs[lo..hi]
                .iter()
                .map(|r| r.frame + 1)
                .max()
                .unwrap_or(0);
            // Depth 0 is a low track rather than a full block, so the bar reads
            // as a scrubber even for a program that never calls anything.
            let step = match maxd {
                0 => 1,
                _ => 1 + d.saturating_sub(1) * (bars.len() - 2) / maxd,
            };
            let sym = bars[step.min(bars.len() - 1)].to_string();
            match c == here_col {
                true => Span::styled(sym, bar()),
                false => Span::styled(sym, dim()),
            }
        });
        Line::from(spans.collect::<Vec<_>>())
    }

    fn render(&mut self, f: &mut Frame) {
        let [title, body, tl, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());

        f.render_widget(self.title_bar(), title);
        f.render_widget(self.timeline(tl.width as usize), tl);
        f.render_widget(
            Paragraph::new(
                " \u{2190}\u{2192} step  \u{2191}\u{2193} x10  n/p same bundle  o over  u out  \
                 g/G first/last  ? keys  q quit",
            )
            .style(dim()),
            footer,
        );

        if self.help {
            let text: Vec<Line> = KEYS
                .iter()
                .map(|(k, what)| {
                    Line::from(vec![
                        Span::styled(format!("  {k:<12}"), bold()),
                        Span::raw(*what),
                    ])
                })
                .collect();
            f.render_widget(
                Paragraph::new(text).block(Block::bordered().title(Span::styled("KEYS", head()))),
                body,
            );
            return;
        }

        let [left, right] =
            Layout::horizontal([Constraint::Percentage(46), Constraint::Min(0)]).areas(body);
        let divider = Block::new()
            .borders(Borders::RIGHT)
            .border_style(dim())
            .padding(Padding::right(1));
        let inner = divider.inner(left);
        f.render_widget(divider, left);

        let mem_h = match self.mem_base() {
            Some(_) => 5,
            None => 0,
        };
        let out_h = match self.acc.out.is_empty() {
            true => 0,
            false => 4,
        };
        let [a_code, a_mem, a_out] = Layout::vertical([
            // The code always keeps a few rows; memory and output give way.
            Constraint::Min(4),
            Constraint::Max(mem_h),
            Constraint::Max(out_h),
        ])
        .spacing(1)
        .areas(inner);
        self.code.select(Some(self.first[self.rec().bundle]));
        let code = self.code();
        f.render_stateful_widget(code, a_code, &mut self.code);
        f.render_widget(
            self.memory(mem_h.saturating_sub(1) as usize, inner.width >= 78),
            a_mem,
        );
        f.render_widget(self.output(), a_out);

        // Each pane asks for exactly the rows it needs — a title plus content —
        // and the layout gives the leftovers to whatever comes last.
        let (belt, belt_h) = self.belt();
        let (stack, stack_h) = self.stack();
        let (scratch, scratch_h) = self.scratch();
        let flight_h = match self.rec().flight.is_empty() {
            true => 0,
            false => self.rec().flight.len() + 1,
        };
        let [a_belt, a_fl, a_st, a_sc, _] = Layout::vertical([
            Constraint::Length(belt_h as u16),
            Constraint::Length(flight_h as u16),
            Constraint::Length(stack_h as u16),
            Constraint::Length(scratch_h as u16),
            Constraint::Min(0),
        ])
        .spacing(1)
        .areas(right.inner(Margin::new(1, 0)));
        f.render_widget(belt, a_belt);
        f.render_widget(self.flight(), a_fl);
        f.render_widget(stack, a_st);
        f.render_widget(scratch, a_sc);
    }

    /// True to keep going.
    fn key(&mut self, k: KeyEvent) -> bool {
        if self.help {
            self.help = false;
            return k.code != KeyCode::Char('q');
        }
        let step = |i: usize, n: isize| (i as isize + n).clamp(0, self.last() as isize) as usize;
        let scan = |pred: &dyn Fn(&Rec) -> bool, back: bool| {
            let i = self.i;
            match back {
                false => (i + 1..=self.last())
                    .find(|j| pred(&self.recs[*j]))
                    .unwrap_or(i),
                true => (0..i).rev().find(|j| pred(&self.recs[*j])).unwrap_or(i),
            }
        };
        let bundle = self.rec().bundle;
        let depth = self.rec().frame;
        self.i = match k.code {
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Char('?') => {
                self.help = true;
                self.i
            }
            KeyCode::Char('x') => {
                self.hex = !self.hex;
                self.i
            }
            KeyCode::Char('[') => {
                self.mem_at = Some(self.mem_base().unwrap_or(0).wrapping_sub(16));
                self.i
            }
            KeyCode::Char(']') => {
                self.mem_at = Some(self.mem_base().unwrap_or(0).wrapping_add(16));
                self.i
            }
            KeyCode::Char('=') => {
                self.mem_at = None;
                self.i
            }
            KeyCode::Right | KeyCode::Char('l' | ' ') => step(self.i, 1),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => step(self.i, -1),
            KeyCode::Down | KeyCode::Char('L') => step(self.i, 10),
            KeyCode::Up | KeyCode::Char('H') => step(self.i, -10),
            KeyCode::PageDown => step(self.i, 100),
            KeyCode::PageUp => step(self.i, -100),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => self.last(),
            KeyCode::Char('n') => scan(&|r| r.bundle == bundle, false),
            KeyCode::Char('p') => scan(&|r| r.bundle == bundle, true),
            KeyCode::Char('o') => scan(&|r| r.frame <= depth, false),
            KeyCode::Char('u') => scan(&|r| r.frame < depth, false),
            _ => self.i,
        };
        true
    }
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

    let mut app = App::new(img, recs, title);
    let mut term = ratatui::init();
    let res = loop {
        app.acc.seek(&app.img, &app.recs, app.i);
        if let Err(e) = term.draw(|f| app.render(f)) {
            break Err(e);
        }
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if !app.key(k) {
                    break Ok(());
                }
            }
            Ok(_) => {}
            Err(e) => break Err(e),
        }
    };
    ratatui::restore();
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mview: {e}");
            ExitCode::from(2)
        }
    }
}

impl App {
    fn new(img: Image, recs: Vec<Rec>, title: String) -> App {
        let (lines, first) = listing(&img);
        let seen: Vec<Option<usize>> = (0..img.bundles.len())
            .map(|b| recs.iter().position(|r| r.bundle == b))
            .collect();
        let mut acc = Acc::default();
        acc.reset(&img);
        App {
            img,
            recs,
            acc,
            lines,
            first,
            seen,
            title,
            i: 0,
            hex: false,
            help: false,
            mem_at: None,
            code: ListState::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use millet_core::{DataSeg, EbbEntry, FuncEntry};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        app.acc.seek(&app.img, &app.recs, app.i);
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole screen for a real program, so a broken pane shows up here.
    /// `MVIEW_DUMP=1 cargo test -p millet-sim --bin mview -- --nocapture` prints it.
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
        let mut app = App::new(a.image, recs, "arraysum".into());
        app.i = app.recs.iter().position(|r| r.bundle == 9).unwrap();
        let s = screen(&mut app, 118, 34);
        if std::env::var_os("MVIEW_DUMP").is_some() {
            println!("{s}");
        }
        for want in [
            "bundle 9 in as_loop",  // the title tracks the record
            "f   rescue 0x0013",    // the code pane found the bundle
            "\u{2190} b4",          // rescue provenance on the exit belt
            "#1 sum  bundle 9",     // the call stack was reconstructed
            "0x00001000  0a 00 00", // .data seeded the memory pane
        ] {
            assert!(s.contains(want), "the screen is missing `{want}`:\n{s}");
        }
    }

    #[test]
    fn a_narrow_screen_still_draws() {
        let img = demo_image();
        let recs = parse_trace(
            r#"{"cycle":0,"frame":0,"bundle":0,"live_in":0,"live_out":3,"belt_in":[0,0],"belt":[42,7],"drops":[{"v":7,"slot":"a0","age":0},{"v":42,"slot":"a1","age":0}],"mem":[{"a":4096,"w":1,"v":9}],"out":"hi\n"}"#,
        )
        .0;
        let mut app = App::new(img, recs, "t.mimg".into());
        for (w, h) in [(40u16, 10u16), (80, 24), (200, 60)] {
            let s = screen(&mut app, w, h);
            if std::env::var_os("MVIEW_DUMP").is_some() {
                println!("{w}x{h}\n{s}\n");
            }
            assert_eq!(s.lines().count(), h as usize);
            assert!(s.lines().all(|l| l.chars().count() == w as usize));
        }
        assert!(screen(&mut app, 80, 24).contains("42"), "the belt is shown");
    }
}
