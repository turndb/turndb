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

/// Bytes per value, by type tag. Fixed width keeps `val` directly indexable; zstd removes the slack.
pub fn width(tag: u8) -> usize {
    match tag {
        0 => 4, // dictionary ordinal
        1 => 8, // i64
        2 => 8, // f64 bits — preserves NaN payloads and -0.0 exactly
        3 => 1, // bool
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
                    let ord = distinct.binary_search(&s).expect("value must be in its own dictionary");
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
            t => bail!("unknown attribute type tag {t}"),
        }

        // A DENSE column — one occurrence per row, in row order — has a row-index array of exactly
        // 0..n, which carries no information. Storing it explicitly was 39% of all part bytes on a
        // real corpus. Elide it; everything else stores ascending deltas as varints (a duplicated key
        // on one row is a zero delta), which zstd then crushes.
        let dense = rid[c].len() == rows.len() && rid[c].iter().enumerate().all(|(i, &r)| r as usize == i);
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

    Ok(BuiltCols { layout, layout_off, meta, cols: out_cols })
}

/// `(key, tag, occurrences, rid_kind)` per column, in ordinal order.
pub fn read_meta(part: &Part) -> Result<Vec<(String, u8, usize, u8)>> {
    let m = part.section_bytes("colmeta")?;
    let mut at = 0usize;
    let n = get_varint(&m, &mut at)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let kl = get_varint(&m, &mut at)? as usize;
        let key = String::from_utf8(m[at..at + kl].to_vec())?;
        at += kl;
        let tag = m[at];
        at += 1;
        let occ = get_varint(&m, &mut at)? as usize;
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
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let l = get_varint(&b, &mut at)? as usize;
        out.push(String::from_utf8(b[at..at + l].to_vec())?);
        at += l;
    }
    Ok(part.dict_put(c, out))
}

/// A column's row indices, decoded. Dense columns are synthesised rather than read.
pub fn rids(part: &Part, c: usize, occ: usize, kind: u8) -> Result<std::sync::Arc<Vec<u32>>> {
    if let Some(v) = part.rid_cached(c) {
        return Ok(v);
    }
    let v = match kind {
        RID_DENSE => (0..occ as u32).collect::<Vec<u32>>(),
        RID_DELTA => {
            let b = part.section_bytes(&format!("col.rid.{c}"))?;
            let mut out = Vec::with_capacity(occ);
            let (mut at, mut cur) = (0usize, 0u32);
            for _ in 0..occ {
                cur += get_varint(&b, &mut at)? as u32;
                out.push(cur);
            }
            out
        }
        k => bail!("unknown rid encoding {k}"),
    };
    Ok(part.rid_cache_put(c, v))
}

fn value_at(tag: u8, val: &[u8], k: usize, dict: &[String]) -> Result<AttrValue> {
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
        2 => AttrValue::Float(f64::from_bits(u64::from_le_bytes(val[at..at + 8].try_into().unwrap()))),
        3 => AttrValue::Bool(val[at] != 0),
        t => bail!("unknown attribute type tag {t}"),
    })
}

/// Row `r`'s attributes in their exact original order, duplicates included.
pub fn read_row(part: &Part, r: usize) -> Result<Vec<(String, AttrValue)>> {
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

    let mut out = Vec::with_capacity(n);
    // per column: the index of this row's first occurrence, then how many we have consumed
    let mut cursor: std::collections::HashMap<usize, (usize, usize)> = std::collections::HashMap::new();
    for _ in 0..n {
        let c = get_varint(&layout, &mut at)? as usize;
        let (key, tag, occ, kind) = meta
            .get(c)
            .ok_or_else(|| anyhow::anyhow!("layout names column {c} which does not exist"))?;
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
        let dict = read_dict(part, c)?;
        let val = part.section_bytes(&format!("col.val.{c}"))?;
        out.push((key.clone(), value_at(*tag, &val, first + used, &dict)?));
        cursor.insert(c, (first, used + 1));
    }
    Ok(out)
}
