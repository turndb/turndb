//! Attribute columns — the typed, queryable plane.
//!
//! One logical column per `(key, type_tag)`, so a key that carries different types across records
//! yields several homogeneous columns rather than one column that can mis-decode.
//!
//! A column is a **sparse pair of parallel arrays**: `rid` (the row each occurrence belongs to,
//! ascending) and `val` (fixed width, so the k-th value is directly addressable). Duplicates are
//! natural — a row simply appears more than once in `rid`, and occurrences stay in original order.
//! Monotonic `rid` runs compress to almost nothing under the section's zstd.
//!
//! Column storage alone cannot reproduce a row's *interleaving* — `[a, b, a]` and `[a, a, b]` have
//! identical columns. So each row also stores a **layout**: the exact sequence of column ordinals it
//! used. Reconstruction walks the layout and draws the next value from each named column.

use super::Part;
use crate::part::idcol::{get_varint, put_varint};
use crate::types::{AttrValue, Record};
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::collections::HashSet;

/// Bytes per value, by type tag. Fixed width keeps `val` directly indexable; zstd removes the slack.
pub fn width(tag: u8) -> usize {
    match tag {
        0 => 4, // dictionary ordinal
        1 => 8, // i64
        2 => 8, // f64 bits — preserves NaN payloads and -0.0 exactly
        3 => 1, // bool
        4 => 8, // u64
        5 => 4, // binary dictionary ordinal
        6 => 8, // signed Unix nanoseconds
        7 => 0, // explicit null: presence lives in rid/layout
        _ => 0,
    }
}

/// How a column's row-index array is stored.
pub const RID_DENSE: u8 = 0;
/// Ascending deltas as varints. Repeats (a duplicated key on one row) encode as a zero.
pub const RID_DELTA: u8 = 1;

pub struct BuiltCol {
    /// Empty when `kind == RID_DENSE`.
    pub rid: Vec<u8>,
    pub kind: u8,
    pub val: Vec<u8>,
    /// Empty for non-string columns.
    pub dict: Vec<u8>,
}

pub struct BuiltCols {
    pub layout: Vec<u8>,
    pub layout_off: Vec<u64>,
    pub meta: Vec<u8>,
    /// The encoded `zone` section — per-column min/max, advisory.
    pub zones: Vec<u8>,
    pub cols: Vec<BuiltCol>,
}

/// Build every column plus the per-row layout, from rows already in id order.
pub fn build(rows: &[&Record]) -> Result<BuiltCols> {
    // Column ordinals are assigned in sorted (key, tag) order so the same input always yields the
    // same ordinals — insertion order would make the output depend on arrival order.
    let mut keys: BTreeMap<(String, u8), usize> = BTreeMap::new();
    for r in rows {
        for (k, v) in &r.attrs {
            keys.entry((k.clone(), v.type_tag())).or_insert(0);
        }
    }
    let cols: Vec<(String, u8)> = keys.keys().cloned().collect();
    for (i, k) in cols.iter().enumerate() {
        keys.insert(k.clone(), i);
    }

    let mut rid: Vec<Vec<u32>> = vec![Vec::new(); cols.len()];
    let mut raw: Vec<Vec<AttrValue>> = vec![Vec::new(); cols.len()];
    let mut layout = Vec::new();
    let mut layout_off = Vec::with_capacity(rows.len() + 1);

    for (ri, r) in rows.iter().enumerate() {
        layout_off.push(layout.len() as u64);
        put_varint(&mut layout, r.attrs.len() as u64);
        for (k, v) in &r.attrs {
            let c = keys[&(k.clone(), v.type_tag())];
            put_varint(&mut layout, c as u64);
            rid[c].push(ri as u32);
            raw[c].push(v.clone());
        }
    }
    layout_off.push(layout.len() as u64);

    let mut out_cols = Vec::with_capacity(cols.len());
    let mut meta = Vec::new();
    put_varint(&mut meta, cols.len() as u64);

    for (c, (key, tag)) in cols.iter().enumerate() {
        let mut dict_bytes = Vec::new();
        let mut val = Vec::with_capacity(raw[c].len() * width(*tag));
        match tag {
            0 => {
                // sorted distinct strings; ordinals index into it
                let mut distinct: Vec<&str> = raw[c]
                    .iter()
                    .map(|v| match v {
                        AttrValue::Str(s) => s.as_str(),
                        _ => unreachable!("column tag says string"),
                    })
                    .collect();
                distinct.sort_unstable();
                distinct.dedup();
                put_varint(&mut dict_bytes, distinct.len() as u64);
                for s in &distinct {
                    put_varint(&mut dict_bytes, s.len() as u64);
                    dict_bytes.extend_from_slice(s.as_bytes());
                }
                for v in &raw[c] {
                    let s = match v {
                        AttrValue::Str(s) => s.as_str(),
                        _ => unreachable!(),
                    };
                    let ord =
                        distinct.binary_search(&s).expect("value must be in its own dictionary");
                    val.extend_from_slice(&(ord as u32).to_le_bytes());
                }
            }
            1 => {
                for v in &raw[c] {
                    match v {
                        AttrValue::Int(x) => val.extend_from_slice(&x.to_le_bytes()),
                        _ => unreachable!(),
                    }
                }
            }
            2 => {
                for v in &raw[c] {
                    match v {
                        // bit pattern, not value: -0.0 and NaN payloads must round-trip exactly
                        AttrValue::Float(x) => val.extend_from_slice(&x.to_bits().to_le_bytes()),
                        _ => unreachable!(),
                    }
                }
            }
            3 => {
                for v in &raw[c] {
                    match v {
                        AttrValue::Bool(x) => val.push(u8::from(*x)),
                        _ => unreachable!(),
                    }
                }
            }
            4 => {
                for v in &raw[c] {
                    match v {
                        AttrValue::UInt(x) => val.extend_from_slice(&x.to_le_bytes()),
                        _ => unreachable!(),
                    }
                }
            }
            5 => {
                let mut distinct: Vec<&[u8]> = raw[c]
                    .iter()
                    .map(|v| match v {
                        AttrValue::Bytes(bytes) => bytes.as_slice(),
                        _ => unreachable!("column tag says binary"),
                    })
                    .collect();
                distinct.sort_unstable();
                distinct.dedup();
                put_varint(&mut dict_bytes, distinct.len() as u64);
                for bytes in &distinct {
                    put_varint(&mut dict_bytes, bytes.len() as u64);
                    dict_bytes.extend_from_slice(bytes);
                }
                for v in &raw[c] {
                    let bytes = match v {
                        AttrValue::Bytes(bytes) => bytes.as_slice(),
                        _ => unreachable!(),
                    };
                    let ord = distinct
                        .binary_search(&bytes)
                        .expect("value must be in its own binary dictionary");
                    val.extend_from_slice(&(ord as u32).to_le_bytes());
                }
            }
            6 => {
                for v in &raw[c] {
                    match v {
                        AttrValue::TimestampNs(x) => val.extend_from_slice(&x.to_le_bytes()),
                        _ => unreachable!(),
                    }
                }
            }
            7 => {
                debug_assert!(raw[c].iter().all(|v| matches!(v, AttrValue::Null)));
            }
            t => bail!("unknown attribute type tag {t}"),
        }

        // A DENSE column — one occurrence per row, in row order — has a row-index array of exactly
        // 0..n, which carries no information. Storing it explicitly was 39% of all part bytes on a
        // real corpus. Elide it; everything else stores ascending deltas as varints (a duplicated key
        // on one row is a zero delta), which zstd then crushes.
        let dense =
            rid[c].len() == rows.len() && rid[c].iter().enumerate().all(|(i, &r)| r as usize == i);
        let (kind, rid_bytes) = if dense {
            (RID_DENSE, Vec::new())
        } else {
            let mut b = Vec::with_capacity(rid[c].len());
            let mut prev = 0u32;
            for &r in &rid[c] {
                put_varint(&mut b, (r - prev) as u64);
                prev = r;
            }
            (RID_DELTA, b)
        };

        put_varint(&mut meta, key.len() as u64);
        meta.extend_from_slice(key.as_bytes());
        meta.push(*tag);
        put_varint(&mut meta, rid[c].len() as u64);
        meta.push(kind);

        out_cols.push(BuiltCol { rid: rid_bytes, kind, val, dict: dict_bytes });
    }

    let mut zones: Vec<ZoneAcc> = cols.iter().map(|(_, tag)| ZoneAcc::new(*tag)).collect();
    for (c, values) in raw.iter().enumerate() {
        for v in values {
            zones[c].add(v);
        }
    }

    Ok(BuiltCols { layout, layout_off, meta, zones: encode_zones(&zones), cols: out_cols })
}

/// Streaming min/max for one column — the `zone` section's accumulator, shared by both builders
/// so their bytes cannot drift.
///
/// Strings deliberately carry no zone: their sorted-distinct dictionary already IS one (the first
/// and last entries bound the column) and repeating it would be bytes spent saying so. A float
/// column that ever sees a NaN declares itself unprunable — NaN is unordered, so any range
/// claiming to cover it would prune wrongly, and soundness beats cleverness.
#[derive(Clone)]
pub struct ZoneAcc {
    tag: u8,
    seen: bool,
    poisoned: bool,
    min_i: i64,
    max_i: i64,
    min_u: u64,
    max_u: u64,
    min_f: f64,
    max_f: f64,
    min_b: u8,
    max_b: u8,
}

impl ZoneAcc {
    pub fn new(tag: u8) -> ZoneAcc {
        ZoneAcc {
            tag,
            seen: false,
            poisoned: matches!(tag, 0 | 5 | 7), // dictionaries/null carry no numeric zone
            min_i: i64::MAX,
            max_i: i64::MIN,
            min_u: u64::MAX,
            max_u: u64::MIN,
            min_f: f64::INFINITY,
            max_f: f64::NEG_INFINITY,
            min_b: 1,
            max_b: 0,
        }
    }

    pub fn add(&mut self, v: &AttrValue) {
        self.seen = true;
        match v {
            AttrValue::Str(_) => {}
            AttrValue::Int(x) => {
                self.min_i = self.min_i.min(*x);
                self.max_i = self.max_i.max(*x);
            }
            AttrValue::Float(x) => {
                if x.is_nan() {
                    self.poisoned = true;
                } else {
                    self.min_f = self.min_f.min(*x);
                    self.max_f = self.max_f.max(*x);
                }
            }
            AttrValue::Bool(x) => {
                self.min_b = self.min_b.min(u8::from(*x));
                self.max_b = self.max_b.max(u8::from(*x));
            }
            AttrValue::UInt(x) => {
                self.min_u = self.min_u.min(*x);
                self.max_u = self.max_u.max(*x);
            }
            AttrValue::Bytes(_) | AttrValue::Null => self.poisoned = true,
            AttrValue::TimestampNs(x) => {
                self.min_i = self.min_i.min(*x);
                self.max_i = self.max_i.max(*x);
            }
        }
    }

    /// One entry: a presence byte, then 8-byte min and max when present.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        if !self.seen || self.poisoned {
            out.push(0);
            return;
        }
        out.push(1);
        match self.tag {
            1 | 6 => {
                out.extend_from_slice(&self.min_i.to_le_bytes());
                out.extend_from_slice(&self.max_i.to_le_bytes());
            }
            2 => {
                // bit patterns, like the column itself — a reader compares as floats
                out.extend_from_slice(&self.min_f.to_bits().to_le_bytes());
                out.extend_from_slice(&self.max_f.to_bits().to_le_bytes());
            }
            4 => {
                out.extend_from_slice(&self.min_u.to_le_bytes());
                out.extend_from_slice(&self.max_u.to_le_bytes());
            }
            _ => {
                out.extend_from_slice(&(self.min_b as i64).to_le_bytes());
                out.extend_from_slice(&(self.max_b as i64).to_le_bytes());
            }
        }
    }
}

/// Encode the whole `zone` section from per-column accumulators.
pub fn encode_zones(zones: &[ZoneAcc]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + zones.len() * 17);
    put_varint(&mut out, zones.len() as u64);
    for z in zones {
        z.encode_into(&mut out);
    }
    out
}

/// Column `c`'s zone, as `(min, max)` in the column's own type. `None` when the section is absent
/// (an older part), the ordinal is out of range, the column declared itself unprunable, or the
/// section is malformed — a zone map may only ever WIDEN what a reader scans, never wrongly
/// narrow it, so every doubt resolves to "no pruning".
pub fn read_zone(part: &Part, c: usize) -> Result<Option<(AttrValue, AttrValue)>> {
    if !part.section_present("zone") {
        return Ok(None);
    }
    let meta = read_meta(part)?;
    let Some((_, tag, _, _)) = meta.get(c) else {
        return Ok(None);
    };
    let b = part.section_bytes("zone")?;
    let mut at = 0usize;
    let Ok(n) = get_varint(&b, &mut at) else {
        return Ok(None);
    };
    if c as u64 >= n {
        return Ok(None);
    }
    // walk entries to ordinal c — entries are 1 or 17 bytes, decided by their presence byte
    for _ in 0..c {
        let Some(&flag) = b.get(at) else { return Ok(None) };
        at += if flag == 1 { 17 } else { 1 };
    }
    let Some(&flag) = b.get(at) else { return Ok(None) };
    if flag != 1 {
        return Ok(None);
    }
    if 16 > b.len().saturating_sub(at + 1) {
        return Ok(None);
    }
    let lo = u64::from_le_bytes(b[at + 1..at + 9].try_into().unwrap());
    let hi = u64::from_le_bytes(b[at + 9..at + 17].try_into().unwrap());
    Ok(match tag {
        1 => Some((AttrValue::Int(lo as i64), AttrValue::Int(hi as i64))),
        2 => Some((AttrValue::Float(f64::from_bits(lo)), AttrValue::Float(f64::from_bits(hi)))),
        3 => Some((AttrValue::Bool(lo != 0), AttrValue::Bool(hi != 0))),
        4 => Some((AttrValue::UInt(lo), AttrValue::UInt(hi))),
        6 => Some((AttrValue::TimestampNs(lo as i64), AttrValue::TimestampNs(hi as i64))),
        _ => None,
    })
}

/// `(key, tag, occurrences, rid_kind)` per column, in ordinal order.
///
/// This — like every reader below — parses section bytes that carry NO verified checksum on the
/// read path (that is `verify_sections`' deliberate territory). Corrupt bytes must surface as
/// errors, never as panics or wild allocations: every slice is bounds-checked first, and every
/// count is capped by the bytes that would have to carry it before it sizes an allocation.
pub fn read_meta(part: &Part) -> Result<Vec<(String, u8, usize, u8)>> {
    let m = part.section_bytes("colmeta")?;
    let mut at = 0usize;
    let n = get_varint(&m, &mut at)? as usize;
    let mut out = Vec::with_capacity(n.min(m.len()));
    for _ in 0..n {
        let kl = get_varint(&m, &mut at)? as usize;
        // `kl >= len - at`, never `at + kl + 1 > len`: the sum can overflow, the subtraction cannot.
        if kl >= m.len() - at {
            bail!("colmeta entry runs past the section");
        }
        let key = String::from_utf8(m[at..at + kl].to_vec())?;
        at += kl;
        let tag = m[at];
        at += 1;
        let occ = get_varint(&m, &mut at)? as usize;
        if at >= m.len() {
            bail!("colmeta entry is truncated before its rid kind");
        }
        let kind = m[at];
        at += 1;
        out.push((key, tag, occ, kind));
    }
    Ok(out)
}

/// A string column's sorted distinct dictionary; empty for non-string columns.
pub fn read_dict(part: &Part, c: usize) -> Result<std::sync::Arc<Vec<String>>> {
    if let Some(v) = part.dict_cached(c) {
        return Ok(v);
    }
    let name = format!("col.dict.{c}");
    if !part.section_present(&name) {
        return Ok(part.dict_put(c, Vec::new()));
    }
    let b = part.section_bytes(&name)?;
    let mut at = 0usize;
    let n = get_varint(&b, &mut at)? as usize;
    let mut out = Vec::with_capacity(n.min(b.len()));
    for _ in 0..n {
        let l = get_varint(&b, &mut at)? as usize;
        if l > b.len() - at {
            bail!("column dictionary entry runs past the section");
        }
        out.push(String::from_utf8(b[at..at + l].to_vec())?);
        at += l;
    }
    Ok(part.dict_put(c, out))
}

/// A binary column's sorted distinct dictionary; empty for non-binary columns.
pub fn read_binary_dict(part: &Part, c: usize) -> Result<std::sync::Arc<Vec<Vec<u8>>>> {
    if let Some(v) = part.binary_dict_cached(c) {
        return Ok(v);
    }
    let name = format!("col.dict.{c}");
    if !part.section_present(&name) {
        return Ok(part.binary_dict_put(c, Vec::new()));
    }
    let b = part.section_bytes(&name)?;
    let mut at = 0usize;
    let n = get_varint(&b, &mut at)? as usize;
    let mut out = Vec::with_capacity(n.min(b.len()));
    for _ in 0..n {
        let l = get_varint(&b, &mut at)? as usize;
        if l > b.len() - at {
            bail!("binary column dictionary entry runs past the section");
        }
        out.push(b[at..at + l].to_vec());
        at += l;
    }
    Ok(part.binary_dict_put(c, out))
}

/// A column's row indices, decoded. Dense columns are synthesised rather than read.
pub fn rids(part: &Part, c: usize, occ: usize, kind: u8) -> Result<std::sync::Arc<Vec<u32>>> {
    if let Some(v) = part.rid_cached(c) {
        return Ok(v);
    }
    // `occ` comes from colmeta and is untrusted. No column can have more occurrences than it has
    // rows carrying them... times the attrs a row can hold — but a DENSE column is one occurrence
    // per row exactly, and a delta column cannot outnumber the bytes that encode it. Both bounds
    // are checked before they size anything.
    let v = match kind {
        RID_DENSE => {
            if occ > part.len() {
                bail!("dense column claims {occ} occurrences in a part of {} rows", part.len());
            }
            (0..occ as u32).collect::<Vec<u32>>()
        }
        RID_DELTA => {
            let b = part.section_bytes(&format!("col.rid.{c}"))?;
            let mut out = Vec::with_capacity(occ.min(b.len()));
            let (mut at, mut cur) = (0usize, 0u32);
            for _ in 0..occ {
                let d = get_varint(&b, &mut at)?;
                cur = u32::try_from(d)
                    .ok()
                    .and_then(|d| cur.checked_add(d))
                    .ok_or_else(|| anyhow::anyhow!("rid delta overflows the u32 row space"))?;
                out.push(cur);
            }
            out
        }
        k => bail!("unknown rid encoding {k}"),
    };
    Ok(part.rid_cache_put(c, v))
}

fn value_at(
    tag: u8,
    val: &[u8],
    k: usize,
    dict: &[String],
    binary_dict: &[Vec<u8>],
) -> Result<AttrValue> {
    let w = width(tag);
    let at = k * w;
    if at + w > val.len() {
        bail!("value index {k} out of range");
    }
    Ok(match tag {
        0 => {
            let ord = u32::from_le_bytes(val[at..at + 4].try_into().unwrap()) as usize;
            AttrValue::Str(
                dict.get(ord)
                    .ok_or_else(|| anyhow::anyhow!("dictionary ordinal {ord} out of range"))?
                    .clone(),
            )
        }
        1 => AttrValue::Int(i64::from_le_bytes(val[at..at + 8].try_into().unwrap())),
        2 => AttrValue::Float(f64::from_bits(u64::from_le_bytes(
            val[at..at + 8].try_into().unwrap(),
        ))),
        3 => AttrValue::Bool(val[at] != 0),
        4 => AttrValue::UInt(u64::from_le_bytes(val[at..at + 8].try_into().unwrap())),
        5 => {
            let ord = u32::from_le_bytes(val[at..at + 4].try_into().unwrap()) as usize;
            AttrValue::Bytes(
                binary_dict
                    .get(ord)
                    .ok_or_else(|| anyhow::anyhow!("binary dictionary ordinal {ord} out of range"))?
                    .clone(),
            )
        }
        6 => AttrValue::TimestampNs(i64::from_le_bytes(val[at..at + 8].try_into().unwrap())),
        7 => AttrValue::Null,
        t => bail!("unknown attribute type tag {t}"),
    })
}

/// Row `r`'s attributes in their exact original order, duplicates included.
pub fn read_row(part: &Part, r: usize) -> Result<Vec<(String, AttrValue)>> {
    read_row_filtered(part, r, None)
}

/// Selected attributes at row `r`, preserving their relative order and duplicate occurrences.
/// Layout and column metadata are shared structural sections; value/rid/dictionary sections are
/// opened only for columns whose name appears in `names`.
pub fn read_row_selected(
    part: &Part,
    r: usize,
    names: &HashSet<&str>,
) -> Result<Vec<(String, AttrValue)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    read_row_filtered(part, r, Some(names))
}

/// Selected attributes for several rows, in the caller's row order.
///
/// Unlike repeated [`read_row_selected`] calls, this parses the shared layout offsets and column
/// directory once, and opens each selected rid/value/dictionary section once for the whole gather.
/// Each row still follows its layout, so duplicate fields and their original interleaving survive.
pub fn read_rows_selected(
    part: &Part,
    rows: &[usize],
    names: &HashSet<&str>,
) -> Result<Vec<Vec<(String, AttrValue)>>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    if names.is_empty() || !part.section_present("colmeta") {
        return Ok(vec![Vec::new(); rows.len()]);
    }

    let layout = part.section_bytes("layout")?;
    let offs = part.nums("layout.off", 8)?;
    let meta = read_meta(part)?;
    let selected: Vec<bool> =
        meta.iter().map(|(key, _, _, _)| names.contains(key.as_str())).collect();

    // Decode the selected column ordinals per row first. This is both the row's output ordering and
    // the exact multiplicity against which the sparse rid column is checked below.
    let mut row_layouts = Vec::with_capacity(rows.len());
    let mut used_columns = std::collections::BTreeSet::new();
    for &r in rows {
        if r >= offs.len().saturating_sub(1) {
            bail!("row {r} out of range for the layout");
        }
        let mut at = usize::try_from(offs[r])
            .map_err(|_| anyhow::anyhow!("row {r} layout offset exceeds this platform"))?;
        let n = get_varint(&layout, &mut at)? as usize;
        let mut columns = Vec::with_capacity(n.min(layout.len()));
        for _ in 0..n {
            let c = get_varint(&layout, &mut at)? as usize;
            if c >= meta.len() {
                bail!("layout names column {c} which does not exist");
            }
            if selected[c] {
                columns.push(c);
                used_columns.insert(c);
            }
        }
        row_layouts.push(columns);
    }

    struct Decoder {
        key: String,
        tag: u8,
        rids: std::sync::Arc<Vec<u32>>,
        values: std::sync::Arc<Vec<u8>>,
        dict: std::sync::Arc<Vec<String>>,
        binary_dict: std::sync::Arc<Vec<Vec<u8>>>,
    }

    let mut decoders = std::collections::HashMap::with_capacity(used_columns.len());
    for c in used_columns {
        let (key, tag, occ, kind) = &meta[c];
        decoders.insert(
            c,
            Decoder {
                key: key.clone(),
                tag: *tag,
                rids: rids(part, c, *occ, *kind)?,
                values: part.section_bytes(&format!("col.val.{c}"))?,
                dict: if *tag == 0 { read_dict(part, c)? } else { Default::default() },
                binary_dict: if *tag == 5 {
                    read_binary_dict(part, c)?
                } else {
                    Default::default()
                },
            },
        );
    }

    let mut out = Vec::with_capacity(rows.len());
    for (&r, columns) in rows.iter().zip(row_layouts) {
        let mut row = Vec::with_capacity(columns.len());
        let mut cursors = std::collections::HashMap::<usize, (usize, usize)>::new();
        for c in columns {
            let decoder = &decoders[&c];
            let (first, used) = match cursors.get(&c).copied() {
                Some(cursor) => cursor,
                None => {
                    let first = decoder.rids.partition_point(|&candidate| (candidate as usize) < r);
                    if first >= decoder.rids.len() || decoder.rids[first] as usize != r {
                        bail!("row {r} names column {c} but has no occurrence in it");
                    }
                    (first, 0)
                }
            };
            let occurrence = first
                .checked_add(used)
                .ok_or_else(|| anyhow::anyhow!("row {r} column {c} occurrence overflows"))?;
            if decoder.rids.get(occurrence).copied().map(|row| row as usize) != Some(r) {
                bail!("row {r} names more occurrences of column {c} than its row ids contain");
            }
            row.push((
                decoder.key.clone(),
                value_at(
                    decoder.tag,
                    &decoder.values,
                    occurrence,
                    &decoder.dict,
                    &decoder.binary_dict,
                )?,
            ));
            cursors.insert(c, (first, used + 1));
        }
        out.push(row);
    }
    Ok(out)
}

fn read_row_filtered(
    part: &Part,
    r: usize,
    names: Option<&HashSet<&str>>,
) -> Result<Vec<(String, AttrValue)>> {
    if !part.section_present("colmeta") {
        return Ok(Vec::new());
    }
    let layout = part.section_bytes("layout")?;
    let offs = part.nums("layout.off", 8)?;
    if r + 1 >= offs.len() {
        bail!("row {r} out of range for the layout");
    }
    let mut at = offs[r] as usize;
    let n = get_varint(&layout, &mut at)? as usize;
    if n == 0 {
        return Ok(Vec::new());
    }
    let meta = read_meta(part)?;

    let mut out = Vec::with_capacity(n.min(layout.len()));
    // per column: the index of this row's first occurrence, then how many we have consumed
    let mut cursor: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();
    for _ in 0..n {
        let c = get_varint(&layout, &mut at)? as usize;
        let (key, tag, occ, kind) = meta
            .get(c)
            .ok_or_else(|| anyhow::anyhow!("layout names column {c} which does not exist"))?;
        if names.is_some_and(|names| !names.contains(key.as_str())) {
            continue;
        }
        let entry = match cursor.get(&c) {
            Some(e) => *e,
            None => {
                let rids = rids(part, c, *occ, *kind)?;
                // first occurrence of this row in the column — rids are ascending
                let first = rids.partition_point(|&x| (x as usize) < r);
                if first >= rids.len() || rids[first] as usize != r {
                    bail!("row {r} names column {c} but has no occurrence in it");
                }
                let e = (first, 0usize);
                cursor.insert(c, e);
                e
            }
        };
        let (first, used) = entry;
        let dict = if *tag == 0 { read_dict(part, c)? } else { Default::default() };
        let binary_dict = if *tag == 5 { read_binary_dict(part, c)? } else { Default::default() };
        let val = part.section_bytes(&format!("col.val.{c}"))?;
        out.push((key.clone(), value_at(*tag, &val, first + used, &dict, &binary_dict)?));
        cursor.insert(c, (first, used + 1));
    }
    Ok(out)
}
