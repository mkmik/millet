//! Operand metadata: the tag every belt value carries alongside its bits.
//!
//! On the Mill an operand is data *plus* metadata, and the metadata is what
//! makes speculation safe. An operation that fails drops a **NaR** ("not a
//! result") carrying the failure information instead of trapping; the fault is
//! raised only if something non-speculable — a store, a branch, an IO —
//! actually consumes it. A **None** is a value that is not there at all: it
//! propagates like a NaR but *suppresses* a store rather than faulting, which
//! is what lets a store be hoisted above the condition that guards it.
//!
//! None wins over NaR ("a NaR and a None is a None"): None means the operation
//! was never really meant to happen, so it must not report a fault.
//!
//! Millet carries these two status tags and not the Mill's width and
//! scalarity metadata: no Millet operation is width-polymorphic — widths live
//! in the `load`/`store` encodings — so those bits would be carried and never
//! read.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Tag {
    #[default]
    Val,
    None,
    Nar,
}

/// A belt (or scratchpad) value: 64 bits of data and its tag. A NaR's bits are
/// its payload — what went wrong and where — exactly as on the Mill, where the
/// failure information is hashed into the data field for the debugger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value {
    pub bits: u64,
    pub tag: Tag,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NarKind {
    /// A load from an address no `.data` section covers and nothing wrote to.
    Unbacked,
    DivZero,
    /// Signed `INT_MIN / -1`.
    Overflow,
    Unknown,
}

impl NarKind {
    fn code(self) -> u64 {
        match self {
            NarKind::Unbacked => 1,
            NarKind::DivZero => 2,
            NarKind::Overflow => 3,
            NarKind::Unknown => 0,
        }
    }

    fn from_code(c: u64) -> NarKind {
        match c {
            1 => NarKind::Unbacked,
            2 => NarKind::DivZero,
            3 => NarKind::Overflow,
            _ => NarKind::Unknown,
        }
    }

    pub fn why(self) -> &'static str {
        match self {
            NarKind::Unbacked => "load from an unbacked address",
            NarKind::DivZero => "division by zero",
            NarKind::Overflow => "signed division overflow",
            NarKind::Unknown => "unknown failure",
        }
    }
}

impl Value {
    pub const fn val(bits: u64) -> Value {
        Value {
            bits,
            tag: Tag::Val,
        }
    }

    pub const NONE: Value = Value {
        bits: 0,
        tag: Tag::None,
    };

    /// A NaR whose payload records what failed and the bundle it failed in.
    pub fn nar(kind: NarKind, bundle: usize) -> Value {
        Value {
            bits: kind.code() << 32 | (bundle as u64 & 0xffff_ffff),
            tag: Tag::Nar,
        }
    }

    pub fn is_poison(self) -> bool {
        self.tag != Tag::Val
    }

    /// The poison that dominates these operands, if any: a None if one is
    /// present, otherwise the first NaR — payload and all, so the origin
    /// survives however far the value propagates.
    pub fn poison(args: &[Value]) -> Option<Value> {
        if args.iter().any(|v| v.tag == Tag::None) {
            return Some(Value::NONE);
        }
        args.iter().find(|v| v.tag == Tag::Nar).copied()
    }

    /// The long form, for a fault diagnostic.
    pub fn describe(self) -> String {
        match self.tag {
            Tag::Val => format!("{}", self.bits as i64),
            Tag::None => "a None".into(),
            Tag::Nar => format!(
                "a NaR from bundle {} ({})",
                self.bits & 0xffff_ffff,
                NarKind::from_code(self.bits >> 32).why()
            ),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tag {
            Tag::Val => write!(f, "{}", self.bits as i64),
            Tag::None => write!(f, "None"),
            Tag::Nar => write!(f, "NaR"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_wins_over_nar() {
        let nar = Value::nar(NarKind::DivZero, 7);
        assert_eq!(Value::poison(&[nar, Value::NONE]), Some(Value::NONE));
        assert_eq!(Value::poison(&[Value::NONE, nar]), Some(Value::NONE));
        assert_eq!(Value::poison(&[Value::val(1), nar]), Some(nar));
        assert_eq!(Value::poison(&[Value::val(1), Value::val(2)]), None);
    }

    #[test]
    fn a_nar_carries_where_it_came_from() {
        let v = Value::nar(NarKind::Unbacked, 12);
        assert_eq!(
            v.describe(),
            "a NaR from bundle 12 (load from an unbacked address)"
        );
    }
}
