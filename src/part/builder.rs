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

use super::{FilePartSink, PartMeta, Writer};
use crate::fold::Loc;
use crate::part::attrs::{encode_zones, ZoneAcc, RID_DELTA, RID_DENSE};
use crate::part::bloom;
use crate::part::idcol::{put_varint, RESTART};
use crate::types::{AttrValue, Content, PieceHash};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// An append-only overflow file for one unbounded section. Named `<part>.s<N>.tmp`, so a crash
/// leaves only `*.tmp` litter for the store's sweep; deleted on `finish` and best-effort on drop.
///
/// Creation and removal go through the vfs seam so the crash simulator materializes the litter
/// the writer-open sweep must handle. The CONTENT writes deliberately do not: a spool's bytes are
/// never read by recovery, never claimed durable, and never named by anything committed — while
/// recording them would push every appended byte into the op log and multiply every sweep by the
/// data volume. The simulator sees spools as the empty files whose names must be swept, which is
/// the whole of what any invariant asks of them.
struct Spool {
    path: PathBuf,
    w: std::io::BufWriter<std::fs::File>,
    len: u64,
}

impl Spool {
    fn new(base: &Path, n: usize) -> Result<Spool> {
        let path = base.with_extension(format!("s{n}.tmp"));
        let f = crate::vfs::create(&path)
            .with_context(|| format!("create spool {}", path.display()))?;
        Ok(Spool { path, w: std::io::BufWriter::new(f), len: 0 })
    }
    fn append(&mut self, b: &[u8]) -> Result<()> {
        self.w.write_all(b)?;
        self.len += b.len() as u64;
        Ok(())
    }
    /// Everything appended so far, loaded whole — `finish`'s per-section working set.
    fn take(
        mut self,
        read_limits: crate::read_limits::ReadLimits,
        section: &str,
    ) -> Result<Vec<u8>> {
        read_limits.admit_decoded(format!("new part section {section:?}"), self.len)?;
        self.w.flush()?;
        let b = std::fs::read(&self.path)?;
        let _ = crate::vfs::unlink(&self.path);
        Ok(b)
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        let _ = crate::vfs::unlink(&self.path);
    }
}

/// One column's streaming state. Ordinals were fixed before the first row, so values append as
/// they arrive; whether `rid` was dense — and therefore elided — is only known at the end, so
/// deltas spool unconditionally and a dense column discards its spool.
struct Col {
    key: String,
    tag: u8,
    /// Sorted distinct values for string/binary columns; the ordinal space `val` writes into.
    dict: Vec<Vec<u8>>,
    occurrences: u64,
    dense: bool,
    prev_rid: u32,
    zone: ZoneAcc,
    val: Spool,
    rid: Spool,
}

struct ContentCol {
    name: String,
    occurrences: u64,
    dense: bool,
    prev_rid: u64,
    prog: Spool,
    off: Spool,
    rid: Spool,
    identity: Spool,
    prog_len: u64,
}

pub(crate) struct SinkBuilder<S: crate::vfs::ArtifactSink> {
    w: Writer<S>,
    dict: Vec<(Loc, PieceHash)>,
    dict_index: HashMap<PieceHash, u32>,
    cols: Vec<Col>,
    col_of: HashMap<(String, u8), usize>,
    content_cols: Vec<ContentCol>,
    content_of: HashMap<String, usize>,

    ids: Spool,
    id_restarts: Vec<u32>,
    id_stream_len: u64,
    prev_id: Vec<u8>,

    layout: Spool,
    layout_off: Spool,
    layout_len: u64,

    tomb: Vec<u8>,
    tomb_n: u64,
    tomb_prev: u64,

    rows: u64,
    read_limits: crate::read_limits::ReadLimits,
}

/// The streaming builder's public, file-backed face: exactly the surface it always had. The
/// sink-generic engine underneath is [`SinkBuilder`], a crate concern — public callers build
/// parts as files; the engine builds them wherever its store lives.
pub struct StreamBuilder(SinkBuilder<FilePartSink>);

impl StreamBuilder {
    /// `dict` is the full piece dictionary the part will carry (referenced plus retained), in ANY
    /// order — it is sorted to fold order here, exactly as `build_full` sorts it. `columns` is the
    /// exact `(key, tag)` universe of the rows that will be pushed, with each string column's
    /// sorted-distinct byte dictionary in `value_dicts` (empty for fixed-width columns).
    pub fn new(
        path: &Path,
        level: i32,
        dict: Vec<(Loc, PieceHash)>,
        content_names: Vec<String>,
        columns: Vec<(String, u8)>,
        value_dicts: Vec<Vec<Vec<u8>>>,
    ) -> Result<StreamBuilder> {
        Self::new_with_limits(
            path,
            level,
            dict,
            content_names,
            columns,
            value_dicts,
            crate::read_limits::ReadLimits::default(),
        )
    }

    /// [`StreamBuilder::new`] with atomic-frame limits applied while spools become sections.
    pub fn new_with_limits(
        path: &Path,
        level: i32,
        dict: Vec<(Loc, PieceHash)>,
        content_names: Vec<String>,
        columns: Vec<(String, u8)>,
        value_dicts: Vec<Vec<Vec<u8>>>,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<StreamBuilder> {
        Ok(StreamBuilder(SinkBuilder::over(
            FilePartSink::create(path)?,
            path,
            level,
            dict,
            content_names,
            columns,
            value_dicts,
            read_limits,
        )?))
    }

    /// Append one row. Rows must arrive in strictly increasing id order.
    pub fn push(
        &mut self,
        id: &[u8],
        tomb: bool,
        contents: &[Content],
        attrs: &[(String, AttrValue)],
    ) -> Result<()> {
        self.0.push(id, tomb, contents, attrs)
    }

    /// Assemble the part; the file is fsynced before this returns.
    pub fn finish(self, seq_lo: u64, seq_hi: u64) -> Result<PartMeta> {
        let (meta, _sink) = self.0.finish(seq_lo, seq_hi)?;
        Ok(meta)
    }
}

impl<S: crate::vfs::ArtifactSink> SinkBuilder<S> {
    /// Stream into any sink. `spool_base` is where the unbounded-section overflow files live —
    /// beside the output for a file part, inside the store's transient `-tmp` directory for a
    /// member of the live file. Spools are scratch either way: never durable, never read by
    /// recovery, swept as litter after a crash.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn over(
        sink: S,
        spool_base: &Path,
        level: i32,
        mut dict: Vec<(Loc, PieceHash)>,
        mut content_names: Vec<String>,
        columns: Vec<(String, u8)>,
        value_dicts: Vec<Vec<Vec<u8>>>,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<SinkBuilder<S>> {
        let read_limits = read_limits.validate()?;
        if columns.len() != value_dicts.len() {
            bail!("every column needs its value dictionary slot");
        }
        dict.sort_by_key(|(l, _)| (l.block_id, l.in_off));
        let dict_index: HashMap<PieceHash, u32> =
            dict.iter().enumerate().map(|(i, (_, h))| (*h, i as u32)).collect();

        // Column ordinals in sorted (key, tag) order — the same rule build_full applies, so the
        // same input yields the same ordinals.
        let mut order: Vec<usize> = (0..columns.len()).collect();
        order.sort_by(|&a, &b| columns[a].cmp(&columns[b]));

        let mut spool_n = 0usize;
        let mut spool = |_ignored: &Path| -> Result<Spool> {
            spool_n += 1;
            Spool::new(spool_base, spool_n)
        };
        let mut cols = Vec::with_capacity(columns.len());
        let mut col_of = HashMap::with_capacity(columns.len());
        for &i in &order {
            let (key, tag) = columns[i].clone();
            col_of.insert((key.clone(), tag), cols.len());
            cols.push(Col {
                key,
                tag,
                dict: value_dicts[i].clone(),
                occurrences: 0,
                dense: true,
                prev_rid: 0,
                zone: ZoneAcc::new(tag),
                val: spool(spool_base)?,
                rid: spool(spool_base)?,
            });
        }

        content_names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        if content_names.iter().any(String::is_empty) {
            bail!("content column names must not be empty");
        }
        if content_names.windows(2).any(|w| w[0] == w[1]) {
            bail!("content column names must be unique");
        }
        let mut content_cols = Vec::with_capacity(content_names.len());
        let mut content_of = HashMap::with_capacity(content_names.len());
        for name in content_names {
            content_of.insert(name.clone(), content_cols.len());
            content_cols.push(ContentCol {
                name,
                occurrences: 0,
                dense: true,
                prev_rid: 0,
                prog: spool(spool_base)?,
                off: spool(spool_base)?,
                rid: spool(spool_base)?,
                identity: spool(spool_base)?,
                prog_len: 0,
            });
        }

        Ok(SinkBuilder {
            w: Writer::over(sink, level, read_limits)?,
            dict,
            dict_index,
            cols,
            col_of,
            content_cols,
            content_of,
            ids: spool(spool_base)?,
            id_restarts: Vec::new(),
            id_stream_len: 0,
            prev_id: Vec::new(),
            layout: spool(spool_base)?,
            layout_off: spool(spool_base)?,
            layout_len: 0,
            tomb: Vec::new(),
            tomb_n: 0,
            tomb_prev: 0,
            rows: 0,
            read_limits,
        })
    }

    /// Append one row. Rows must arrive in strictly increasing id order — the k-way merge's
    /// natural output, asserted rather than trusted.
    pub fn push(
        &mut self,
        id: &[u8],
        tomb: bool,
        contents: &[Content],
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

        // ---- named content columns ----
        crate::types::validate_contents(contents)?;
        for content in contents {
            let &c = self.content_of.get(&content.name).ok_or_else(|| {
                anyhow::anyhow!(
                    "content {:?} is outside the declared column universe",
                    content.name
                )
            })?;
            let col = &mut self.content_cols[c];
            col.dense = col.dense && col.occurrences == row;
            let mut d = Vec::new();
            put_varint(&mut d, row - col.prev_rid);
            col.rid.append(&d)?;
            col.prev_rid = row;
            col.off.append(&col.prog_len.to_le_bytes())?;
            let mut p = Vec::new();
            super::content::encode_program(&mut p, &content.ops, &self.dict_index)?;
            col.prog.append(&p)?;
            let mut identity = Vec::with_capacity(33);
            super::content::encode_identity(&mut identity, content);
            col.identity.append(&identity)?;
            col.prog_len += p.len() as u64;
            col.occurrences += 1;
        }

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
                    let ord = col
                        .dict
                        .binary_search_by(|value| value.as_slice().cmp(s.as_bytes()))
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "string value outside the declared dictionary for {k:?}"
                            )
                        })?;
                    col.val.append(&(ord as u32).to_le_bytes())?;
                }
                AttrValue::Int(x) => col.val.append(&x.to_le_bytes())?,
                AttrValue::Float(x) => col.val.append(&x.to_bits().to_le_bytes())?,
                AttrValue::Bool(x) => col.val.append(&[u8::from(*x)])?,
                AttrValue::UInt(x) => col.val.append(&x.to_le_bytes())?,
                AttrValue::Bytes(bytes) => {
                    let ord = col.dict.binary_search(bytes).map_err(|_| {
                        anyhow::anyhow!("binary value outside the declared dictionary for {k:?}")
                    })?;
                    col.val.append(&(ord as u32).to_le_bytes())?;
                }
                AttrValue::TimestampNs(ns) => col.val.append(&ns.to_le_bytes())?,
                AttrValue::Null => {}
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
    /// resident at a time. Returns the sink so a member handle can be carried back to the
    /// registration that names it.
    pub fn finish(mut self, seq_lo: u64, seq_hi: u64) -> Result<(PartMeta, S)> {
        if self.rows > u32::MAX as u64 {
            bail!("{} records exceeds the u32 record count a part footer can name", self.rows);
        }
        let n = self.rows;
        self.layout_off.append(&self.layout_len.to_le_bytes())?;

        self.w.section("ids", &self.ids.take(self.read_limits, "ids")?)?;
        let restarts: Vec<u8> = self.id_restarts.iter().flat_map(|x| x.to_le_bytes()).collect();
        self.w.section("ids.restart", &restarts)?;
        let mut cmeta = Vec::new();
        put_varint(&mut cmeta, self.content_cols.len() as u64);
        for c in &self.content_cols {
            put_varint(&mut cmeta, c.name.len() as u64);
            cmeta.extend_from_slice(c.name.as_bytes());
            put_varint(&mut cmeta, c.occurrences);
            cmeta.push(if c.dense && c.occurrences == n {
                super::content::RID_DENSE
            } else {
                super::content::RID_DELTA
            });
        }
        self.w.section("cmeta", &cmeta)?;
        for (i, mut c) in self.content_cols.into_iter().enumerate() {
            c.off.append(&c.prog_len.to_le_bytes())?;
            self.w.section(
                &format!("con.prog.{i}"),
                &c.prog.take(self.read_limits, &format!("con.prog.{i}"))?,
            )?;
            self.w.section(
                &format!("con.off.{i}"),
                &c.off.take(self.read_limits, &format!("con.off.{i}"))?,
            )?;
            self.w.section(
                &format!("con.id.{i}"),
                &c.identity.take(self.read_limits, &format!("con.id.{i}"))?,
            )?;
            let rid = c.rid.take(self.read_limits, &format!("con.rid.{i}"))?;
            if !(c.dense && c.occurrences == n) {
                self.w.section(&format!("con.rid.{i}"), &rid)?;
            }
        }
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
        self.w.section("layout", &self.layout.take(self.read_limits, "layout")?)?;
        self.w.section("layout.off", &self.layout_off.take(self.read_limits, "layout.off")?)?;

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
            self.w.section(
                &format!("col.val.{i}"),
                &c.val.take(self.read_limits, &format!("col.val.{i}"))?,
            )?;
            let rid = c.rid.take(self.read_limits, &format!("col.rid.{i}"))?;
            if !dense && !rid.is_empty() {
                self.w.section(&format!("col.rid.{i}"), &rid)?;
            }
            if matches!(c.tag, 0 | 5) && !c.dict.is_empty() {
                let mut d = Vec::new();
                put_varint(&mut d, c.dict.len() as u64);
                for value in &c.dict {
                    put_varint(&mut d, value.len() as u64);
                    d.extend_from_slice(value);
                }
                self.w.section(&format!("col.dict.{i}"), &d)?;
            }
        }

        let meta = PartMeta { n_records: n as u32, seq_lo, seq_hi };
        let sink = self.w.finish(meta)?;
        Ok((meta, sink))
    }
}
