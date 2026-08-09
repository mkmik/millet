//! Image to text. Output re-assembles to a byte-identical image.

use millet_core::isa::{decode, format_op, Op, Slot};
use millet_core::Image;

pub fn ebb_name(img: &Image, i: usize) -> String {
    match img.funcs.iter().position(|f| f.ebb as usize == i) {
        Some(f) => func_name(img, f),
        None => img.ebb_label(i),
    }
}

pub fn func_name(img: &Image, i: usize) -> String {
    img.func_label(i)
}

pub fn disassemble(img: &Image) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("; disassembled by mas -d\n");
    out.push_str(&format!(
        ".entry {}\n\n",
        func_name(img, img.entry as usize)
    ));

    for seg in &img.data {
        out.push_str(&format!(".data {:#x}\n", seg.addr));
        for chunk in seg.bytes.chunks(16) {
            let vals: Vec<String> = chunk.iter().map(|b| format!("{b:#04x}")).collect();
            out.push_str(&format!(".u8 {}\n", vals.join(", ")));
        }
        out.push('\n');
    }

    let name_ebb = |i: u32| ebb_name(img, i as usize);
    let name_fn = |i: u16| func_name(img, i as usize);

    for (b, words) in img.bundles.iter().enumerate() {
        if let Some(ei) = img.ebb_at(b as u32) {
            let e = &img.ebbs[ei];
            match img.funcs.iter().position(|f| f.ebb as usize == ei) {
                Some(fi) => out.push_str(&format!(
                    ".func {}({}) -> {}\n",
                    func_name(img, fi),
                    e.arity,
                    img.funcs[fi].nres
                )),
                None => out.push_str(&format!(".ebb {}({})\n", ebb_name(img, ei), e.arity)),
            }
        }
        let mut any = false;
        for slot in Slot::ALL {
            let op = decode(words[slot.index()]).map_err(|e| format!("bundle {b}: {e}"))?;
            if op == Op::Nop {
                continue;
            }
            out.push_str(&format!(
                "    {:<3} {}\n",
                slot.tag(),
                format_op(&op, &name_ebb, &name_fn)
            ));
            any = true;
        }
        if !any {
            // Keep empty bundles addressable: an explicit nop encodes to zero.
            out.push_str("    a0  nop\n");
        }
        out.push('\n');
    }
    Ok(out)
}
