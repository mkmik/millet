//! The Millet binary image: a small header plus code, EBB table, function
//! table and `.data` blobs (PRD §10). Everything is little-endian.

use std::fmt;

pub const MAGIC: &[u8; 4] = b"MILT";
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EbbEntry {
    pub bundle: u32,
    pub arity: u8,
    /// Source label, kept in the image purely so traces and disassembly can
    /// say `loop` instead of `ebb3`. Empty when the image was built by hand.
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuncEntry {
    pub ebb: u32,
    pub arity: u8,
    pub nres: u8,
    pub name: String,
}

impl EbbEntry {
    pub fn new(bundle: u32, arity: u8) -> EbbEntry {
        EbbEntry {
            bundle,
            arity,
            name: String::new(),
        }
    }
}

impl FuncEntry {
    pub fn new(ebb: u32, arity: u8, nres: u8) -> FuncEntry {
        FuncEntry {
            ebb,
            arity,
            nres,
            name: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataSeg {
    pub addr: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Image {
    pub bundles: Vec<[u32; 4]>,
    pub ebbs: Vec<EbbEntry>,
    pub funcs: Vec<FuncEntry>,
    pub data: Vec<DataSeg>,
    /// Index into `funcs` of the entry point.
    pub entry: u32,
}

#[derive(Debug)]
pub struct ImageError(pub String);

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ImageError> {
        if self.pos + n > self.buf.len() {
            return Err(ImageError("truncated image".into()));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, ImageError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ImageError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, ImageError> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| ImageError("label is not valid UTF-8".into()))
    }
}

impl Image {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.bundles.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.ebbs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.funcs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.entry.to_le_bytes());
        for bundle in &self.bundles {
            for word in bundle {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
        for e in &self.ebbs {
            out.extend_from_slice(&e.bundle.to_le_bytes());
            out.extend_from_slice(&(e.arity as u32).to_le_bytes());
            put_str(&mut out, &e.name);
        }
        for f in &self.funcs {
            out.extend_from_slice(&f.ebb.to_le_bytes());
            out.extend_from_slice(&(f.arity as u32).to_le_bytes());
            out.extend_from_slice(&(f.nres as u32).to_le_bytes());
            put_str(&mut out, &f.name);
        }
        for d in &self.data {
            out.extend_from_slice(&d.addr.to_le_bytes());
            out.extend_from_slice(&(d.bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&d.bytes);
        }
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Image, ImageError> {
        let mut r = Reader { buf, pos: 0 };
        if r.take(4)? != MAGIC {
            return Err(ImageError("not a Millet image (bad magic)".into()));
        }
        let version = r.u32()?;
        if version != VERSION {
            return Err(ImageError(format!("unsupported image version {version}")));
        }
        let n_bundles = r.u32()? as usize;
        let n_ebbs = r.u32()? as usize;
        let n_funcs = r.u32()? as usize;
        let n_data = r.u32()? as usize;
        let entry = r.u32()?;

        let mut bundles = Vec::with_capacity(n_bundles);
        for _ in 0..n_bundles {
            bundles.push([r.u32()?, r.u32()?, r.u32()?, r.u32()?]);
        }
        let mut ebbs = Vec::with_capacity(n_ebbs);
        for _ in 0..n_ebbs {
            ebbs.push(EbbEntry {
                bundle: r.u32()?,
                arity: r.u32()? as u8,
                name: r.string()?,
            });
        }
        let mut funcs = Vec::with_capacity(n_funcs);
        for _ in 0..n_funcs {
            funcs.push(FuncEntry {
                ebb: r.u32()?,
                arity: r.u32()? as u8,
                nres: r.u32()? as u8,
                name: r.string()?,
            });
        }
        let mut data = Vec::with_capacity(n_data);
        for _ in 0..n_data {
            let addr = r.u64()?;
            let len = r.u32()? as usize;
            data.push(DataSeg {
                addr,
                bytes: r.take(len)?.to_vec(),
            });
        }
        if entry as usize >= funcs.len() {
            return Err(ImageError("entry function index out of range".into()));
        }
        Ok(Image {
            bundles,
            ebbs,
            funcs,
            data,
            entry,
        })
    }

    /// Display name for an EBB, falling back to a generated one.
    pub fn ebb_label(&self, i: usize) -> String {
        match self.ebbs.get(i) {
            Some(e) if !e.name.is_empty() => e.name.clone(),
            _ => format!("ebb{i}"),
        }
    }

    /// Display name for a function, falling back to a generated one.
    pub fn func_label(&self, i: usize) -> String {
        match self.funcs.get(i) {
            Some(f) if !f.name.is_empty() => f.name.clone(),
            _ => format!("fn{i}"),
        }
    }

    /// EBB index whose entry is `bundle`, if any.
    pub fn ebb_at(&self, bundle: u32) -> Option<usize> {
        self.ebbs.iter().position(|e| e.bundle == bundle)
    }

    /// EBB index containing `bundle` — the last one starting at or before it.
    pub fn ebb_containing(&self, bundle: u32) -> Option<usize> {
        self.ebbs
            .iter()
            .enumerate()
            .filter(|(_, e)| e.bundle <= bundle)
            .max_by_key(|(_, e)| e.bundle)
            .map(|(i, _)| i)
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_round_trip() {
        let img = Image {
            bundles: vec![[1, 2, 3, 4], [0, 0, 0, 0]],
            ebbs: vec![EbbEntry {
                bundle: 0,
                arity: 2,
                name: "main".into(),
            }],
            funcs: vec![FuncEntry {
                ebb: 0,
                arity: 2,
                nres: 1,
                name: "main".into(),
            }],
            data: vec![DataSeg {
                addr: 0x1000,
                bytes: vec![1, 2, 3],
            }],
            entry: 0,
        };
        let bytes = img.to_bytes();
        assert_eq!(Image::from_bytes(&bytes).unwrap(), img);
    }
}
