//! The Millet instruction set: opcode table, decoded operation form, encoding
//! and decoding (PRD §5, §10).
//!
//! Every slot is exactly 32 bits with the opcode in the top byte. Field
//! positions follow the encoding sketch in PRD §10 literally, reading each
//! `[a:8][b:4]...` left-to-right from bit 31 down.

use std::fmt;

pub const MAX_ARGS: usize = 3;
pub const MAX_CONFORM: usize = 6;

/// The four slots of a bundle, in the order they appear in the bundle and in
/// the drop-ordering rule of PRD §3.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    A0 = 0,
    A1 = 1,
    M = 2,
    F = 3,
}

impl Slot {
    pub const ALL: [Slot; 4] = [Slot::A0, Slot::A1, Slot::M, Slot::F];

    pub fn tag(self) -> &'static str {
        match self {
            Slot::A0 => "a0",
            Slot::A1 => "a1",
            Slot::M => "m",
            Slot::F => "f",
        }
    }

    pub fn from_tag(s: &str) -> Option<Slot> {
        match s {
            "a0" => Some(Slot::A0),
            "a1" => Some(Slot::A1),
            "m" => Some(Slot::M),
            "f" => Some(Slot::F),
            _ => None,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AluOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Sar,
    Mul,
    Div,
    Divu,
    Rem,
    Remu,
    Eq,
    Ne,
    Lt,
    Le,
    Ltu,
    Leu,
}

impl AluOp {
    pub fn opcode(self) -> u8 {
        match self {
            AluOp::Add => 0x01,
            AluOp::Sub => 0x02,
            AluOp::And => 0x03,
            AluOp::Or => 0x04,
            AluOp::Xor => 0x05,
            AluOp::Shl => 0x06,
            AluOp::Shr => 0x07,
            AluOp::Sar => 0x08,
            AluOp::Mul => 0x09,
            AluOp::Div => 0x0a,
            AluOp::Divu => 0x0b,
            AluOp::Rem => 0x0c,
            AluOp::Remu => 0x0d,
            AluOp::Eq => 0x0e,
            AluOp::Ne => 0x0f,
            AluOp::Lt => 0x10,
            AluOp::Le => 0x11,
            AluOp::Ltu => 0x12,
            AluOp::Leu => 0x13,
        }
    }

    pub fn from_opcode(op: u8) -> Option<AluOp> {
        use AluOp::*;
        Some(match op {
            0x01 => Add,
            0x02 => Sub,
            0x03 => And,
            0x04 => Or,
            0x05 => Xor,
            0x06 => Shl,
            0x07 => Shr,
            0x08 => Sar,
            0x09 => Mul,
            0x0a => Div,
            0x0b => Divu,
            0x0c => Rem,
            0x0d => Remu,
            0x0e => Eq,
            0x0f => Ne,
            0x10 => Lt,
            0x11 => Le,
            0x12 => Ltu,
            0x13 => Leu,
            _ => return None,
        })
    }

    pub fn mnemonic(self) -> &'static str {
        use AluOp::*;
        match self {
            Add => "add",
            Sub => "sub",
            And => "and",
            Or => "or",
            Xor => "xor",
            Shl => "shl",
            Shr => "shr",
            Sar => "sar",
            Mul => "mul",
            Div => "div",
            Divu => "divu",
            Rem => "rem",
            Remu => "remu",
            Eq => "eq",
            Ne => "ne",
            Lt => "lt",
            Le => "le",
            Ltu => "ltu",
            Leu => "leu",
        }
    }

    pub fn from_mnemonic(s: &str) -> Option<AluOp> {
        use AluOp::*;
        Some(match s {
            "add" => Add,
            "sub" => Sub,
            "and" => And,
            "or" => Or,
            "xor" => Xor,
            "shl" => Shl,
            "shr" => Shr,
            "sar" => Sar,
            "mul" => Mul,
            "div" => Div,
            "divu" => Divu,
            "rem" => Rem,
            "remu" => Remu,
            "eq" => Eq,
            "ne" => Ne,
            "lt" => Lt,
            "le" => Le,
            "ltu" => Ltu,
            "leu" => Leu,
            _ => return None,
        })
    }

    /// PRD §5.4.
    pub fn latency(self) -> u32 {
        use AluOp::*;
        match self {
            Mul => 3,
            Div | Divu | Rem | Remu => 8,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrKind {
    Always,
    IfTrue,
    IfFalse,
}

impl BrKind {
    pub fn mnemonic(self) -> &'static str {
        match self {
            BrKind::Always => "br",
            BrKind::IfTrue => "brt",
            BrKind::IfFalse => "brf",
        }
    }
}

/// A decoded operation. Belt references are 0..15, scratchpad slots 0..63.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Nop,
    Alu {
        op: AluOp,
        a: u8,
        b: u8,
    },
    Pick {
        c: u8,
        t: u8,
        f: u8,
    },
    Con {
        imm: i32,
    },
    /// Drop a None — a value that is not there (`value.rs`).
    NoneVal,
    /// Metadata predicates. These read the tag, so they never propagate it.
    IsNar {
        a: u8,
    },
    IsNone {
        a: u8,
    },
    Load {
        addr: u8,
        offset: i16,
        width: u8,
        sext: bool,
        delay: u8,
    },
    Store {
        addr: u8,
        offset: i16,
        width: u8,
        val: u8,
    },
    Spill {
        slot: u8,
        val: u8,
    },
    Fill {
        slot: u8,
    },
    Br {
        kind: BrKind,
        cond: u8,
        target: u32,
    },
    Call {
        target: u16,
        args: Vec<u8>,
    },
    Retn {
        res: Vec<u8>,
    },
    Conform(Vec<u8>),
    Rescue(u16),
    Sys(u8),
    Halt,
}

impl Op {
    /// Which slots may hold this op (PRD §5.2).
    pub fn allowed_in(&self, slot: Slot) -> bool {
        match self {
            Op::Nop => true,
            Op::Alu { .. }
            | Op::Pick { .. }
            | Op::Con { .. }
            | Op::NoneVal
            | Op::IsNar { .. }
            | Op::IsNone { .. } => {
                matches!(slot, Slot::A0 | Slot::A1)
            }
            Op::Load { .. } | Op::Store { .. } | Op::Spill { .. } | Op::Fill { .. } => {
                slot == Slot::M
            }
            _ => slot == Slot::F,
        }
    }

    /// Latency in bundles, or `None` for ops that produce no result.
    /// Latency 1 means "retires at the end of the issuing bundle" (PRD §5.4),
    /// so an op issued in bundle `t` with latency `L` retires at the end of
    /// bundle `t + L - 1`. Load delays are latencies on that same scale.
    pub fn latency(&self) -> Option<u32> {
        match self {
            Op::Alu { op, .. } => Some(op.latency()),
            Op::Pick { .. }
            | Op::Con { .. }
            | Op::NoneVal
            | Op::IsNar { .. }
            | Op::IsNone { .. } => Some(1),
            Op::Load { delay, .. } => Some(*delay as u32),
            Op::Fill { .. } => Some(3),
            // A call's results retire at the end of the calling bundle.
            Op::Call { .. } => Some(1),
            Op::Sys(code) => {
                if *code == 0 {
                    None
                } else {
                    Some(1)
                }
            }
            _ => None,
        }
    }

    /// How many results this op drops.
    pub fn n_results(&self, callee_nres: Option<usize>) -> usize {
        match self {
            Op::Alu { .. }
            | Op::Pick { .. }
            | Op::Con { .. }
            | Op::NoneVal
            | Op::IsNar { .. }
            | Op::IsNone { .. }
            | Op::Load { .. }
            | Op::Fill { .. } => 1,
            Op::Call { .. } => callee_nres.unwrap_or(0),
            Op::Sys(code) => {
                if *code == 0 {
                    0
                } else {
                    1
                }
            }
            _ => 0,
        }
    }

    /// Belt positions read by this op at bundle entry.
    pub fn belt_reads(&self) -> Vec<u8> {
        match self {
            Op::Alu { a, b, .. } => vec![*a, *b],
            Op::Pick { c, t, f } => vec![*c, *t, *f],
            Op::IsNar { a } | Op::IsNone { a } => vec![*a],
            Op::Load { addr, .. } => vec![*addr],
            Op::Store { addr, val, .. } => vec![*addr, *val],
            Op::Spill { val, .. } => vec![*val],
            Op::Br { kind, cond, .. } => {
                if *kind == BrKind::Always {
                    vec![]
                } else {
                    vec![*cond]
                }
            }
            Op::Call { args, .. } => args.clone(),
            Op::Retn { res } => res.clone(),
            // `sys` is the one op that takes fixed positions (PRD §9.5).
            Op::Sys(code) => match code {
                0 => vec![0],
                1 | 2 => vec![0, 1, 2],
                _ => vec![],
            },
            _ => vec![],
        }
    }

    pub fn is_reshape(&self) -> bool {
        matches!(self, Op::Conform(_) | Op::Rescue(_))
    }

    pub fn is_branch(&self) -> bool {
        matches!(self, Op::Br { .. })
    }

    /// True if this op ends the EBB unconditionally (no fall-through).
    pub fn ends_flow(&self) -> bool {
        match self {
            Op::Br { kind, .. } => *kind == BrKind::Always,
            Op::Retn { .. } | Op::Halt => true,
            Op::Sys(0) => true,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

pub const OP_NOP: u8 = 0x00;
pub const OP_PICK: u8 = 0x14;
pub const OP_CON: u8 = 0x15;
pub const OP_NONE: u8 = 0x16;
pub const OP_ISNAR: u8 = 0x17;
pub const OP_ISNONE: u8 = 0x18;
pub const OP_LOAD: u8 = 0x20;
pub const OP_STORE: u8 = 0x21;
pub const OP_SPILL: u8 = 0x22;
pub const OP_FILL: u8 = 0x23;
pub const OP_BR: u8 = 0x30;
pub const OP_BRT: u8 = 0x31;
pub const OP_BRF: u8 = 0x32;
pub const OP_CALL: u8 = 0x33;
pub const OP_RETN: u8 = 0x34;
pub const OP_CONFORM1: u8 = 0x35;
pub const OP_RESCUE: u8 = 0x3b;
pub const OP_SYS: u8 = 0x3c;
pub const OP_HALT: u8 = 0x3d;

#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn f(word: u32, hi: u32, width: u32) -> u32 {
    (word >> (hi + 1 - width)) & ((1u32 << width) - 1)
}

fn put(word: &mut u32, hi: u32, width: u32, v: u32) {
    let mask = (1u32 << width) - 1;
    *word |= (v & mask) << (hi + 1 - width);
}

fn sign_extend(v: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((v << shift) as i32) >> shift
}

pub fn width_code(width: u8) -> Option<u32> {
    match width {
        1 => Some(0),
        2 => Some(1),
        4 => Some(2),
        8 => Some(3),
        _ => None,
    }
}

pub fn code_width(code: u32) -> u8 {
    1u8 << code
}

pub fn encode(op: &Op) -> Result<u32, DecodeError> {
    let mut w = 0u32;
    match op {
        Op::Nop => {}
        Op::Alu { op, a, b } => {
            put(&mut w, 31, 8, op.opcode() as u32);
            put(&mut w, 23, 4, *a as u32);
            put(&mut w, 19, 4, *b as u32);
        }
        Op::Pick { c, t, f: fa } => {
            put(&mut w, 31, 8, OP_PICK as u32);
            put(&mut w, 23, 4, *c as u32);
            put(&mut w, 19, 4, *t as u32);
            put(&mut w, 15, 4, *fa as u32);
        }
        Op::Con { imm } => {
            if *imm < -(1 << 23) || *imm >= (1 << 23) {
                return Err(DecodeError(format!(
                    "con immediate {imm} does not fit in 24 bits"
                )));
            }
            put(&mut w, 31, 8, OP_CON as u32);
            put(&mut w, 23, 24, *imm as u32);
        }
        Op::NoneVal => put(&mut w, 31, 8, OP_NONE as u32),
        Op::IsNar { a } | Op::IsNone { a } => {
            let opc = if matches!(op, Op::IsNar { .. }) {
                OP_ISNAR
            } else {
                OP_ISNONE
            };
            put(&mut w, 31, 8, opc as u32);
            put(&mut w, 23, 4, *a as u32);
        }
        Op::Load {
            addr,
            offset,
            width,
            sext,
            delay,
        } => {
            let wc =
                width_code(*width).ok_or_else(|| DecodeError(format!("bad load width {width}")))?;
            if *delay < 3 || *delay > 15 {
                return Err(DecodeError(format!("load delay {delay} outside 3..15")));
            }
            check_off13(*offset)?;
            put(&mut w, 31, 8, OP_LOAD as u32);
            put(&mut w, 23, 4, *addr as u32);
            put(&mut w, 19, 4, *delay as u32);
            put(&mut w, 15, 2, wc);
            put(&mut w, 13, 1, *sext as u32);
            put(&mut w, 12, 13, *offset as u32);
        }
        Op::Store {
            addr,
            offset,
            width,
            val,
        } => {
            let wc = width_code(*width)
                .ok_or_else(|| DecodeError(format!("bad store width {width}")))?;
            check_off13(*offset)?;
            put(&mut w, 31, 8, OP_STORE as u32);
            put(&mut w, 23, 4, *addr as u32);
            put(&mut w, 19, 4, *val as u32);
            put(&mut w, 15, 2, wc);
            put(&mut w, 13, 13, *offset as u32);
        }
        Op::Spill { slot, val } => {
            put(&mut w, 31, 8, OP_SPILL as u32);
            put(&mut w, 23, 6, *slot as u32);
            put(&mut w, 17, 4, *val as u32);
        }
        Op::Fill { slot } => {
            put(&mut w, 31, 8, OP_FILL as u32);
            put(&mut w, 23, 6, *slot as u32);
        }
        Op::Br { kind, cond, target } => {
            let opc = match kind {
                BrKind::Always => OP_BR,
                BrKind::IfTrue => OP_BRT,
                BrKind::IfFalse => OP_BRF,
            };
            if *target >= (1 << 20) {
                return Err(DecodeError("branch target index out of range".into()));
            }
            put(&mut w, 31, 8, opc as u32);
            put(&mut w, 23, 4, *cond as u32);
            put(&mut w, 19, 20, *target);
        }
        Op::Call { target, args } => {
            if args.len() > MAX_ARGS {
                return Err(DecodeError("call takes at most 3 arguments".into()));
            }
            if *target >= (1 << 10) {
                return Err(DecodeError("call target index out of range".into()));
            }
            put(&mut w, 31, 8, OP_CALL as u32);
            put(&mut w, 23, 2, args.len() as u32);
            for (i, a) in args.iter().enumerate() {
                put(&mut w, 21 - 4 * i as u32, 4, *a as u32);
            }
            put(&mut w, 9, 10, *target as u32);
        }
        Op::Retn { res } => {
            if res.len() > MAX_ARGS {
                return Err(DecodeError("retn takes at most 3 results".into()));
            }
            put(&mut w, 31, 8, OP_RETN as u32);
            put(&mut w, 23, 2, res.len() as u32);
            for (i, r) in res.iter().enumerate() {
                put(&mut w, 21 - 4 * i as u32, 4, *r as u32);
            }
        }
        Op::Conform(list) => {
            if list.is_empty() || list.len() > MAX_CONFORM {
                return Err(DecodeError("conform takes 1..6 positions".into()));
            }
            put(&mut w, 31, 8, (OP_CONFORM1 + list.len() as u8 - 1) as u32);
            for (i, p) in list.iter().enumerate() {
                put(&mut w, 23 - 4 * i as u32, 4, *p as u32);
            }
        }
        Op::Rescue(mask) => {
            put(&mut w, 31, 8, OP_RESCUE as u32);
            put(&mut w, 23, 16, *mask as u32);
        }
        Op::Sys(code) => {
            put(&mut w, 31, 8, OP_SYS as u32);
            put(&mut w, 23, 8, *code as u32);
        }
        Op::Halt => {
            put(&mut w, 31, 8, OP_HALT as u32);
        }
    }
    Ok(w)
}

fn check_off13(offset: i16) -> Result<(), DecodeError> {
    if offset < -(1 << 12) || offset >= (1 << 12) {
        return Err(DecodeError(format!(
            "offset {offset} does not fit in signed 13 bits"
        )));
    }
    Ok(())
}

pub fn decode(word: u32) -> Result<Op, DecodeError> {
    let opc = (word >> 24) as u8;
    let rest = word & 0x00ff_ffff;
    let unused = |mask: u32| -> Result<(), DecodeError> {
        if word & mask != 0 {
            Err(DecodeError(format!(
                "reserved bits set in slot word {word:#010x}"
            )))
        } else {
            Ok(())
        }
    };

    if opc == OP_NOP {
        unused(0x00ff_ffff)?;
        return Ok(Op::Nop);
    }
    if let Some(op) = AluOp::from_opcode(opc) {
        unused(0x0000_ffff)?;
        return Ok(Op::Alu {
            op,
            a: f(word, 23, 4) as u8,
            b: f(word, 19, 4) as u8,
        });
    }
    Ok(match opc {
        OP_PICK => {
            unused(0x0000_0fff)?;
            Op::Pick {
                c: f(word, 23, 4) as u8,
                t: f(word, 19, 4) as u8,
                f: f(word, 15, 4) as u8,
            }
        }
        OP_CON => Op::Con {
            imm: sign_extend(rest, 24),
        },
        OP_NONE => {
            unused(0x00ff_ffff)?;
            Op::NoneVal
        }
        OP_ISNAR | OP_ISNONE => {
            unused(0x000f_ffff)?;
            let a = f(word, 23, 4) as u8;
            if opc == OP_ISNAR {
                Op::IsNar { a }
            } else {
                Op::IsNone { a }
            }
        }
        OP_LOAD => {
            let delay = f(word, 19, 4) as u8;
            if delay < 3 {
                return Err(DecodeError(format!(
                    "illegal load delay {delay} (must be 3..15)"
                )));
            }
            Op::Load {
                addr: f(word, 23, 4) as u8,
                delay,
                width: code_width(f(word, 15, 2)),
                sext: f(word, 13, 1) == 1,
                offset: sign_extend(f(word, 12, 13), 13) as i16,
            }
        }
        OP_STORE => {
            unused(0x0000_0001)?;
            Op::Store {
                addr: f(word, 23, 4) as u8,
                val: f(word, 19, 4) as u8,
                width: code_width(f(word, 15, 2)),
                offset: sign_extend(f(word, 13, 13), 13) as i16,
            }
        }
        OP_SPILL => {
            unused(0x0000_3fff)?;
            Op::Spill {
                slot: f(word, 23, 6) as u8,
                val: f(word, 17, 4) as u8,
            }
        }
        OP_FILL => {
            unused(0x0003_ffff)?;
            Op::Fill {
                slot: f(word, 23, 6) as u8,
            }
        }
        OP_BR | OP_BRT | OP_BRF => {
            let kind = match opc {
                OP_BR => BrKind::Always,
                OP_BRT => BrKind::IfTrue,
                _ => BrKind::IfFalse,
            };
            if kind == BrKind::Always && f(word, 23, 4) != 0 {
                return Err(DecodeError(
                    "unconditional br must have zero cond field".into(),
                ));
            }
            Op::Br {
                kind,
                cond: f(word, 23, 4) as u8,
                target: f(word, 19, 20),
            }
        }
        OP_CALL => {
            let n = f(word, 23, 2) as usize;
            let mut args = Vec::new();
            for i in 0..n {
                args.push(f(word, 21 - 4 * i as u32, 4) as u8);
            }
            for i in n..MAX_ARGS {
                if f(word, 21 - 4 * i as u32, 4) != 0 {
                    return Err(DecodeError("unused call argument field is nonzero".into()));
                }
            }
            Op::Call {
                target: f(word, 9, 10) as u16,
                args,
            }
        }
        OP_RETN => {
            unused(0x0000_03ff)?;
            let n = f(word, 23, 2) as usize;
            let mut res = Vec::new();
            for i in 0..n {
                res.push(f(word, 21 - 4 * i as u32, 4) as u8);
            }
            for i in n..MAX_ARGS {
                if f(word, 21 - 4 * i as u32, 4) != 0 {
                    return Err(DecodeError("unused retn result field is nonzero".into()));
                }
            }
            Op::Retn { res }
        }
        0x35..=0x3a => {
            let n = (opc - OP_CONFORM1 + 1) as usize;
            let mut list = Vec::new();
            for i in 0..n {
                list.push(f(word, 23 - 4 * i as u32, 4) as u8);
            }
            for i in n..MAX_CONFORM {
                if f(word, 23 - 4 * i as u32, 4) != 0 {
                    return Err(DecodeError("unused conform field is nonzero".into()));
                }
            }
            Op::Conform(list)
        }
        OP_RESCUE => {
            unused(0x0000_00ff)?;
            Op::Rescue(f(word, 23, 16) as u16)
        }
        OP_SYS => {
            unused(0x0000_ffff)?;
            Op::Sys(f(word, 23, 8) as u8)
        }
        OP_HALT => {
            unused(0x00ff_ffff)?;
            Op::Halt
        }
        _ => return Err(DecodeError(format!("unknown opcode {opc:#04x}"))),
    })
}

/// Render an op in assembly syntax. `ebb` and `func` resolve table indices to
/// names; the disassembler passes generated names.
pub fn format_op(op: &Op, ebb: &dyn Fn(u32) -> String, func: &dyn Fn(u16) -> String) -> String {
    let b = |p: &u8| format!("b{p}");
    match op {
        Op::Nop => "nop".into(),
        Op::Alu { op, a, b: bo } => format!("{} b{}, b{}", op.mnemonic(), a, bo),
        Op::Pick { c, t, f } => format!("pick b{c}, b{t}, b{f}"),
        Op::Con { imm } => format!("con {imm}"),
        Op::NoneVal => "none".into(),
        Op::IsNar { a } => format!("isnar b{a}"),
        Op::IsNone { a } => format!("isnone b{a}"),
        Op::Load {
            addr,
            offset,
            width,
            sext,
            delay,
        } => format!(
            "load b{addr}, {offset}, {width}, {}, {delay}",
            if *sext { "sign" } else { "zero" }
        ),
        Op::Store {
            addr,
            offset,
            width,
            val,
        } => format!("store b{addr}, {offset}, {width}, b{val}"),
        Op::Spill { slot, val } => format!("spill s{slot}, b{val}"),
        Op::Fill { slot } => format!("fill s{slot}"),
        Op::Br { kind, cond, target } => match kind {
            BrKind::Always => format!("br {}", ebb(*target)),
            _ => format!("{} b{}, {}", kind.mnemonic(), cond, ebb(*target)),
        },
        Op::Call { target, args } => {
            let mut s = format!("call {}", func(*target));
            for a in args {
                s.push_str(&format!(", {}", b(a)));
            }
            s
        }
        Op::Retn { res } => {
            let mut s = String::from("retn");
            for (i, r) in res.iter().enumerate() {
                s.push_str(if i == 0 { " " } else { ", " });
                s.push_str(&b(r));
            }
            s
        }
        Op::Conform(list) => {
            let parts: Vec<String> = list.iter().map(b).collect();
            format!("conform {}", parts.join(", "))
        }
        Op::Rescue(mask) => format!("rescue {mask:#06x}"),
        Op::Sys(code) => format!("sys {code}"),
        Op::Halt => "halt".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(op: Op) {
        let w = encode(&op).expect("encode");
        let back = decode(w).expect("decode");
        assert_eq!(op, back, "round trip failed for word {w:#010x}");
    }

    #[test]
    fn round_trip_all_forms() {
        rt(Op::Nop);
        rt(Op::Alu {
            op: AluOp::Add,
            a: 3,
            b: 15,
        });
        rt(Op::Pick { c: 1, t: 2, f: 3 });
        rt(Op::NoneVal);
        rt(Op::IsNar { a: 15 });
        rt(Op::IsNone { a: 0 });
        rt(Op::Con { imm: -1234 });
        rt(Op::Con { imm: 0x7fffff });
        rt(Op::Con { imm: -0x800000 });
        rt(Op::Load {
            addr: 2,
            offset: -4000,
            width: 8,
            sext: false,
            delay: 15,
        });
        rt(Op::Load {
            addr: 0,
            offset: 4095,
            width: 1,
            sext: true,
            delay: 3,
        });
        rt(Op::Store {
            addr: 5,
            offset: -1,
            width: 4,
            val: 6,
        });
        rt(Op::Spill { slot: 63, val: 7 });
        rt(Op::Fill { slot: 0 });
        rt(Op::Br {
            kind: BrKind::Always,
            cond: 0,
            target: 1023,
        });
        rt(Op::Br {
            kind: BrKind::IfTrue,
            cond: 9,
            target: 3,
        });
        rt(Op::Call {
            target: 7,
            args: vec![1, 2, 3],
        });
        rt(Op::Call {
            target: 0,
            args: vec![],
        });
        rt(Op::Retn { res: vec![4, 5] });
        rt(Op::Conform(vec![1]));
        rt(Op::Conform(vec![1, 2, 3, 4, 5, 6]));
        rt(Op::Rescue(0xbeef));
        rt(Op::Sys(2));
        rt(Op::Halt);
    }

    #[test]
    fn illegal_load_delay_rejected() {
        assert!(
            encode(&Op::Load {
                addr: 0,
                offset: 0,
                width: 8,
                sext: false,
                delay: 2
            })
            .is_err()
        );
        // hand-built word with delay 2
        let w = (OP_LOAD as u32) << 24 | 2 << 16 | 3 << 14;
        assert!(decode(w).is_err());
    }

    #[test]
    fn reserved_bits_checked() {
        let w = (AluOp::Add.opcode() as u32) << 24 | 1;
        assert!(decode(w).is_err());
    }
}
