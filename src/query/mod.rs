//! The query lens: parts, read a column at a time, as Arrow.
//!
//! Everything below this module addresses rows by index — `record(r)`, `attrs(r)`, `body(r)` — which is
//! the wrong shape for the storage underneath it. A part keeps one section per column, independently
//! compressed; asking for one column of a million rows should touch one section, and asking for no
//! content should touch the fold zero times. That is what this module provides and what the row API
//! cannot express.
//!
//! # Projection is the whole point
//!
//! Body reconstruction is the expensive operation in this system — it resolves piece references,
//! decompresses fold blocks, and concatenates. A query over attributes alone must never pay it. Here
//! `body` is an ordinary projectable column, so `SELECT model, tokens FROM t WHERE ...` reads two
//! attribute sections and **opens no fold block at all**. [`ScanStats::fold_reads`] records this so the
//! claim is measured rather than asserted.
//!
//! # Strings stay dictionary-encoded
//!
//! A part stores string columns as ordinals into a sorted distinct dictionary. Materialising them into
//! a flat `StringArray` at the Arrow boundary would throw that away exactly where a query engine can
//! most use it, so they surface as `Dictionary(Int32, Utf8)` and comparisons run on ordinals.
//!
//! # One row shape, from many parts
//!
//! Columns are keyed `(name, type)`, so a key carrying different types in different records yields
//! several homogeneous columns. The lens names them `key` when a key has exactly one type across the
//! scanned parts and `key#type` when it does not — never a silent merge of two types into one field.

pub mod table;

use crate::fold::Fold;
use crate::part::{attrs, Part};
use anyhow::{bail, Result};
use datafusion::arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Float64Builder, Int32Array, Int64Builder, StringArray,
    StringBuilder,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::{RecordBatch, RecordBatchOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Rows per batch, when nothing bounds it sooner.
pub const BATCH_ROWS: usize = 8192;

/// Byte ceiling for a batch's reconstructed content.
///
/// Row count alone is the wrong bound for a `body` column: trace bodies here average ~97 KiB, so 8192
/// of them is a 795 MiB batch. Records are unbounded in size and rows are not fungible, so a batch
/// closes on whichever limit is reached first. Attribute-only scans never come near this.
pub const BATCH_BYTES: usize = 32 << 20;

/// The always-present columns. Both are synthesised rather than stored as attribute columns: `id` from
/// the front-coded id section, `body` from the fold.
pub const F_ID: &str = "id";
pub const F_BODY: &str = "body";

fn type_name(tag: u8) -> &'static str {
    match tag {
        0 => "str",
        1 => "int",
        2 => "float",
        3 => "bool",
        _ => "unknown",
    }
}

fn arrow_type(tag: u8) -> DataType {
    match tag {
        0 => DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
        1 => DataType::Int64,
        2 => DataType::Float64,
        3 => DataType::Boolean,
        _ => DataType::Null,
    }
}

/// What a scan actually touched. Exists so that "projection avoids the fold" is a measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub rows: usize,
    pub batches: usize,
    /// Attribute column sections decoded. A projected scan should decode only what it projects.
    pub columns_decoded: usize,
    /// Records whose body was reconstructed out of the fold. Zero unless `body` was projected.
    pub fold_reads: usize,
    /// Occurrences hidden by the flat column view: a row that names one key several times surfaces its
    /// FIRST value here. Counted, never silent — the full interleaved sequence stays available through
    /// [`Part::record`].
    pub shadowed_occurrences: usize,
}

impl ScanStats {
    /// What happened between `prev` and now. A lazy scan publishes progress by delta, so a query that
    /// stops early (LIMIT, an error, a dropped stream) reports what it actually touched.
    pub fn since(&self, prev: ScanStats) -> ScanStats {
        ScanStats {
            rows: self.rows - prev.rows,
            batches: self.batches - prev.batches,
            columns_decoded: self.columns_decoded - prev.columns_decoded,
            fold_reads: self.fold_reads - prev.fold_reads,
            shadowed_occurrences: self.shadowed_occurrences - prev.shadowed_occurrences,
        }
    }

    pub fn add(&mut self, o: ScanStats) {
        self.rows += o.rows;
        self.batches += o.batches;
        self.columns_decoded += o.columns_decoded;
        self.fold_reads += o.fold_reads;
        self.shadowed_occurrences += o.shadowed_occurrences;
    }
}

/// A stable row shape over a set of parts, and the machinery to read batches through it.
pub struct Lens {
    schema: SchemaRef,
    /// Per schema field beyond `id`/`body`: the `(key, tag)` it resolves to inside a part.
    binding: Vec<Option<(String, u8)>>,
}

impl Lens {
    /// Derive the row shape from the parts that will be scanned.
    pub fn new(parts: &[Arc<Part>]) -> Result<Lens> {
        // Which types does each key carry, across every part?
        let mut tags: BTreeMap<String, BTreeSet<u8>> = BTreeMap::new();
        for p in parts {
            if !p.has_columns() {
                continue;
            }
            for (key, tag, _, _) in attrs::read_meta(p)? {
                tags.entry(key).or_default().insert(tag);
            }
        }

        let mut fields = vec![
            Field::new(F_ID, DataType::Utf8, false),
            Field::new(F_BODY, DataType::Binary, true),
        ];
        let mut binding = vec![None, None];
        for (key, ts) in &tags {
            for &t in ts {
                // A key with one type keeps its name. A key with several is never silently merged.
                let name = if ts.len() == 1 { key.clone() } else { format!("{key}#{}", type_name(t)) };
                fields.push(Field::new(&name, arrow_type(t), true));
                binding.push(Some((key.clone(), t)));
            }
        }
        Ok(Lens { schema: Arc::new(Schema::new(fields)), binding })
    }

    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// Field indices for a projection given by name, in the order asked for.
    pub fn project(&self, names: &[&str]) -> Result<Vec<usize>> {
        names
            .iter()
            .map(|n| {
                self.schema
                    .index_of(n)
                    .map_err(|_| anyhow::anyhow!("no column {n:?} in this store"))
            })
            .collect()
    }

    /// Stream `part` as batches under `projection` (field indices into [`Lens::schema`]).
    ///
    /// `fold` is required only if the projection includes `body`; passing `None` otherwise makes it
    /// impossible for a scan to touch content by accident.
    pub fn scan(
        &self,
        part: &Arc<Part>,
        fold: Option<&Arc<Fold>>,
        projection: &[usize],
    ) -> Result<PartScan> {
        let proj: Vec<usize> = projection.to_vec();
        for &f in &proj {
            if f >= self.binding.len() {
                bail!("projection names field {f}, past the end of the schema");
            }
        }
        if proj.iter().any(|&f| self.schema.field(f).name() == F_BODY) && fold.is_none() {
            bail!("the body column was projected but no fold was supplied to read it from");
        }
        let fields: Vec<Field> = proj.iter().map(|&f| self.schema.field(f).clone()).collect();
        let out_schema = Arc::new(Schema::new(fields));

        // Resolve each projected field to a part-local column ONCE, not per batch. A field this part
        // lacks resolves to None and yields nulls, which is how parts with different columns share a
        // schema.
        let meta = if part.has_columns() { attrs::read_meta(part)? } else { Vec::new() };
        let mut cols: Vec<Col> = Vec::with_capacity(proj.len());
        let mut decoded = 0usize;
        for &f in &proj {
            let name = self.schema.field(f).name().as_str();
            if name == F_ID {
                cols.push(Col::Id);
            } else if name == F_BODY {
                cols.push(Col::Body);
            } else {
                let want = self.binding[f].clone().expect("only id/body are unbound");
                match meta.iter().position(|(k, t, _, _)| *k == want.0 && *t == want.1) {
                    Some(c) => {
                        let (_, tag, occ, kind) = meta[c].clone();
                        decoded += 1;
                        cols.push(Col::Attr {
                            tag,
                            rids: attrs::rids(part, c, occ, kind)?,
                            val: part.column_values(c)?,
                            dict: attrs::read_dict(part, c)?,
                        });
                    }
                    None => cols.push(Col::Missing(self.schema.field(f).data_type().clone())),
                }
            }
        }

        // Ids are decoded once for the whole part and sliced per batch — the column is front-coded, so
        // there is no meaningful way to start decoding from the middle.
        let ids = if cols.iter().any(|c| matches!(c, Col::Id)) { part.ids()? } else { Vec::new() };

        Ok(PartScan {
            n: part.len(),
            part: part.clone(),
            fold: fold.cloned(),
            schema: out_schema,
            cols,
            ids,
            row: 0,
            stats: ScanStats { columns_decoded: decoded, ..ScanStats::default() },
            fetch: None,
        })
    }
}

enum Col {
    Id,
    Body,
    Attr { tag: u8, rids: Arc<Vec<u32>>, val: Arc<Vec<u8>>, dict: Arc<Vec<String>> },
    /// This part has no such column; the batch contributes nulls of the right type.
    Missing(DataType),
}

/// A lazy batch stream over one part. Holds every column's decoded handles, so per-batch work is a
/// bounded scatter and nothing is re-decoded.
///
/// Owns its handles rather than borrowing them — everything it needs is already reference-counted, and
/// a borrowing scan could never become a `'static` stream, which is what a query engine requires.
pub struct PartScan {
    part: Arc<Part>,
    fold: Option<Arc<Fold>>,
    schema: SchemaRef,
    cols: Vec<Col>,
    ids: Vec<String>,
    row: usize,
    n: usize,
    stats: ScanStats,
    /// Stop after this many rows. A LIMIT the query engine pushed down.
    fetch: Option<usize>,
}

impl PartScan {
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// What this scan has touched so far. Grows as batches are pulled, because the scan is lazy.
    pub fn stats(&self) -> ScanStats {
        self.stats
    }

    pub fn rows_remaining(&self) -> usize {
        self.n - self.row
    }

    /// Bound this scan to `n` rows.
    ///
    /// Streaming alone bounds MEMORY, not WORK: a consumer that wants one row still triggers a whole
    /// batch, and a batch of bodies is BATCH_BYTES of fold reads — per part, since every partition
    /// executes. Carrying the limit down is what makes `LIMIT 1` cost one row instead of one batch
    /// times the part count.
    pub fn with_fetch(mut self, n: Option<usize>) -> Self {
        self.fetch = n;
        self
    }

    /// The next batch, or `None` at the end of the part.
    pub fn next_batch(&mut self) -> Result<Option<RecordBatch>> {
        let end = match self.fetch {
            Some(f) => self.n.min(f),
            None => self.n,
        };
        if self.row >= end {
            return Ok(None);
        }
        let lo = self.row;
        let cap = (lo + BATCH_ROWS).min(end);

        // Content is reconstructed FIRST, because it is what decides how many rows this batch can hold.
        // Every other column then follows that decision, so all arrays end up the same length.
        let mut bodies: Option<Vec<Vec<u8>>> = None;
        if self.cols.iter().any(|c| matches!(c, Col::Body)) {
            let fold = self.fold.as_ref().expect("checked when the scan was built");
            let mut v = Vec::new();
            let mut bytes = 0usize;
            for r in lo..cap {
                let b = self.part.reconstruct(r, fold)?;
                bytes += b.len();
                v.push(b);
                // At least one row always lands, however large it is — otherwise a single record
                // bigger than the ceiling could never be read at all.
                if bytes >= BATCH_BYTES {
                    break;
                }
            }
            bodies = Some(v);
        }
        let hi = match &bodies {
            Some(v) => lo + v.len(),
            None => cap,
        };
        let len = hi - lo;
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(self.cols.len());

        for c in &self.cols {
            arrays.push(match c {
                Col::Id => Arc::new(StringArray::from(
                    self.ids[lo..hi].iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                )) as ArrayRef,
                Col::Body => {
                    let mut b = BinaryBuilder::new();
                    for v in bodies.as_ref().expect("built above when body is projected") {
                        b.append_value(v);
                    }
                    self.stats.fold_reads += len;
                    Arc::new(b.finish()) as ArrayRef
                }
                Col::Missing(t) => datafusion::arrow::array::new_null_array(t, len),
                Col::Attr { tag, rids, val, dict } => {
                    scatter(*tag, rids, val, dict, lo, hi, &mut self.stats.shadowed_occurrences)?
                }
            });
        }

        self.row = hi;
        self.stats.rows += len;
        self.stats.batches += 1;
        // The row count is carried explicitly because a projection can legitimately be EMPTY —
        // `SELECT count(*)` needs the cardinality and nothing else, and a batch with no columns has
        // no other way to say how many rows it stands for.
        Ok(Some(RecordBatch::try_new_with_options(
            self.schema.clone(),
            arrays,
            &RecordBatchOptions::new().with_row_count(Some(len)),
        )?))
    }
}

/// Scatter a sparse `(rid, val)` column into a dense array over rows `lo..hi`.
///
/// Rows absent from `rid` become null. A row present more than once keeps its FIRST occurrence and
/// counts the rest as shadowed — a flat column cannot represent a repeated key, and dropping them
/// silently would be a lie about the data.
fn scatter(
    tag: u8,
    rids: &[u32],
    val: &[u8],
    dict: &[String],
    lo: usize,
    hi: usize,
    shadowed: &mut usize,
) -> Result<ArrayRef> {
    let len = hi - lo;
    let start = rids.partition_point(|&x| (x as usize) < lo);
    let mut taken: Vec<Option<usize>> = vec![None; len];
    let mut k = start;
    while k < rids.len() && (rids[k] as usize) < hi {
        let slot = rids[k] as usize - lo;
        if taken[slot].is_none() {
            taken[slot] = Some(k);
        } else {
            *shadowed += 1;
        }
        k += 1;
    }

    let w = attrs::width(tag);
    let at = |k: usize| -> Result<&[u8]> {
        let o = k * w;
        if o + w > val.len() {
            bail!("column value {k} runs past its section");
        }
        Ok(&val[o..o + w])
    };

    Ok(match tag {
        // Ordinals stay ordinals: the part's sorted distinct dictionary IS an Arrow dictionary, so the
        // encoding survives the boundary instead of being flattened into repeated strings.
        0 => {
            let keys: Vec<Option<i32>> = taken
                .iter()
                .map(|t| match *t {
                    Some(k) => {
                        let o = k * 4;
                        if o + 4 > val.len() {
                            None
                        } else {
                            Some(u32::from_le_bytes(val[o..o + 4].try_into().unwrap()) as i32)
                        }
                    }
                    None => None,
                })
                .collect();
            let values = StringArray::from(dict.iter().map(|s| s.as_str()).collect::<Vec<_>>());
            Arc::new(datafusion::arrow::array::DictionaryArray::try_new(
                Int32Array::from(keys),
                Arc::new(values),
            )?) as ArrayRef
        }
        1 => {
            let mut b = Int64Builder::with_capacity(len);
            for t in &taken {
                match *t {
                    Some(k) => b.append_value(i64::from_le_bytes(at(k)?.try_into().unwrap())),
                    None => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        2 => {
            let mut b = Float64Builder::with_capacity(len);
            for t in &taken {
                match *t {
                    // from_bits, not from a float parse: -0.0 and NaN payloads round-trip exactly
                    Some(k) => b.append_value(f64::from_bits(u64::from_le_bytes(at(k)?.try_into().unwrap()))),
                    None => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        3 => {
            let mut b = BooleanBuilder::with_capacity(len);
            for t in &taken {
                match *t {
                    Some(k) => b.append_value(at(k)?[0] != 0),
                    None => b.append_null(),
                }
            }
            Arc::new(b.finish())
        }
        t => bail!("unknown attribute type tag {t}"),
    })
}

/// Materialise every batch of a projection over many parts. Convenience for tests and small scans;
/// large scans should drive [`PartScan`] directly, or go through DataFusion.
pub fn collect(
    parts: &[Arc<Part>],
    fold: Option<&Arc<Fold>>,
    lens: &Lens,
    projection: &[usize],
) -> Result<(Vec<RecordBatch>, ScanStats)> {
    let mut stats = ScanStats::default();
    let mut out = Vec::new();
    for p in parts {
        let mut sc = lens.scan(p, fold, projection)?;
        while let Some(b) = sc.next_batch()? {
            out.push(b);
        }
        stats.add(sc.stats());
    }
    Ok((out, stats))
}

/// A `StringBuilder`-based flat view of a dictionary column, for callers that want plain strings.
pub fn flatten_strings(a: &ArrayRef) -> Result<ArrayRef> {
    use datafusion::arrow::array::{Array, AsArray};
    let d = a
        .as_any()
        .downcast_ref::<datafusion::arrow::array::DictionaryArray<datafusion::arrow::datatypes::Int32Type>>()
        .ok_or_else(|| anyhow::anyhow!("not a dictionary column"))?;
    let values = d.values().as_string::<i32>();
    let mut b = StringBuilder::new();
    for i in 0..d.len() {
        match d.key(i) {
            Some(k) => b.append_value(values.value(k)),
            None => b.append_null(),
        }
    }
    Ok(Arc::new(b.finish()))
}
