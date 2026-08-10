//! millet-asm — the Millet assembler, static belt checker and disassembler.

pub mod asm;
pub mod check;
pub mod disasm;

pub use asm::{Assembled, assemble};
pub use check::{Diag, Prediction};
pub use disasm::disassemble;
