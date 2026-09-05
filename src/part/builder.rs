//! The streaming part builder: rows in one at a time, with row-sized sections spooled to disk.
//! Memory holds the piece dictionary, column universe, and admitted restart/tombstone metadata;
//! it never materializes the complete row payload.
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
use crate::part::attrs::{ZoneAcc, RID_DELTA, RID_DENSE};
use crate::part::bloom;
use crate::part::idcol::{put_varint, RESTART};
use crate::types::{AttrValue, Content, PieceHash};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// An append-only overflow file for one unbounded section. Named `<part>.s<N>.tmp`, so a crash
/// leaves only `*.tmp` litter for the store's sweep; deleted on `finish` and best-effort on drop.
///
/// Creation and removal go through the vfs seam so the crash simulator materializes the litter
/// the writer-open sweep must handle. The CONTENT writes deliberately do not: a spool's bytes are
/// never read by writer open or WAL replay, never claimed durable, and never named by published
/// authority — while
/// recording them would push every appended byte into the op log and multiply every sweep by the
/// data volume. The simulator sees spools as the empty files whose names must be swept, which is
/// the whole of what any invariant asks of them.
struct Spool {
    path: PathBuf,
    w: std::io::BufWriter<std::fs::File>,
    len: u64,
    read_limits: crate::read_limits::ReadLimits,
}

impl Spool {
    fn new(base: &Path, n: usize, read_limits: crate::read_limits::ReadLimits) -> Result<Spool> {
        let path = base.with_extension(format!("s{n}.tmp"));
        let f = crate::vfs::create(&path)
            .with_context(|| format!("create spool {}", path.display()))?;
        Ok(Spool { path, w: std::io::BufWriter::new(f), len: 0, read_limits })
    }
    fn append(&mut self, b: &[u8]) -> Result<()> {
        let additional = u64::try_from(b.len()).context("part spool append exceeds u64")?;
        let next = self.len.checked_add(additional).context("part spool length overflows")?;
        if next > u64::from(u32::MAX) {
            bail!("new part section length {next} exceeds the u32 format field");
        }
        self.read_limits.admit_decoded("new streaming part section", next)?;
        self.w.write_all(b)?;
        self.len = next;
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
        let b = crate::vfs::read_file(&self.path)?;
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
    poisoned: bool,
}

/// The streaming builder's public, file-backed face: exactly the surface it always had. The
/// sink-generic engine underneath is `SinkBuilder`, a crate concern — public callers build
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
        let read_limits = read_limits.validate()?;
        validate_stream_universe(&dict, &content_names, &columns, &value_dicts)?;
        admit_stream_universe_sections(&dict, &value_dicts, read_limits)?;
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

fn validate_stream_universe(
    dict: &[(Loc, PieceHash)],
    content_names: &[String],
    columns: &[(String, u8)],
    value_dicts: &[Vec<Vec<u8>>],
) -> Result<()> {
    if columns.len() != value_dicts.len() {
        bail!("every column needs its value dictionary slot");
    }
    u32::try_from(dict.len()).context("piece dictionary exceeds the u32 ordinal domain")?;
    u32::try_from(columns.len())
        .context("attribute-column universe exceeds the u32 ordinal domain")?;

    let mut locations = BTreeSet::new();
    let mut hashes = BTreeSet::new();
    for (location, hash) in dict {
        if location.raw == 0 {
            bail!("piece dictionary contains a zero-length fold location");
        }
        if !locations.insert(*location) {
            bail!("piece dictionary repeats fold location {location:?}");
        }
        if !hashes.insert(*hash) {
            bail!("piece dictionary repeats piece identity {hash}");
        }
    }

    let mut names = BTreeSet::new();
    for name in content_names {
        if name.is_empty() {
            bail!("content column names must not be empty");
        }
        if !names.insert(name.as_bytes()) {
            bail!("content column names must be unique");
        }
    }

    let mut declared_columns = BTreeSet::new();
    for ((key, tag), values) in columns.iter().zip(value_dicts) {
        if key.is_empty() {
            bail!("attribute column names must not be empty");
        }
        if *tag > 7 {
            bail!("attribute column {key:?} has unknown type tag {tag}");
        }
        if !declared_columns.insert((key.as_bytes(), *tag)) {
            bail!("attribute column ({key:?}, {tag}) is duplicated");
        }
        if !matches!(*tag, 0 | 5) && !values.is_empty() {
            bail!("attribute column ({key:?}, {tag}) cannot carry a value dictionary");
        }
        u32::try_from(values.len()).with_context(|| {
            format!("attribute column ({key:?}, {tag}) dictionary exceeds the u32 ordinal domain")
        })?;
        if values.windows(2).any(|pair| pair[0] >= pair[1]) {
            bail!("attribute column ({key:?}, {tag}) dictionary is duplicated or out of order");
        }
        if *tag == 0 {
            for value in values {
                std::str::from_utf8(value)
                    .with_context(|| format!("string dictionary for {key:?} contains non-UTF-8"))?;
            }
        }
    }
    Ok(())
}

fn varint_len(mut value: u64) -> usize {
    let mut bytes = 1;
    while value >= 0x80 {
        value >>= 7;
        bytes += 1;
    }
    bytes
}

fn admitted_section_len(
    read_limits: crate::read_limits::ReadLimits,
    name: &str,
    len: u64,
) -> Result<usize> {
    if len > u64::from(u32::MAX) {
        bail!("new part section {name:?} is {len} bytes; the format caps a section at 4 GiB");
    }
    read_limits.admit_decoded(format!("new part section {name:?}"), len)?;
    usize::try_from(len).context("new part section exceeds this process's address space")
}

fn checked_width(count: usize, width: u64, name: &str) -> Result<u64> {
    u64::try_from(count)
        .context("part collection count exceeds u64")?
        .checked_mul(width)
        .with_context(|| format!("new part section {name:?} length overflows"))
}

fn encoded_dictionary_len(values: &[Vec<u8>]) -> Result<u64> {
    let mut len = u64::try_from(varint_len(values.len() as u64))?;
    for value in values {
        len = len
            .checked_add(u64::try_from(varint_len(value.len() as u64))?)
            .and_then(|total| total.checked_add(value.len() as u64))
            .context("attribute dictionary section length overflows")?;
    }
    Ok(len)
}

fn admit_stream_universe_sections(
    dict: &[(Loc, PieceHash)],
    value_dicts: &[Vec<Vec<u8>>],
    read_limits: crate::read_limits::ReadLimits,
) -> Result<()> {
    admitted_section_len(read_limits, "pdict.loc", checked_width(dict.len(), 12, "pdict.loc")?)?;
    admitted_section_len(read_limits, "pdict.hash", checked_width(dict.len(), 32, "pdict.hash")?)?;
    admitted_section_len(read_limits, "pdict.hsort", checked_width(dict.len(), 4, "pdict.hsort")?)?;
    let bloom = u64::try_from(bloom::Bloom::encoded_len_for_capacity(dict.len())?)?;
    admitted_section_len(read_limits, "pdict.bloom", bloom)?;
    for (index, values) in value_dicts.iter().enumerate() {
        if !values.is_empty() {
            admitted_section_len(
                read_limits,
                &format!("col.dict.{index}"),
                encoded_dictionary_len(values)?,
            )?;
        }
    }
    Ok(())
}

fn try_byte_vec(capacity: usize, label: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)
        .with_context(|| format!("reserve derived part section {label:?}"))?;
    Ok(out)
}

impl<S: crate::vfs::ArtifactSink> SinkBuilder<S> {
    fn admit_derived_sections(&self) -> Result<()> {
        admitted_section_len(
            self.read_limits,
            "ids.restart",
            checked_width(self.id_restarts.len(), 4, "ids.restart")?,
        )?;
        admit_stream_universe_sections(&self.dict, &[], self.read_limits)?;

        let mut cmeta = u64::try_from(varint_len(self.content_cols.len() as u64))?;
        for column in &self.content_cols {
            cmeta = cmeta
                .checked_add(u64::try_from(varint_len(column.name.len() as u64))?)
                .and_then(|len| len.checked_add(column.name.len() as u64))
                .and_then(|len| len.checked_add(varint_len(column.occurrences) as u64))
                .and_then(|len| len.checked_add(1))
                .context("content metadata section length overflows")?;
        }
        admitted_section_len(self.read_limits, "cmeta", cmeta)?;

        if self.tomb_n > 0 {
            let tomb = u64::try_from(varint_len(self.tomb_n))?
                .checked_add(u64::try_from(self.tomb.len())?)
                .context("tombstone section length overflows")?;
            admitted_section_len(self.read_limits, "tomb", tomb)?;
        }
        if !self.cols.is_empty() {
            let mut colmeta = u64::try_from(varint_len(self.cols.len() as u64))?;
            let mut zone = u64::try_from(varint_len(self.cols.len() as u64))?;
            for (index, column) in self.cols.iter().enumerate() {
                colmeta = colmeta
                    .checked_add(u64::try_from(varint_len(column.key.len() as u64))?)
                    .and_then(|len| len.checked_add(column.key.len() as u64))
                    .and_then(|len| len.checked_add(1))
                    .and_then(|len| len.checked_add(varint_len(column.occurrences) as u64))
                    .and_then(|len| len.checked_add(1))
                    .context("attribute metadata section length overflows")?;
                zone = zone
                    .checked_add(column.zone.encoded_len() as u64)
                    .context("zone section length overflows")?;
                if matches!(column.tag, 0 | 5) {
                    admitted_section_len(
                        self.read_limits,
                        &format!("col.dict.{index}"),
                        encoded_dictionary_len(&column.dict)?,
                    )?;
                }
            }
            admitted_section_len(self.read_limits, "colmeta", colmeta)?;
            admitted_section_len(self.read_limits, "zone", zone)?;
        }
        Ok(())
    }

    /// Stream into any sink. `spool_base` is where the unbounded-section overflow files live —
    /// beside the output for a file part, inside the store's transient `-tmp` directory for a
    /// member of the live file. Spools are scratch either way: never durable, never read by
    /// writer open or WAL replay, and swept as litter after a crash.
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
        validate_stream_universe(&dict, &content_names, &columns, &value_dicts)?;
        let read_limits = read_limits.validate()?;
        admit_stream_universe_sections(&dict, &value_dicts, read_limits)?;
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
            Spool::new(spool_base, spool_n, read_limits)
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
            poisoned: false,
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
        if self.poisoned {
            bail!("streaming part builder is poisoned by an earlier row-write failure");
        }
        self.validate_row(id, tomb, contents, attrs)?;
        let result = self.push_prevalidated(id, tomb, contents, attrs);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn validate_row(
        &self,
        id: &[u8],
        tomb: bool,
        contents: &[Content],
        attrs: &[(String, AttrValue)],
    ) -> Result<()> {
        let row = self.rows;
        if row >= u64::from(u32::MAX) {
            bail!("a part cannot contain more than {} records", u32::MAX);
        }
        if id.is_empty() {
            bail!("record id must not be empty");
        }
        std::str::from_utf8(id).context("record id is not UTF-8")?;
        if row > 0 && id <= self.prev_id.as_slice() {
            bail!(
                "streaming builder requires strictly increasing ids: {:?} then {:?}",
                self.prev_id,
                id
            );
        }
        crate::types::validate_contents(contents)?;
        for content in contents {
            if !self.content_of.contains_key(&content.name) {
                bail!("content {:?} is outside the declared column universe", content.name);
            }
            if content.identity.is_none() {
                bail!("content {:?} has no reconstructed-byte identity", content.name);
            }
            for op in &content.ops {
                if let crate::types::ContentOp::Piece { hash, .. } = op {
                    if !self.dict_index.contains_key(hash) {
                        bail!(
                            "content {:?} references piece {hash} outside the declared dictionary",
                            content.name
                        );
                    }
                }
            }
        }
        for (key, value) in attrs {
            if key.is_empty() {
                bail!("attribute name must not be empty");
            }
            let Some(&column) = self.col_of.get(&(key.clone(), value.type_tag())) else {
                bail!("attribute {key:?} is outside the declared column universe");
            };
            match value {
                AttrValue::Str(value)
                    if self.cols[column]
                        .dict
                        .binary_search_by(|candidate| candidate.as_slice().cmp(value.as_bytes()))
                        .is_err() =>
                {
                    bail!("string value outside the declared dictionary for {key:?}");
                }
                AttrValue::Bytes(value) if self.cols[column].dict.binary_search(value).is_err() => {
                    bail!("binary value outside the declared dictionary for {key:?}");
                }
                _ => {}
            }
        }
        if (row as usize).is_multiple_of(RESTART) {
            let bytes = self
                .id_restarts
                .len()
                .checked_add(1)
                .and_then(|count| count.checked_mul(std::mem::size_of::<u32>()))
                .context("id restart table size overflows")?;
            self.read_limits.admit_decoded("new ids.restart section", bytes as u64)?;
        }
        if tomb {
            let next_count = self.tomb_n.checked_add(1).context("tombstone count overflows")?;
            let encoded_delta = varint_len(row - self.tomb_prev);
            let payload = self
                .tomb
                .len()
                .checked_add(encoded_delta)
                .and_then(|len| len.checked_add(varint_len(next_count)))
                .context("tombstone section size overflows")?;
            if payload > u32::MAX as usize {
                bail!("tombstone section length {payload} exceeds the u32 format field");
            }
            self.read_limits.admit_decoded("new tomb section", payload as u64)?;
        }
        Ok(())
    }

    fn push_prevalidated(
        &mut self,
        id: &[u8],
        tomb: bool,
        contents: &[Content],
        attrs: &[(String, AttrValue)],
    ) -> Result<()> {
        let row = self.rows;

        // ---- id column, front-coded with restarts ----
        let mut e = Vec::with_capacity(id.len() + 8);
        let restart = (row as usize).is_multiple_of(RESTART);
        let shared = if restart {
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
        let next_id_stream_len = super::idcol::checked_stream_end(self.id_stream_len, e.len())?;
        if restart {
            self.id_restarts.try_reserve(1).context("reserve the next id restart offset")?;
            self.id_restarts
                .push(u32::try_from(self.id_stream_len).expect("stream length was bounded above"));
        }
        self.ids.append(&e)?;
        self.id_stream_len = next_id_stream_len;
        self.prev_id.clear();
        self.prev_id.extend_from_slice(id);

        // ---- named content columns ----
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
            let mut identity = Vec::with_capacity(32);
            super::content::encode_identity(&mut identity, content)?;
            col.identity.append(&identity)?;
            col.prog_len = col.prog.len;
            col.occurrences =
                col.occurrences.checked_add(1).context("content occurrence overflows")?;
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
            col.occurrences =
                col.occurrences.checked_add(1).context("attribute occurrence overflows")?;
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
        self.layout_len = self.layout.len;

        if tomb {
            let mut encoded = Vec::new();
            put_varint(&mut encoded, row - self.tomb_prev);
            self.tomb.try_reserve(encoded.len()).context("reserve the next tombstone ordinal")?;
            self.tomb.extend_from_slice(&encoded);
            self.tomb_prev = row;
            self.tomb_n = self.tomb_n.checked_add(1).context("tombstone count overflows")?;
        }

        self.rows = self.rows.checked_add(1).context("part row count overflows")?;
        Ok(())
    }

    /// Assemble the part: sections in the canonical order `build_full` writes them, one spool
    /// resident at a time. Returns the sink so a member handle can be carried back to the
    /// registration that names it.
    pub fn finish(mut self, seq_lo: u64, seq_hi: u64) -> Result<(PartMeta, S)> {
        if self.poisoned {
            bail!("streaming part builder is poisoned by an earlier row-write failure");
        }
        if seq_lo > seq_hi {
            bail!("part sequence interval is inverted: {seq_lo}..{seq_hi}");
        }
        if self.rows > u32::MAX as u64 {
            bail!("{} records exceeds the u32 record count a part footer can name", self.rows);
        }
        if let Some(column) = self.content_cols.iter().find(|column| column.occurrences == 0) {
            bail!("declared content column {:?} has no occurrences", column.name);
        }
        if let Some(column) = self.cols.iter().find(|column| column.occurrences == 0) {
            bail!("declared attribute column {:?} has no occurrences", column.key);
        }
        self.admit_derived_sections()?;
        let n = self.rows;
        self.layout_off.append(&self.layout_len.to_le_bytes())?;

        self.w.section("ids", &self.ids.take(self.read_limits, "ids")?)?;
        let restart_capacity = admitted_section_len(
            self.read_limits,
            "ids.restart",
            checked_width(self.id_restarts.len(), 4, "ids.restart")?,
        )?;
        let mut restarts = try_byte_vec(restart_capacity, "ids.restart")?;
        for restart in &self.id_restarts {
            restarts.extend_from_slice(&restart.to_le_bytes());
        }
        self.w.section("ids.restart", &restarts)?;
        let mut cmeta = try_byte_vec(
            admitted_section_len(
                self.read_limits,
                "cmeta",
                self.content_cols.iter().try_fold(
                    u64::try_from(varint_len(self.content_cols.len() as u64))?,
                    |len, column| {
                        len.checked_add(varint_len(column.name.len() as u64) as u64)
                            .and_then(|len| len.checked_add(column.name.len() as u64))
                            .and_then(|len| len.checked_add(varint_len(column.occurrences) as u64))
                            .and_then(|len| len.checked_add(1))
                            .context("content metadata section length overflows")
                    },
                )?,
            )?,
            "cmeta",
        )?;
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
        let mut locations = try_byte_vec(
            admitted_section_len(
                self.read_limits,
                "pdict.loc",
                checked_width(self.dict.len(), 12, "pdict.loc")?,
            )?,
            "pdict.loc",
        )?;
        let mut hashes = try_byte_vec(
            admitted_section_len(
                self.read_limits,
                "pdict.hash",
                checked_width(self.dict.len(), 32, "pdict.hash")?,
            )?,
            "pdict.hash",
        )?;
        for (location, hash) in &self.dict {
            locations.extend_from_slice(&location.encode());
            hashes.extend_from_slice(&hash.0);
        }
        self.w.section("pdict.loc", &locations)?;
        self.w.section("pdict.hash", &hashes)?;
        let mut hsort = Vec::new();
        hsort.try_reserve_exact(self.dict.len()).context("reserve hash-order piece ordinals")?;
        hsort.extend(0..self.dict.len() as u32);
        hsort.sort_by_key(|&i| self.dict[i as usize].1 .0);
        let mut hsort_bytes = try_byte_vec(
            admitted_section_len(
                self.read_limits,
                "pdict.hsort",
                checked_width(self.dict.len(), 4, "pdict.hsort")?,
            )?,
            "pdict.hsort",
        )?;
        for ordinal in hsort {
            hsort_bytes.extend_from_slice(&ordinal.to_le_bytes());
        }
        self.w.section("pdict.hsort", &hsort_bytes)?;
        let mut bl = bloom::Bloom::try_with_capacity(self.dict.len())?;
        for (_, h) in &self.dict {
            bl.insert(h);
        }
        self.w.section("pdict.bloom", &bl.try_encode()?)?;
        if self.tomb_n > 0 {
            let mut out = Vec::with_capacity(self.tomb.len() + 4);
            put_varint(&mut out, self.tomb_n);
            out.extend_from_slice(&self.tomb);
            self.w.section("tomb", &out)?;
        }
        if !self.cols.is_empty() {
            self.w.section("layout", &self.layout.take(self.read_limits, "layout")?)?;
            self.w.section("layout.off", &self.layout_off.take(self.read_limits, "layout.off")?)?;

            let mut meta = Vec::new();
            let meta_capacity = self.cols.iter().try_fold(
                u64::try_from(varint_len(self.cols.len() as u64))?,
                |len, column| {
                    len.checked_add(varint_len(column.key.len() as u64) as u64)
                        .and_then(|len| len.checked_add(column.key.len() as u64))
                        .and_then(|len| len.checked_add(1))
                        .and_then(|len| len.checked_add(varint_len(column.occurrences) as u64))
                        .and_then(|len| len.checked_add(1))
                        .context("attribute metadata section length overflows")
                },
            )?;
            meta.try_reserve_exact(admitted_section_len(
                self.read_limits,
                "colmeta",
                meta_capacity,
            )?)
            .context("reserve attribute metadata section")?;
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
            self.w.section("zone", &crate::part::attrs::try_encode_zones(&zones)?)?;

            for (i, c) in self.cols.into_iter().enumerate() {
                let dense = c.dense && c.occurrences == n;
                self.w.section(
                    &format!("col.val.{i}"),
                    &c.val.take(self.read_limits, &format!("col.val.{i}"))?,
                )?;
                let rid = c.rid.take(self.read_limits, &format!("col.rid.{i}"))?;
                if !dense {
                    self.w.section(&format!("col.rid.{i}"), &rid)?;
                }
                if matches!(c.tag, 0 | 5) {
                    let dictionary_capacity = admitted_section_len(
                        self.read_limits,
                        &format!("col.dict.{i}"),
                        encoded_dictionary_len(&c.dict)?,
                    )?;
                    let mut d = try_byte_vec(dictionary_capacity, &format!("col.dict.{i}"))?;
                    put_varint(&mut d, c.dict.len() as u64);
                    for value in &c.dict {
                        put_varint(&mut d, value.len() as u64);
                        d.extend_from_slice(value);
                    }
                    self.w.section(&format!("col.dict.{i}"), &d)?;
                }
            }
        }

        let meta = PartMeta { n_records: n as u32, seq_lo, seq_hi };
        let sink = self.w.finish(meta)?;
        Ok((meta, sink))
    }
}
