//! millet-asm — the Millet assembler, static belt checker and disassembler.

pub mod asm;
pub mod check;
pub mod disasm;

pub use asm::{Assembled, assemble};
pub use check::{Diag, Prediction};
pub use disasm::disassemble;

use millet_core::{Config, Image, image::MAGIC};

/// Load an image, assembling it first if the file is source rather than an
/// assembled image. The magic decides, so the extension is free to be anything.
/// Errors come back rendered, ready to print.
// ponytail: assembler warnings are dropped here — `mas` is where you go for those.
pub fn load(path: &str) -> Result<Image, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if bytes.starts_with(MAGIC) {
        return Image::from_bytes(&bytes).map_err(|e| format!("{path}: {e}"));
    }
    let src = String::from_utf8(bytes)
        .map_err(|_| format!("{path}: neither a Millet image nor UTF-8 source"))?;
    assemble(&src, &Config::default())
        .map(|a| a.image)
        .map_err(|e| {
            let mut diags = e.diags;
            diags.sort_by_key(|d| d.line);
            diags
                .iter()
                .map(|d| d.render(path))
                .collect::<Vec<_>>()
                .join("\n")
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn source_and_image_load_the_same_way() {
        let src = super::load("../examples/hello.mil").expect("source assembles");
        let p = std::env::temp_dir().join("millet-load-test.mimg");
        std::fs::write(&p, src.to_bytes()).unwrap();
        assert_eq!(super::load(p.to_str().unwrap()).unwrap(), src);
        assert!(super::load("../examples/nope.mil").is_err());
    }
}
