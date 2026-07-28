//! The streaming part builder: rows in one at a time, sections spooled to disk, memory bounded by
//! the piece dictionary and the column universe — never by the record count.
//!
//! [`super::build_full`] holds every record and every section in memory at once, which is right
//! for a memtable flush (bounded by the flush interval) and wrong for a merge (bounded by nothing
//! but the store). This builder takes what a merge can know CHEAPLY up front — the piece
//! dictionary union from the input dictionaries, the column universe and string dictionaries from
//! a metadata pre-pass — and then streams rows through, appending each unbounded section to its
//! own spool file. `finish` assembles the final part by loading one spool at a time.
//!
//! **The output is byte-identical to `build_full` given the same rows** — asserted by test, not
//! assumed — which is what makes the old builder the streaming builder's oracle.

use super::{PartMeta, Writer, OP_LIT, OP_PIECE};
use crate::fold::Loc;
use crate::part::attrs::{encode_zones, ZoneAcc, RID_DELTA, RID_DENSE};
use crate::part::bloom;
use crate::part::idcol::{put_varint, RESTART};
use crate::types::{AttrValue, BodyOp, PieceHash};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// An append-only overflow file for one unbounded section. Named `<part>.s<N>.tmp`, so a crash
/// leaves only `*.tmp` litter for the store's sweep; deleted on `finish` and best-effort on drop.
struct Spool {
    path: PathBuf,
    w: std::io::BufWriter<std::fs::File>,
    len: u64,
}

impl Spool {
    fn new(base: &Path, n: usize) -> Result<Spool> {
        let path = base.with_extension(format!("s{n}.tmp"));
        let f = std::fs::File::create(&path)
            .with_context(|| format!("create spool {}", path.display()))?;
        Ok(Spool { path, w: std::io::BufWriter::new(f), len: 0 })
    }
    fn append(&mut self, b: &[u8]) -> Result<()> {
        self.w.write_all(b)?;
        self.len += b.len() as u64;
        Ok(())
    }
    /// Everything appended so far, loaded whole — `finish`'s per-section working set.
    fn take(mut self) -> Result<Vec<u8>> {
        self.w.flush()?;
        let b = std::fs::read(&self.path)?;
        let _ = std::fs::remove_file(&self.path);
        Ok(b)
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One column's streaming state. Ordinals were fixed before the first row, so values append as
/// they arrive; whether `rid` was dense — and therefore elided — is only known at the end, so
/// deltas spool unconditionally and a dense column discards its spool.
struct Col {
    key: String,
    tag: u8,
    /// Sorted distinct values, for string columns; the ordinal space `val` writes into.
    dict: Vec<String>,
    occurrences: u64,
    dense: bool,
    prev_rid: u32,
    zone: ZoneAcc,
    val: Spool,
    rid: Spool,
}

pub struct StreamBuilder {
    w: Writer,
    dict: Vec<(Loc, PieceHash)>,
    dict_index: HashMap<PieceHash, u32>,
    cols: Vec<Col>,
    col_of: HashMap<(String, u8), usize>,

    ids: Spool,
    id_restarts: Vec<u32>,
    id_stream_len: u64,
    prev_id: Vec<u8>,

    prog: Spool,
    prog_off: Spool,
    prog_len: u64,
    layout: Spool,
    layout_off: Spool,
    layout_len: u64,

    tomb: Vec<u8>,
    tomb_n: u64,
    tomb_prev: u64,

    rows: u64,
}

impl StreamBuilder {
    /// `dict` is the full piece dictionary the part will carry (referenced plus retained), in ANY
    /// order — it is sorted to fold order here, exactly as `build_full` sorts it. `columns` is the
    /// exact `(key, tag)` universe of the rows that will be pushed, with each string column's
    /// sorted-distinct dictionary in `string_dicts` (empty vecs for non-string columns).
    pub fn new(
        path: &Path,
        level: i32,
        mut dict: Vec<(Loc, PieceHash)>,
        columns: Vec<(String, u8)>,
        string_dicts: Vec<Vec<String>>,
    ) -> Result<StreamBuilder> {
        if columns.len() != string_dicts.len() {
            bail!("every column needs its string dictionary slot");
        }
        dict.sort_by_key(|(l, _)| (l.block_id, l.in_off));
        let dict_index: HashMap<PieceHash, u32> =
            dict.iter().enumerate().map(|(i, (_, h))| (*h, i as u32)).collect();

        // Column ordinals in sorted (key, tag) order — the same rule build_full applies, so the
        // same input yields the same ordinals.
        let mut order: Vec<usize> = (0..columns.len()).collect();
        order.sort_by(|&a, &b| columns[a].cmp(&columns[b]));

        let mut spool_n = 0usize;
        let mut spool = |base: &Path| -> Result<Spool> {
            spool_n += 1;
            Spool::new(base, spool_n)
        };
        let mut cols = Vec::with_capacity(columns.len());
        let mut col_of = HashMap::with_capacity(columns.len());
        for &i in &order {
            let (key, tag) = columns[i].clone();
            col_of.insert((key.clone(), tag), cols.len());
            cols.push(Col {
                key,
                tag,
                dict: string_dicts[i].clone(),
                occurrences: 0,
                dense: true,
                prev_rid: 0,
                zone: ZoneAcc::new(tag),
                val: spool(path)?,
                rid: spool(path)?,
            });
        }

        Ok(StreamBuilder {
            w: Writer::new(path, level)?,
            dict,
            dict_index,
            cols,
            col_of,
            ids: spool(path)?,
            id_restarts: Vec::new(),
            id_stream_len: 0,
            prev_id: Vec::new(),
            prog: spool(path)?,
            prog_off: spool(path)?,
            prog_len: 0,
            layout: spool(path)?,
            layout_off: spool(path)?,
            layout_len: 0,
            tomb: Vec::new(),
            tomb_n: 0,
            tomb_prev: 0,
            rows: 0,
        })
    }

    /// Append one row. Rows must arrive in strictly increasing id order — the k-way merge's
    /// natural output, asserted rather than trusted.
    pub fn push(
        &mut self,
        id: &[u8],
        tomb: bool,
        body: &[BodyOp],
        attrs: &[(String, AttrValue)],
    ) -> Result<()> {
        let row = self.rows;

        // ---- id column, front-coded with restarts ----
        if row > 0 && id <= self.prev_id.as_slice() {
            bail!(
                "streaming builder requires strictly increasing ids: {:?} then {:?}",
                self.prev_id,
                id
            );
        }
        let mut e = Vec::with_capacity(id.len() + 8);
        let shared = if (row as usize).is_multiple_of(RESTART) {
            self.id_restarts.push(self.id_stream_len as u32);
            0
        } else {
            let n = self.prev_id.len().min(id.len());
            let mut i = 0;
            while i < n && self.prev_id[i] == id[i] {
                i += 1;
            }
            i
        };
        put_varint(&mut e, shared as u64);
        put_varint(&mut e, (id.len() - shared) as u64);
        e.extend_from_slice(&id[shared..]);
        self.ids.append(&e)?;
        self.id_stream_len += e.len() as u64;
        self.prev_id.clear();
        self.prev_id.extend_from_slice(id);

        // ---- body program ----
        self.prog_off.append(&self.prog_len.to_le_bytes())?;
        let mut p = Vec::new();
        let emitted =
            body.iter().filter(|op| !matches!(op, BodyOp::Lit(b) if b.is_empty())).count();
        put_varint(&mut p, emitted as u64);
        for op in body {
            match op {
                BodyOp::Lit(b) => {
                    if b.is_empty() {
                        continue;
                    }
                    put_varint(&mut p, ((b.len() as u64) << 1) | OP_LIT);
                    p.extend_from_slice(b);
                }
                BodyOp::Piece { hash, len } => {
                    let idx = *self.dict_index.get(hash).ok_or_else(|| {
                        anyhow::anyhow!("piece {hash} is not in the builder's dictionary")
                    })?;
                    put_varint(&mut p, ((idx as u64) << 1) | OP_PIECE);
                    put_varint(&mut p, *len as u64);
                }
            }
        }
        self.prog.append(&p)?;
        self.prog_len += p.len() as u64;

        // ---- layout + columns ----
        self.layout_off.append(&self.layout_len.to_le_bytes())?;
        let mut l = Vec::new();
        put_varint(&mut l, attrs.len() as u64);
        for (k, v) in attrs {
            let &c = self.col_of.get(&(k.clone(), v.type_tag())).ok_or_else(|| {
                anyhow::anyhow!("attribute {k:?} is outside the declared column universe")
            })?;
            put_varint(&mut l, c as u64);
            let col = &mut self.cols[c];
            // dense means: the k-th occurrence sits at row k, for every occurrence
            col.dense = col.dense && col.occurrences == row;
            let mut d = Vec::new();
            put_varint(&mut d, (row as u32 - col.prev_rid) as u64);
            col.rid.append(&d)?;
            col.prev_rid = row as u32;
            col.occurrences += 1;
            col.zone.add(v);
            match v {
                AttrValue::Str(s) => {
                    let ord = col.dict.binary_search(s).map_err(|_| {
                        anyhow::anyhow!("string value outside the declared dictionary for {k:?}")
                    })?;
                    col.val.append(&(ord as u32).to_le_bytes())?;
                }
                AttrValue::Int(x) => col.val.append(&x.to_le_bytes())?,
                AttrValue::Float(x) => col.val.append(&x.to_bits().to_le_bytes())?,
                AttrValue::Bool(x) => col.val.append(&[u8::from(*x)])?,
            }
        }
        self.layout.append(&l)?;
        self.layout_len += l.len() as u64;

        if tomb {
            put_varint(&mut self.tomb, row - self.tomb_prev);
            self.tomb_prev = row;
            self.tomb_n += 1;
        }

        self.rows += 1;
        Ok(())
    }

    /// Assemble the part: sections in the canonical order `build_full` writes them, one spool
    /// resident at a time.
    pub fn finish(mut self, seq_lo: u64, seq_hi: u64) -> Result<PartMeta> {
        if self.rows > u32::MAX as u64 {
            bail!("{} records exceeds the u32 record count a part footer can name", self.rows);
        }
        let n = self.rows;
        self.prog_off.append(&self.prog_len.to_le_bytes())?;
        self.layout_off.append(&self.layout_len.to_le_bytes())?;

        self.w.section("ids", &self.ids.take()?)?;
        let restarts: Vec<u8> = self.id_restarts.iter().flat_map(|x| x.to_le_bytes()).collect();
        self.w.section("ids.restart", &restarts)?;
        self.w.section("prog", &self.prog.take()?)?;
        self.w.section("prog.off", &self.prog_off.take()?)?;
        self.w.section(
            "pdict.loc",
            &self.dict.iter().flat_map(|(l, _)| l.encode()).collect::<Vec<u8>>(),
        )?;
        self.w
            .section("pdict.hash", &self.dict.iter().flat_map(|(_, h)| h.0).collect::<Vec<u8>>())?;
        let mut hsort: Vec<u32> = (0..self.dict.len() as u32).collect();
        hsort.sort_by_key(|&i| self.dict[i as usize].1 .0);
        let hsort_bytes: Vec<u8> = hsort.iter().flat_map(|x| x.to_le_bytes()).collect();
        self.w.section("pdict.hsort", &hsort_bytes)?;
        let mut bl = bloom::Bloom::with_capacity(self.dict.len());
        for (_, h) in &self.dict {
            bl.insert(h);
        }
        self.w.section("pdict.bloom", &bl.encode())?;
        if self.tomb_n > 0 {
            let mut out = Vec::with_capacity(self.tomb.len() + 4);
            put_varint(&mut out, self.tomb_n);
            out.extend_from_slice(&self.tomb);
            self.w.section("tomb", &out)?;
        }
        self.w.section("layout", &self.layout.take()?)?;
        self.w.section("layout.off", &self.layout_off.take()?)?;

        let mut meta = Vec::new();
        put_varint(&mut meta, self.cols.len() as u64);
        for c in &self.cols {
            put_varint(&mut meta, c.key.len() as u64);
            meta.extend_from_slice(c.key.as_bytes());
            meta.push(c.tag);
            put_varint(&mut meta, c.occurrences);
            meta.push(if c.dense && c.occurrences == n { RID_DENSE } else { RID_DELTA });
        }
        self.w.section("colmeta", &meta)?;
        let zones: Vec<ZoneAcc> = self.cols.iter().map(|c| c.zone.clone()).collect();
        self.w.section("zone", &encode_zones(&zones))?;

        for (i, c) in self.cols.into_iter().enumerate() {
            let dense = c.dense && c.occurrences == n;
            self.w.section(&format!("col.val.{i}"), &c.val.take()?)?;
            let rid = c.rid.take()?;
            if !dense && !rid.is_empty() {
                self.w.section(&format!("col.rid.{i}"), &rid)?;
            }
            if c.tag == 0 && !c.dict.is_empty() {
                let mut d = Vec::new();
                put_varint(&mut d, c.dict.len() as u64);
                for s in &c.dict {
                    put_varint(&mut d, s.len() as u64);
                    d.extend_from_slice(s.as_bytes());
                }
                self.w.section(&format!("col.dict.{i}"), &d)?;
            }
        }

        let meta = PartMeta { n_records: n as u32, seq_lo, seq_hi };
        self.w.finish(meta)?;
        Ok(meta)
    }
}
