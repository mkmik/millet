//! millet-core — ISA definitions, instruction encoding, and the binary image
//! format shared by the assembler and the simulator.

pub mod image;
pub mod isa;

pub use image::{DataSeg, EbbEntry, FuncEntry, Image};
pub use isa::{AluOp, BrKind, Op, Slot, MAX_ARGS, MAX_CONFORM};

/// Machine parameters (PRD §2). Runtime, not const-generic, so the
/// FPGA-oriented belt-8 variant is a parameter change rather than a fork.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub belt: usize,
    pub slots: usize,
    pub scratch: usize,
    pub bundle_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            belt: 16,
            slots: 4,
            scratch: 64,
            bundle_bytes: 16,
        }
    }
}

/// Hard upper bound on the belt: belt references are 4 bits wide.
pub const BELT_MAX: usize = 16;
/// Hard upper bound on the scratchpad: the slot field is 6 bits wide.
pub const SCRATCH_MAX: usize = 64;
