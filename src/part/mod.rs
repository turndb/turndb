//! A **part**: an immutable, self-contained, id-sorted columnar slice of the store.
//!
//! A part holds record identity, the flat body programs that reconstruct content out of the fold, the
//! piece dictionary those programs reference, and the typed attribute columns. It holds no content —
//! content lives in the fold and is shared by every part. That is what makes merging parts cheap:
//! a merge rewrites references and columns, never bytes.
//!
//! ```text
//!   [ sections … ]  [ TOC ]  [ FOOTER (48B, at EOF) ]
//! ```
//! The footer is written last and is the completeness marker: a part whose footer is absent or fails
//! its checksum was torn mid-write and is discarded, never half-read.
//!
//! # Invariants
//! - Ids are strictly increasing. Version resolution across parts is by sequence range, so a part
//!   never holds two versions of one id.
//! - The piece dictionary is sorted by `(seg, block_off, in_off)` — fold order, not hash order. That
//!   makes a merge's dictionary union a linear merge instead of a sort, and makes a scan read the
//!   fold sequentially.

pub mod attrs;
pub mod idcol;

use crate::fold::{Fold, Loc};
use crate::types::{AttrValue, BodyOp, PieceHash, Record};
use anyhow::{bail, Context, Result};
use idcol::{get_varint, put_varint, IdCol};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

pub const MAGIC: &[u8; 8] = b"TURNPART";
pub const FOOTER_LEN: u64 = 56;

/// Body-program op tags, packed into the low bit of a varint.
const OP_LIT: u64 = 0;
const OP_PIECE: u64 = 1;

/// One section's location and encoding.
#[derive(Clone, Debug)]
struct Section {
    off: u64,
    stored: u32,
    raw: u32,
    codec: u8,
}

/// What a part says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartMeta {
    pub n_records: u32,
    /// Inclusive sequence range. Version resolution across parts compares `seq_hi`; a merge's output
    /// takes `min(seq_lo)`/`max(seq_hi)` of its inputs so it sorts correctly against parts it did not
    /// include.
    pub seq_lo: u64,
    pub seq_hi: u64,
}

// ---------------------------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------------------------

/// Write a part from records that are already carved, in any order.
///
/// `resolve` maps a piece's content identity to its location in the fold — the caller owns that
/// mapping because the fold's dedup window and the parts' own dictionaries are both valid sources.
pub fn build(
    path: &Path,
    records: &[Record],
    seq_lo: u64,
    seq_hi: u64,
    level: i32,
    mut resolve: impl FnMut(&PieceHash) -> Option<Loc>,
) -> Result<PartMeta> {
    // ---- order + uniqueness ----
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| records[a].id.cmp(&records[b].id));
    for w in order.windows(2) {
        if records[w[0]].id == records[w[1]].id {
            bail!("a part cannot hold two versions of id {:?}", records[w[0]].id);
        }
    }
    let ids: Vec<String> = order.iter().map(|&i| records[i].id.clone()).collect();

    // ---- piece dictionary: distinct locs, sorted in FOLD order ----
    let mut piece_of: HashMap<PieceHash, Loc> = HashMap::new();
    for r in records {
        for op in &r.body {
            if let BodyOp::Piece { hash, .. } = op {
                if !piece_of.contains_key(hash) {
                    let loc = resolve(hash)
                        .ok_or_else(|| anyhow::anyhow!("piece {hash} is referenced but not in the fold"))?;
                    piece_of.insert(*hash, loc);
                }
            }
        }
    }
    let mut dict: Vec<(Loc, PieceHash)> = piece_of.iter().map(|(h, l)| (*l, *h)).collect();
    dict.sort_by_key(|(l, _)| (l.seg, l.block_off, l.in_off));
    let dict_index: HashMap<PieceHash, u32> =
        dict.iter().enumerate().map(|(i, (_, h))| (*h, i as u32)).collect();

    // ---- body programs ----
    let mut prog = Vec::new();
    let mut prog_off: Vec<u64> = Vec::with_capacity(ids.len() + 1);
    for &ri in &order {
        prog_off.push(prog.len() as u64);
        let r = &records[ri];
        put_varint(&mut prog, r.body.len() as u64);
        for op in &r.body {
            match op {
                BodyOp::Lit(b) => {
                    put_varint(&mut prog, ((b.len() as u64) << 1) | OP_LIT);
                    prog.extend_from_slice(b);
                }
                BodyOp::Piece { hash, len } => {
                    let idx = dict_index[hash];
                    put_varint(&mut prog, ((idx as u64) << 1) | OP_PIECE);
                    put_varint(&mut prog, *len as u64);
                }
            }
        }
    }
    prog_off.push(prog.len() as u64);

    // ---- attribute columns ----
    let ordered: Vec<&Record> = order.iter().map(|&i| &records[i]).collect();
    let built = attrs::build(&ordered)?;

    // ---- id column ----
    let (id_stream, id_restarts) = idcol::build(&ids)?;

    // ---- lay the sections down, in a fixed order (determinism) ----
    let mut w = Writer::new(path, level)?;
    w.section("ids", &id_stream)?;
    w.section("ids.restart", &u32s(&id_restarts))?;
    w.section("prog", &prog)?;
    w.section("prog.off", &u64s(&prog_off))?;
    w.section("pdict.loc", &dict.iter().flat_map(|(l, _)| l.encode()).collect::<Vec<u8>>())?;
    w.section("pdict.hash", &dict.iter().flat_map(|(_, h)| h.0).collect::<Vec<u8>>())?;
    w.section("layout", &built.layout)?;
    w.section("layout.off", &u64s(&built.layout_off))?;
    w.section("colmeta", &built.meta)?;
    for (i, c) in built.cols.iter().enumerate() {
        w.section(&format!("col.val.{i}"), &c.val)?;
        if !c.rid.is_empty() {
            w.section(&format!("col.rid.{i}"), &c.rid)?;
        }
        if !c.dict.is_empty() {
            w.section(&format!("col.dict.{i}"), &c.dict)?;
        }
    }
    let meta = PartMeta { n_records: ids.len() as u32, seq_lo, seq_hi };
    w.finish(meta)?;
    Ok(meta)
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn u64s(v: &[u64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

struct Writer {
    f: File,
    off: u64,
    toc: Vec<(String, Section)>,
    level: i32,
}

impl Writer {
    fn new(path: &Path, level: i32) -> Result<Writer> {
        let f = File::create(path).with_context(|| format!("create part {}", path.display()))?;
        Ok(Writer { f, off: 0, toc: Vec::new(), level })
    }

    fn section(&mut self, name: &str, raw: &[u8]) -> Result<()> {
        let (codec, payload) = crate::fold::codec::encode(raw, None, self.level)?;
        self.f.write_all(&payload)?;
        self.toc.push((
            name.to_string(),
            Section { off: self.off, stored: payload.len() as u32, raw: raw.len() as u32, codec },
        ));
        self.off += payload.len() as u64;
        Ok(())
    }

    fn finish(mut self, meta: PartMeta) -> Result<()> {
        let mut toc = Vec::new();
        put_varint(&mut toc, self.toc.len() as u64);
        for (name, s) in &self.toc {
            put_varint(&mut toc, name.len() as u64);
            toc.extend_from_slice(name.as_bytes());
            put_varint(&mut toc, s.off);
            put_varint(&mut toc, s.stored as u64);
            put_varint(&mut toc, s.raw as u64);
            toc.push(s.codec);
        }
        let (toc_codec, toc_payload) = crate::fold::codec::encode(&toc, None, self.level)?;
        let toc_off = self.off;
        self.f.write_all(&toc_payload)?;

        let mut foot = Vec::with_capacity(FOOTER_LEN as usize);
        foot.extend_from_slice(MAGIC);
        foot.extend_from_slice(&toc_off.to_le_bytes());
        foot.extend_from_slice(&(toc_payload.len() as u32).to_le_bytes());
        foot.extend_from_slice(&(toc.len() as u32).to_le_bytes());
        foot.extend_from_slice(&meta.n_records.to_le_bytes());
        foot.extend_from_slice(&meta.seq_lo.to_le_bytes());
        foot.extend_from_slice(&meta.seq_hi.to_le_bytes());
        foot.push(toc_codec);
        while foot.len() < FOOTER_LEN as usize - 4 {
            foot.push(0);
        }
        let x = blake3::hash(&foot);
        foot.extend_from_slice(&x.as_bytes()[0..4]);
        debug_assert_eq!(foot.len(), FOOTER_LEN as usize);
        // The footer lands LAST and is the completeness marker.
        self.f.write_all(&foot)?;
        self.f.sync_all()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

pub struct Part {
    f: File,
    toc: HashMap<String, Section>,
    meta: PartMeta,
    cache: Mutex<HashMap<String, std::sync::Arc<Vec<u8>>>>,
    /// Decoded row-index arrays, per column. Without this every row read re-decoded a whole column.
    rid_cache: Mutex<HashMap<usize, std::sync::Arc<Vec<u32>>>>,
}

impl Part {
    pub fn open(path: &Path) -> Result<Part> {
        let f = File::open(path).with_context(|| format!("open part {}", path.display()))?;
        let len = f.metadata()?.len();
        if len < FOOTER_LEN {
            bail!("part {} is too short to hold a footer", path.display());
        }
        let mut foot = [0u8; FOOTER_LEN as usize];
        f.read_exact_at(&mut foot, len - FOOTER_LEN)?;
        if &foot[0..8] != MAGIC {
            bail!("not a turndb part (bad magic) — or the footer never landed");
        }
        let want = blake3::hash(&foot[..FOOTER_LEN as usize - 4]);
        if want.as_bytes()[0..4] != foot[FOOTER_LEN as usize - 4..] {
            bail!("part footer checksum mismatch — torn write");
        }
        let toc_off = u64::from_le_bytes(foot[8..16].try_into().unwrap());
        let toc_stored = u32::from_le_bytes(foot[16..20].try_into().unwrap());
        let toc_raw = u32::from_le_bytes(foot[20..24].try_into().unwrap());
        let n_records = u32::from_le_bytes(foot[24..28].try_into().unwrap());
        let seq_lo = u64::from_le_bytes(foot[28..36].try_into().unwrap());
        let seq_hi = u64::from_le_bytes(foot[36..44].try_into().unwrap());
        let toc_codec = foot[44];

        let mut tbuf = vec![0u8; toc_stored as usize];
        f.read_exact_at(&mut tbuf, toc_off)?;
        let toc_bytes = crate::fold::codec::decode(toc_codec, &tbuf, toc_raw, None)?;

        let mut at = 0usize;
        let n = get_varint(&toc_bytes, &mut at)? as usize;
        let mut toc = HashMap::with_capacity(n);
        for _ in 0..n {
            let nl = get_varint(&toc_bytes, &mut at)? as usize;
            let name = String::from_utf8(toc_bytes[at..at + nl].to_vec())?;
            at += nl;
            let off = get_varint(&toc_bytes, &mut at)?;
            let stored = get_varint(&toc_bytes, &mut at)? as u32;
            let raw = get_varint(&toc_bytes, &mut at)? as u32;
            let codec = toc_bytes[at];
            at += 1;
            toc.insert(name, Section { off, stored, raw, codec });
        }
        Ok(Part {
            f,
            toc,
            meta: PartMeta { n_records, seq_lo, seq_hi },
            cache: Mutex::new(HashMap::new()),
            rid_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn meta(&self) -> PartMeta {
        self.meta
    }

    pub fn len(&self) -> usize {
        self.meta.n_records as usize
    }

    pub fn is_empty(&self) -> bool {
        self.meta.n_records == 0
    }

    /// A section's decompressed bytes, cached after first touch.
    fn sect(&self, name: &str) -> Result<std::sync::Arc<Vec<u8>>> {
        if let Some(v) = self.cache.lock().unwrap().get(name) {
            return Ok(v.clone());
        }
        let s = self
            .toc
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("part has no section {name}"))?
            .clone();
        let mut buf = vec![0u8; s.stored as usize];
        self.f.read_exact_at(&mut buf, s.off)?;
        let raw = crate::fold::codec::decode(s.codec, &buf, s.raw, None)?;
        let arc = std::sync::Arc::new(raw);
        self.cache.lock().unwrap().insert(name.to_string(), arc.clone());
        Ok(arc)
    }

    fn has(&self, name: &str) -> bool {
        self.toc.contains_key(name)
    }

    /// All ids, in order.
    pub fn ids(&self) -> Result<Vec<String>> {
        let stream = self.sect("ids")?;
        let r = self.sect("ids.restart")?;
        let restarts = as_u32s(&r);
        let c = IdCol::new(&stream, &restarts, self.len());
        c.iter()?.into_iter().map(|b| Ok(String::from_utf8(b)?)).collect()
    }

    /// Row index of `id`, or `None`.
    pub fn find(&self, id: &str) -> Result<Option<usize>> {
        let stream = self.sect("ids")?;
        let r = self.sect("ids.restart")?;
        let restarts = as_u32s(&r);
        IdCol::new(&stream, &restarts, self.len()).find(id.as_bytes())
    }

    /// The piece dictionary entry at `i`.
    pub fn piece(&self, i: usize) -> Result<(Loc, PieceHash)> {
        let l = self.sect("pdict.loc")?;
        let h = self.sect("pdict.hash")?;
        if (i + 1) * 16 > l.len() || (i + 1) * 32 > h.len() {
            bail!("piece dictionary index {i} out of range");
        }
        let loc = Loc::decode(&l[i * 16..])?;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&h[i * 32..i * 32 + 32]);
        Ok((loc, PieceHash(hash)))
    }

    pub fn piece_count(&self) -> Result<usize> {
        Ok(self.sect("pdict.loc")?.len() / 16)
    }

    /// The body program of row `r`, with piece references resolved to content identity.
    pub fn body(&self, r: usize) -> Result<Vec<BodyOp>> {
        let prog = self.sect("prog")?;
        let offs = self.sect("prog.off")?;
        let offs = as_u64s(&offs);
        if r + 1 >= offs.len() {
            bail!("row {r} out of range");
        }
        let (mut at, end) = (offs[r] as usize, offs[r + 1] as usize);
        let n = get_varint(&prog, &mut at)? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let tagged = get_varint(&prog, &mut at)?;
            if tagged & 1 == OP_LIT {
                let len = (tagged >> 1) as usize;
                if at + len > end {
                    bail!("literal runs past the program");
                }
                out.push(BodyOp::Lit(prog[at..at + len].to_vec()));
                at += len;
            } else {
                let idx = (tagged >> 1) as usize;
                let len = get_varint(&prog, &mut at)? as u32;
                let (_, hash) = self.piece(idx)?;
                out.push(BodyOp::Piece { hash, len });
            }
        }
        Ok(out)
    }

    /// Row `r`'s attributes, in their exact original order, duplicates included.
    pub fn attrs(&self, r: usize) -> Result<Vec<(String, AttrValue)>> {
        attrs::read_row(self, r)
    }

    /// The whole record at row `r`.
    pub fn record(&self, r: usize) -> Result<Record> {
        let ids = self.sect("ids")?;
        let rs = self.sect("ids.restart")?;
        let restarts = as_u32s(&rs);
        let id = String::from_utf8(IdCol::new(&ids, &restarts, self.len()).get(r)?)?;
        Ok(Record { id, body: self.body(r)?, attrs: self.attrs(r)? })
    }

    /// Reconstruct row `r`'s content byte-exactly, resolving pieces through `fold`.
    ///
    /// Piece references go through the part's own dictionary, so the fold is addressed by location and
    /// never searched. The dictionary is in fold order, so a scan walks the fold forward.
    pub fn reconstruct(&self, r: usize, fold: &Fold) -> Result<Vec<u8>> {
        let prog = self.sect("prog")?;
        let offs = self.sect("prog.off")?;
        let offs = as_u64s(&offs);
        if r + 1 >= offs.len() {
            bail!("row {r} out of range");
        }
        let (mut at, end) = (offs[r] as usize, offs[r + 1] as usize);
        let n = get_varint(&prog, &mut at)? as usize;
        let mut out = Vec::new();
        for _ in 0..n {
            let tagged = get_varint(&prog, &mut at)?;
            if tagged & 1 == OP_LIT {
                let len = (tagged >> 1) as usize;
                if at + len > end {
                    bail!("literal runs past the program");
                }
                out.extend_from_slice(&prog[at..at + len]);
                at += len;
            } else {
                let idx = (tagged >> 1) as usize;
                let len = get_varint(&prog, &mut at)? as u32;
                let (loc, hash) = self.piece(idx)?;
                let bytes = fold.read_verified(loc, hash)?;
                if bytes.len() as u32 != len {
                    bail!("piece {hash} is {} bytes but the program says {len}", bytes.len());
                }
                out.extend_from_slice(&bytes);
            }
        }
        Ok(out)
    }

    /// Reconstruct by id.
    pub fn reconstruct_id(&self, id: &str, fold: &Fold) -> Result<Option<Vec<u8>>> {
        match self.find(id)? {
            Some(r) => Ok(Some(self.reconstruct(r, fold)?)),
            None => Ok(None),
        }
    }

    /// Every section: `(name, stored, raw, codec)` — the on-disk anatomy of this part.
    pub fn sections(&self) -> Vec<(String, u32, u32, u8)> {
        let mut v: Vec<(String, u32, u32, u8)> =
            self.toc.iter().map(|(n, s)| (n.clone(), s.stored, s.raw, s.codec)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }

    pub(crate) fn rid_cached(&self, c: usize) -> Option<std::sync::Arc<Vec<u32>>> {
        self.rid_cache.lock().unwrap().get(&c).cloned()
    }
    pub(crate) fn rid_cache_put(&self, c: usize, v: Vec<u32>) -> std::sync::Arc<Vec<u32>> {
        let a = std::sync::Arc::new(v);
        self.rid_cache.lock().unwrap().insert(c, a.clone());
        a
    }

    pub(crate) fn section_bytes(&self, name: &str) -> Result<std::sync::Arc<Vec<u8>>> {
        self.sect(name)
    }
    pub(crate) fn section_present(&self, name: &str) -> bool {
        self.has(name)
    }
}

pub(crate) fn as_u32s(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}
pub(crate) fn as_u64s(b: &[u8]) -> Vec<u64> {
    b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}
