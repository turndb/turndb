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
use anyhow::{bail, Context, Result};
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

/// Prove the row-layout stream is the one exact framing described by its offsets and column
/// metadata. This is an open-time structural check: point reads may then select one row without
/// leaving malformed neighboring rows latent.
pub(crate) fn validate_layout(part: &Part, meta: &[(String, u8, usize, u8)]) -> Result<()> {
    let layout = part.section_bytes("layout")?;
    let offsets = part.nums("layout.off", 8)?;
    let expected = part
        .len()
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("attribute layout offset count overflows"))?;
    if offsets.len() != expected {
        bail!("attribute layout has {} offsets, expected {expected}", offsets.len());
    }
    if offsets.first().copied() != Some(0) {
        bail!("attribute layout must begin at byte zero");
    }
    let column_rows = meta
        .iter()
        .enumerate()
        .map(|(column, (_, _, occurrences, kind))| rids(part, column, *occurrences, *kind))
        .collect::<Result<Vec<_>>>()?;
    let mut occurrences = vec![0usize; meta.len()];
    for row in 0..part.len() {
        let start = usize::try_from(offsets[row])
            .with_context(|| format!("row {row} layout offset exceeds this platform"))?;
        let end = usize::try_from(offsets[row + 1])
            .with_context(|| format!("row {row} layout end exceeds this platform"))?;
        if start > end || end > layout.len() {
            bail!("row {row} layout offsets [{start}, {end}) are outside the layout stream");
        }
        let mut at = start;
        let count = usize::try_from(get_varint(&layout[..end], &mut at)?)
            .context("attribute count exceeds this platform's address space")?;
        for _ in 0..count {
            let column = usize::try_from(get_varint(&layout[..end], &mut at)?)
                .context("attribute column ordinal exceeds this platform's address space")?;
            let seen = occurrences
                .get_mut(column)
                .ok_or_else(|| anyhow::anyhow!("row {row} names absent column {column}"))?;
            if column_rows[column].get(*seen).copied().map(|rid| rid as usize) != Some(row) {
                bail!(
                    "row {row} layout occurrence {} for column {column} disagrees with its row ids",
                    *seen
                );
            }
            *seen = seen
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("attribute occurrence count overflows"))?;
        }
        if at != end {
            bail!("row {row} layout has {} trailing bytes", end - at);
        }
    }
    let final_offset = usize::try_from(*offsets.last().expect("one offset is required"))
        .context("attribute layout final offset exceeds this platform")?;
    if final_offset != layout.len() {
        bail!(
            "attribute layout final offset is {final_offset}, but stream has {} bytes",
            layout.len()
        );
    }
    for (column, (actual, (_, _, declared, _))) in occurrences.iter().zip(meta.iter()).enumerate() {
        if actual != declared {
            bail!("attribute column {column} has {actual} layout occurrences, declared {declared}");
        }
    }
    Ok(())
}

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
    u32::try_from(rows.len()).context("attribute row count exceeds the u32 row-id domain")?;
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
                u32::try_from(distinct.len())
                    .context("string dictionary exceeds the u32 ordinal domain")?;
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
                u32::try_from(distinct.len())
                    .context("binary dictionary exceeds the u32 ordinal domain")?;
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

    pub fn encoded_len(&self) -> usize {
        if !self.seen || self.poisoned {
            1
        } else {
            17
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

pub fn try_encode_zones(zones: &[ZoneAcc]) -> Result<Vec<u8>> {
    let mut count = zones.len() as u64;
    let mut count_bytes = 1usize;
    while count >= 0x80 {
        count >>= 7;
        count_bytes += 1;
    }
    let capacity = zones.iter().try_fold(count_bytes, |total, zone| {
        total.checked_add(zone.encoded_len()).context("zone section length overflows")
    })?;
    let mut out = Vec::new();
    out.try_reserve_exact(capacity).map_err(|error| anyhow::anyhow!(error))?;
    put_varint(&mut out, zones.len() as u64);
    for zone in zones {
        zone.encode_into(&mut out);
    }
    Ok(out)
}

fn parse_zones(
    bytes: &[u8],
    meta: &[(String, u8, usize, u8)],
) -> Result<Vec<Option<(AttrValue, AttrValue)>>> {
    let mut at = 0usize;
    let count = usize::try_from(get_varint(bytes, &mut at)?)
        .context("zone entry count exceeds this platform's address space")?;
    if count != meta.len() {
        bail!("zone carries {count} entries for {} attribute columns", meta.len());
    }
    let mut zones = Vec::with_capacity(count);
    for (column, (_, tag, _, _)) in meta.iter().enumerate() {
        let flag =
            *bytes.get(at).ok_or_else(|| anyhow::anyhow!("zone entry {column} is missing"))?;
        at += 1;
        match flag {
            0 => zones.push(None),
            1 => {
                if !matches!(*tag, 1 | 2 | 3 | 4 | 6) {
                    bail!("zone entry {column} declares bounds for unbounded type tag {tag}");
                }
                let end = at
                    .checked_add(16)
                    .ok_or_else(|| anyhow::anyhow!("zone entry {column} end overflows"))?;
                if end > bytes.len() {
                    bail!("zone entry {column} is truncated");
                }
                let lo = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
                let hi = u64::from_le_bytes(bytes[at + 8..end].try_into().unwrap());
                at = end;
                let bounds = match *tag {
                    1 => {
                        let (lo, hi) = (lo as i64, hi as i64);
                        if lo > hi {
                            bail!("zone entry {column} has inverted signed bounds");
                        }
                        (AttrValue::Int(lo), AttrValue::Int(hi))
                    }
                    2 => {
                        let (lo, hi) = (f64::from_bits(lo), f64::from_bits(hi));
                        if lo.is_nan() || hi.is_nan() || lo > hi {
                            bail!("zone entry {column} has unusable float bounds");
                        }
                        (AttrValue::Float(lo), AttrValue::Float(hi))
                    }
                    3 => {
                        if lo > 1 || hi > 1 || lo > hi {
                            bail!("zone entry {column} has non-boolean or inverted bounds");
                        }
                        (AttrValue::Bool(lo == 1), AttrValue::Bool(hi == 1))
                    }
                    4 => {
                        if lo > hi {
                            bail!("zone entry {column} has inverted unsigned bounds");
                        }
                        (AttrValue::UInt(lo), AttrValue::UInt(hi))
                    }
                    6 => {
                        let (lo, hi) = (lo as i64, hi as i64);
                        if lo > hi {
                            bail!("zone entry {column} has inverted timestamp bounds");
                        }
                        (AttrValue::TimestampNs(lo), AttrValue::TimestampNs(hi))
                    }
                    _ => unreachable!("bounded tags checked above"),
                };
                zones.push(Some(bounds));
            }
            other => bail!("zone entry {column} has unknown presence flag {other}"),
        }
    }
    if at != bytes.len() {
        bail!("zone has {} trailing bytes", bytes.len() - at);
    }
    Ok(zones)
}

/// Column `c`'s zone, as `(min, max)` in the column's own type. `None` when the section is absent
/// (the advisory section is absent), the ordinal is out of range, the column declared itself unprunable, or the
/// section is malformed — a zone map may only ever WIDEN what a reader scans, never wrongly
/// narrow it, so every doubt resolves to "no pruning".
pub fn read_zone(part: &Part, c: usize) -> Result<Option<(AttrValue, AttrValue)>> {
    if !part.section_present("zone") {
        return Ok(None);
    }
    let meta = read_meta(part)?;
    if c >= meta.len() {
        return Ok(None);
    }
    let Some(bytes) = part.verified_advisory_section("zone")? else { return Ok(None) };
    let Some((minimum, maximum)) =
        parse_zones(&bytes, &meta).ok().and_then(|zones| zones.get(c).cloned().flatten())
    else {
        return Ok(None);
    };

    // A checksum proves the advisory bytes were read faithfully, not that their claim is true.
    // Trust a bound for negative pruning only after proving that it contains every authoritative
    // column value. This deliberately makes a forged-but-checksummed broad zone harmless and a
    // narrow one unusable; advisory metadata may improve work, never change an answer.
    let (_, tag, occurrences, _) = meta[c];
    let values = part.section_bytes(&format!("col.val.{c}"))?;
    for occurrence in 0..occurrences {
        let value = value_at(tag, &values, occurrence, &[], &[])?;
        let contains = match (&minimum, &maximum, &value) {
            (AttrValue::Int(lo), AttrValue::Int(hi), AttrValue::Int(value)) => {
                lo <= value && value <= hi
            }
            (AttrValue::Float(lo), AttrValue::Float(hi), AttrValue::Float(value)) => {
                !value.is_nan() && lo <= value && value <= hi
            }
            (AttrValue::Bool(lo), AttrValue::Bool(hi), AttrValue::Bool(value)) => {
                lo <= value && value <= hi
            }
            (AttrValue::UInt(lo), AttrValue::UInt(hi), AttrValue::UInt(value)) => {
                lo <= value && value <= hi
            }
            (
                AttrValue::TimestampNs(lo),
                AttrValue::TimestampNs(hi),
                AttrValue::TimestampNs(value),
            ) => lo <= value && value <= hi,
            _ => false,
        };
        if !contains {
            return Ok(None);
        }
    }
    Ok(Some((minimum, maximum)))
}

/// `(key, tag, occurrences, rid_kind)` per column, in ordinal order.
///
/// This — like every reader below — receives section bytes only after `Part::sect` verifies their
/// stored checksum. Structural corruption must still surface as errors, never as panics or wild
/// allocations: every slice is bounds-checked first, and every
/// count is capped by the bytes that would have to carry it before it sizes an allocation.
pub fn read_meta(part: &Part) -> Result<Vec<(String, u8, usize, u8)>> {
    let m = part.section_bytes("colmeta")?;
    let mut at = 0usize;
    let n = usize::try_from(get_varint(&m, &mut at)?)
        .context("attribute-column count exceeds this platform's address space")?;
    let mut out = Vec::with_capacity(n.min(m.len()));
    let mut previous: Option<(Vec<u8>, u8)> = None;
    for column in 0..n {
        let kl = usize::try_from(get_varint(&m, &mut at)?)
            .context("attribute name length exceeds this platform's address space")?;
        // `kl >= len - at`, never `at + kl + 1 > len`: the sum can overflow, the subtraction cannot.
        if kl == 0 || kl >= m.len() - at {
            bail!("colmeta entry runs past the section");
        }
        let key_bytes = m[at..at + kl].to_vec();
        let key = String::from_utf8(key_bytes.clone())?;
        at += kl;
        let tag = m[at];
        at += 1;
        if tag > 7 {
            bail!("attribute column {column} carries unknown type tag {tag}");
        }
        if previous.as_ref().is_some_and(|prior| prior >= &(key_bytes.clone(), tag)) {
            bail!("attribute columns are duplicated or out of canonical order");
        }
        previous = Some((key_bytes, tag));
        let occ = usize::try_from(get_varint(&m, &mut at)?)
            .context("attribute occurrence count exceeds this platform's address space")?;
        if occ == 0 {
            bail!("attribute column {column} has no occurrences");
        }
        if at >= m.len() {
            bail!("colmeta entry is truncated before its rid kind");
        }
        let kind = m[at];
        at += 1;
        out.push((key, tag, occ, kind));
    }
    if at != m.len() {
        bail!("colmeta has {} trailing bytes", m.len() - at);
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
    let n = usize::try_from(get_varint(&b, &mut at)?)
        .context("string dictionary count exceeds this platform's address space")?;
    let mut out = Vec::with_capacity(n.min(b.len()));
    for entry in 0..n {
        let l = usize::try_from(get_varint(&b, &mut at)?)
            .context("string dictionary entry length exceeds this platform")?;
        if l > b.len() - at {
            bail!("column dictionary entry runs past the section");
        }
        let value = String::from_utf8(b[at..at + l].to_vec())?;
        if out.last().is_some_and(|previous| previous >= &value) {
            bail!("string dictionary entry {entry} is duplicated or out of order");
        }
        out.push(value);
        at += l;
    }
    if at != b.len() {
        bail!("string dictionary has {} trailing bytes", b.len() - at);
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
    let n = usize::try_from(get_varint(&b, &mut at)?)
        .context("binary dictionary count exceeds this platform's address space")?;
    let mut out = Vec::with_capacity(n.min(b.len()));
    for entry in 0..n {
        let l = usize::try_from(get_varint(&b, &mut at)?)
            .context("binary dictionary entry length exceeds this platform")?;
        if l > b.len() - at {
            bail!("binary column dictionary entry runs past the section");
        }
        let value = b[at..at + l].to_vec();
        if out.last().is_some_and(|previous| previous >= &value) {
            bail!("binary dictionary entry {entry} is duplicated or out of order");
        }
        out.push(value);
        at += l;
    }
    if at != b.len() {
        bail!("binary dictionary has {} trailing bytes", b.len() - at);
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
                if cur as usize >= part.len() {
                    bail!("attribute row id {cur} is outside a part of {} rows", part.len());
                }
                out.push(cur);
            }
            if at != b.len() {
                bail!("attribute row ids have {} trailing bytes", b.len() - at);
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
        3 => match val[at] {
            0 => AttrValue::Bool(false),
            1 => AttrValue::Bool(true),
            other => bail!("invalid boolean column byte {other}"),
        },
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
        let end = usize::try_from(offs[r + 1])
            .map_err(|_| anyhow::anyhow!("row {r} layout end exceeds this platform"))?;
        let n = usize::try_from(get_varint(&layout[..end], &mut at)?)
            .context("attribute count exceeds this platform's address space")?;
        let mut columns = Vec::with_capacity(n.min(layout.len()));
        for _ in 0..n {
            let c = usize::try_from(get_varint(&layout[..end], &mut at)?)
                .context("attribute column ordinal exceeds this platform's address space")?;
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
    if r >= offs.len().saturating_sub(1) {
        bail!("row {r} out of range for the layout");
    }
    let mut at = usize::try_from(offs[r])
        .map_err(|_| anyhow::anyhow!("row {r} layout offset exceeds this platform"))?;
    let end = usize::try_from(offs[r + 1])
        .map_err(|_| anyhow::anyhow!("row {r} layout end exceeds this platform"))?;
    let n = usize::try_from(get_varint(&layout[..end], &mut at)?)
        .context("attribute count exceeds this platform's address space")?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let meta = read_meta(part)?;

    let mut out = Vec::with_capacity(n.min(layout.len()));
    // per column: the index of this row's first occurrence, then how many we have consumed
    let mut cursor: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();
    for _ in 0..n {
        let c = usize::try_from(get_varint(&layout[..end], &mut at)?)
            .context("attribute column ordinal exceeds this platform's address space")?;
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
        let occurrence = first
            .checked_add(used)
            .ok_or_else(|| anyhow::anyhow!("row {r} column {c} occurrence overflows"))?;
        let column_rows = rids(part, c, *occ, *kind)?;
        if column_rows.get(occurrence).copied().map(|row| row as usize) != Some(r) {
            bail!("row {r} names more occurrences of column {c} than its row ids contain");
        }
        let val = part.section_bytes(&format!("col.val.{c}"))?;
        out.push((key.clone(), value_at(*tag, &val, occurrence, &dict, &binary_dict)?));
        cursor.insert(c, (first, used + 1));
    }
    Ok(out)
}
