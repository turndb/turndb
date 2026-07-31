//! The merge operator: N parts in, one part out. **The fold is never touched.**
//!
//! That asymmetry is the point. Content lives in the fold, addressed by logical block id, and a part
//! holds only references and columns — so consolidating parts rewrites references and columns and not
//! a single content byte. In a conventional LSM, compaction rewrites the data.
//!
//! # Contiguity is a correctness gate, not a nicety
//!
//! Version resolution across parts compares sequence numbers. If parts with sequences 1 and 3 were
//! merged while 2 was left out, the output would claim the range 1..=3 while missing whatever 2 said
//! about a shared id — silently resurrecting a superseded record. The input set must therefore be a
//! **contiguous slice** of the sequence-ordered live list, and that is checked, not assumed.
//!
//! # Output sequence
//!
//! `seq_lo = min(inputs)`, `seq_hi = max(inputs)`. Allocating a fresh sequence would sort the output
//! above parts that were not merged and invert their resolution; taking the minimum would lose
//! recency against later parts.
//!
//! # Determinism
//!
//! Inputs are canonicalised to ascending sequence before anything else, so the output is a pure
//! function of the input *set* rather than of the caller's argument order. Everything downstream —
//! the id merge, the dictionary unions, the column ordinals — is already order-independent.

use super::{Part, PartMeta};
use crate::fold::Loc;
use crate::types::PieceHash;
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// What a merge did, for the caller's bookkeeping and for tests.
#[derive(Clone, Copy, Debug)]
pub struct MergeStats {
    pub inputs: usize,
    pub records_in: usize,
    pub records_out: usize,
    /// Records dropped because a later part carried a newer version of the same id.
    pub superseded: usize,
    /// Deletions carried forward, because something outside this merge could still hold an older
    /// version of the id.
    pub tombstones_kept: usize,
    /// Deletions finally discarded — only possible when the merge covered every live part.
    pub tombstones_dropped: usize,
    /// Always zero. Asserted, because "merge never touches the fold" is the load-bearing claim.
    pub fold_bytes_touched: u64,
}

/// Merge a contiguous run of parts into `out`.
pub fn merge(out: &Path, inputs: &[Arc<Part>], level: i32) -> Result<(PartMeta, MergeStats)> {
    merge_opts(out, inputs, level, false)
}

/// One step of the k-way walk: the winning `(part, row)` for the smallest id, plus how many other
/// parts held a superseded version of it. Parts are id-sorted and hold one version per id, so a
/// simple positional walk suffices; later parts win ties.
/// One id's worth of a k-way merge step: `(id, winning stream, its row, streams that carried it)`.
type MergeGroup = (Vec<u8>, usize, usize, usize);

struct KWay<'a> {
    cursors: Vec<crate::part::idcol::IdCursor<'a>>,
    current: Vec<Option<Vec<u8>>>,
    row: Vec<usize>,
}

impl<'a> KWay<'a> {
    fn new(streams: &'a [Arc<Vec<u8>>], lens: &[usize]) -> Result<KWay<'a>> {
        let mut cursors: Vec<_> = streams
            .iter()
            .zip(lens)
            .map(|(s, &n)| crate::part::idcol::IdCursor::new(s, n))
            .collect();
        let mut current = Vec::with_capacity(cursors.len());
        for c in &mut cursors {
            current.push(c.next_id()?.map(|b| b.to_vec()));
        }
        Ok(KWay { cursors, current, row: vec![0; streams.len()] })
    }

    fn next_group(&mut self) -> Result<Option<MergeGroup>> {
        let min = match self.current.iter().flatten().min() {
            Some(m) => m.clone(),
            None => return Ok(None),
        };
        let mut winner = (0usize, 0usize);
        let mut holders = 0usize;
        for i in 0..self.cursors.len() {
            if self.current[i].as_deref() == Some(min.as_slice()) {
                winner = (i, self.row[i]);
                holders += 1;
                self.row[i] += 1;
                self.current[i] = self.cursors[i].next_id()?.map(|b| b.to_vec());
            }
        }
        Ok(Some((min, winner.0, winner.1, holders - 1)))
    }
}

/// [`merge`], with the option to DROP tombstones rather than carry them forward.
///
/// A tombstone exists to shadow older versions of its id. It can only be discarded when there is
/// nothing left for it to shadow — that is, when the merge covers every live part, so no part outside
/// it can still hold an older version of that id. Dropping one otherwise RESURRECTS deleted data,
/// which is the worst outcome available here and the reason this is a caller's decision rather than an
/// inference: only the store knows whether the run it passed is the whole live list.
///
/// # Two streaming passes, not one materialized one
///
/// Memory here is bounded by the piece dictionary and the column universe, never by the record
/// count. Pass A walks the id columns to find each id's winner and gathers what the streaming
/// builder must know up front — the exact `(key, type)` universe of surviving rows and each string
/// column's distinct values. Pass B walks again and feeds rows straight into the builder, which
/// spools its sections to disk. (One pass cannot work: `layout` references column ordinals and
/// string values reference dictionary ordinals, and both orderings are only known once every
/// surviving row has been seen.)
pub fn merge_opts(
    out: &Path,
    inputs: &[Arc<Part>],
    level: i32,
    drop_tombstones: bool,
) -> Result<(PartMeta, MergeStats)> {
    if inputs.is_empty() {
        bail!("merge needs at least one input part");
    }
    // Canonicalise: the output must not depend on argument order.
    let mut parts: Vec<&Arc<Part>> = inputs.iter().collect();
    parts.sort_by_key(|p| p.meta().seq_hi);

    // Sequence ranges must be disjoint; overlapping ranges mean the caller built the live list wrong.
    for w in parts.windows(2) {
        if w[0].meta().seq_hi >= w[1].meta().seq_lo {
            bail!(
                "merge inputs overlap in sequence: [{},{}] and [{},{}]",
                w[0].meta().seq_lo,
                w[0].meta().seq_hi,
                w[1].meta().seq_lo,
                w[1].meta().seq_hi
            );
        }
    }
    let seq_lo = parts.first().unwrap().meta().seq_lo;
    let seq_hi = parts.last().unwrap().meta().seq_hi;

    // Every piece any input references, by identity. The union is over dictionaries that are already
    // sorted in fold order and whose Locs are globally stable — block ids do not move — so this is a
    // gather, never a re-derivation.
    let mut locs: HashMap<PieceHash, Loc> = HashMap::new();
    for p in &parts {
        for i in 0..p.piece_count()? {
            let (loc, hash) = p.piece(i)?;
            locs.entry(hash).or_insert(loc);
        }
    }

    let streams: Vec<Arc<Vec<u8>>> =
        parts.iter().map(|p| p.section_bytes("ids")).collect::<Result<_>>()?;
    let lens: Vec<usize> = parts.iter().map(|p| p.len()).collect();
    let records_in: usize = lens.iter().sum();

    // ---- pass A: winners, counts, and the exact column universe of SURVIVING rows ----
    let mut columns: std::collections::BTreeMap<(String, u8), std::collections::BTreeSet<String>> =
        Default::default();
    let mut records_out = 0usize;
    let mut superseded = 0usize;
    let mut tombs_kept = 0usize;
    let mut tombs_dropped = 0usize;
    let mut kway = KWay::new(&streams, &lens)?;
    while let Some((_, pi, row, shadowed)) = kway.next_group()? {
        superseded += shadowed;
        if parts[pi].is_tombstone(row)? {
            if drop_tombstones {
                tombs_dropped += 1;
            } else {
                tombs_kept += 1;
                records_out += 1;
            }
            continue;
        }
        records_out += 1;
        for (k, v) in parts[pi].attrs(row)? {
            let e = columns.entry((k, v.type_tag())).or_default();
            if let crate::types::AttrValue::Str(s) = v {
                e.insert(s);
            }
        }
    }
    let string_dicts: Vec<Vec<String>> =
        columns.values().map(|s| s.iter().cloned().collect()).collect();
    let columns: Vec<(String, u8)> = columns.into_keys().collect();

    // ---- pass B: stream every winning row into the builder ----
    //
    // The builder's dictionary RETAINS the whole gathered union, not just what the surviving
    // records reference. The fold never forgets, so every piece any input knew about is still
    // stored and still worth deduping against — and a record staged but not yet flushed may have
    // matched against an entry that would otherwise stop being referenced here.
    let dict: Vec<(Loc, PieceHash)> = locs.iter().map(|(h, l)| (*l, *h)).collect();
    let mut b = crate::part::builder::StreamBuilder::new(out, level, dict, columns, string_dicts)?;
    let mut kway = KWay::new(&streams, &lens)?;
    while let Some((id, pi, row, _)) = kway.next_group()? {
        if parts[pi].is_tombstone(row)? {
            if !drop_tombstones {
                b.push(&id, true, &[], &[])?;
            }
            continue;
        }
        b.push(&id, false, &parts[pi].body(row)?, &parts[pi].attrs(row)?)?;
    }
    let meta = b.finish(seq_lo, seq_hi)?;

    let stats = MergeStats {
        inputs: parts.len(),
        records_in,
        records_out,
        superseded,
        tombstones_kept: tombs_kept,
        tombstones_dropped: tombs_dropped,
        fold_bytes_touched: 0,
    };
    Ok((meta, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{Fold, FoldCfg};
    use crate::types::{AttrValue, BodyOp, Record};

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let n =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("turndb-merge-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn part_of(
        dir: &std::path::Path,
        fold: &mut Fold,
        name: &str,
        seq: u64,
        recs: &[(&str, &str)],
    ) -> Arc<Part> {
        let rs: Vec<Record> = recs
            .iter()
            .map(|(id, body)| {
                let p = fold.put(body.as_bytes()).unwrap();
                Record {
                    id: (*id).to_string(),
                    body: vec![BodyOp::Piece { hash: p.hash, len: p.loc.raw }],
                    attrs: vec![("v".into(), AttrValue::Str((*body).to_string()))],
                }
            })
            .collect();
        let path = dir.join(name);
        super::super::build(&path, &rs, seq, seq, 3, |h| fold.lookup(*h)).unwrap();
        Arc::new(Part::open(&path).unwrap())
    }

    #[test]
    fn merges_and_resolves_newest_wins() {
        let d = tmpdir("basic");
        let mut fold = Fold::open(&d.join("fold"), FoldCfg::default()).unwrap();
        let a = part_of(&d, &mut fold, "a.part", 1, &[("k1", "one"), ("k2", "two")]);
        let b = part_of(&d, &mut fold, "b.part", 2, &[("k2", "TWO-NEW"), ("k3", "three")]);
        fold.sync().unwrap();

        let outp = d.join("m.part");
        let (meta, st) = merge(&outp, &[a, b], 3).unwrap();
        assert_eq!(meta.n_records, 3, "k1, k2, k3");
        assert_eq!((meta.seq_lo, meta.seq_hi), (1, 2), "output must span its inputs' sequences");
        assert_eq!(st.superseded, 1);
        assert_eq!(st.fold_bytes_touched, 0, "MERGE MUST NOT TOUCH THE FOLD");

        let m = Part::open(&outp).unwrap();
        assert_eq!(m.reconstruct(m.find("k1").unwrap().unwrap(), &fold).unwrap(), b"one");
        assert_eq!(
            m.reconstruct(m.find("k2").unwrap().unwrap(), &fold).unwrap(),
            b"TWO-NEW",
            "the later part's version must win"
        );
        assert_eq!(m.reconstruct(m.find("k3").unwrap().unwrap(), &fold).unwrap(), b"three");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn output_is_independent_of_argument_order() {
        let d = tmpdir("determinism");
        let mut fold = Fold::open(&d.join("fold"), FoldCfg::default()).unwrap();
        let a = part_of(&d, &mut fold, "a.part", 1, &[("x", "alpha"), ("y", "beta")]);
        let b = part_of(&d, &mut fold, "b.part", 2, &[("y", "BETA2"), ("z", "gamma")]);
        fold.sync().unwrap();

        let p1 = d.join("m1.part");
        let p2 = d.join("m2.part");
        merge(&p1, &[a.clone(), b.clone()], 3).unwrap();
        merge(&p2, &[b, a], 3).unwrap();
        assert_eq!(
            std::fs::read(&p1).unwrap(),
            std::fs::read(&p2).unwrap(),
            "a merge must be a pure function of its input SET, not of argument order"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// The pre-streaming merge — gather every winning record, hand the lot to build_full with the
    /// retained union — replayed inline as the ORACLE: the streaming two-pass merge must produce
    /// the identical file, byte for byte, tombstones included.
    #[test]
    fn streaming_merge_matches_the_materialized_oracle_byte_for_byte() {
        let d = tmpdir("oracle");
        let mut fold = Fold::open(&d.join("fold"), FoldCfg::default()).unwrap();
        let a = part_of(&d, &mut fold, "a.part", 1, &[("k1", "one"), ("k2", "two")]);
        let b = part_of(&d, &mut fold, "b.part", 2, &[("k2", "TWO-NEW"), ("k3", "three")]);
        // a third part deleting k1 — a tombstone the merge must carry forward
        let tomb_rec = Record { id: "k1".into(), body: Vec::new(), attrs: Vec::new() };
        let cp = d.join("c.part");
        super::super::build_full(&cp, &[tomb_rec], &[true], 3, 3, 3, |_| None, &HashMap::new())
            .unwrap();
        let c = Arc::new(Part::open(&cp).unwrap());
        fold.sync().unwrap();

        let streamed = d.join("streamed.part");
        merge(&streamed, &[a.clone(), b.clone(), c.clone()], 3).unwrap();

        // The oracle: winners are k1 (tombstone from c), k2 (from b), k3 (from b); the dictionary
        // retains the whole union.
        let mut locs: HashMap<PieceHash, Loc> = HashMap::new();
        for p in [&a, &b, &c] {
            for i in 0..p.piece_count().unwrap() {
                let (loc, hash) = p.piece(i).unwrap();
                locs.entry(hash).or_insert(loc);
            }
        }
        let recs = vec![
            Record { id: "k1".into(), body: Vec::new(), attrs: Vec::new() },
            b.record(b.find("k2").unwrap().unwrap()).unwrap(),
            b.record(b.find("k3").unwrap().unwrap()).unwrap(),
        ];
        let oracle = d.join("oracle.part");
        super::super::build_full(
            &oracle,
            &recs,
            &[true, false, false],
            1,
            3,
            3,
            |h| locs.get(h).copied(),
            &locs,
        )
        .unwrap();

        assert_eq!(
            std::fs::read(&streamed).unwrap(),
            std::fs::read(&oracle).unwrap(),
            "the streaming merge must be byte-identical to the materialized algorithm"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// A total merge of a store whose every record was deleted: the tombstones drop, and the
    /// output is a VALID EMPTY part — zero records, empty sections, correct footer — not an error
    /// and not a refusal. Deleting everything you stored is an ordinary thing to have done.
    #[test]
    fn a_total_merge_of_only_tombstones_yields_a_valid_empty_part() {
        let d = tmpdir("allgone");
        let r = Record { id: "x".into(), body: Vec::new(), attrs: Vec::new() };
        let p1 = d.join("a.part");
        super::super::build_full(&p1, &[r], &[true], 1, 1, 3, |_| None, &HashMap::new()).unwrap();
        let a = Arc::new(Part::open(&p1).unwrap());

        let out = d.join("m.part");
        let (meta, st) = merge_opts(&out, &[a], 3, true).unwrap();
        assert_eq!(meta.n_records, 0);
        assert_eq!(st.tombstones_dropped, 1);
        let m = Part::open(&out).unwrap();
        assert_eq!(m.len(), 0);
        assert!(m.ids().unwrap().is_empty());
        assert!(m.find("x").unwrap().is_none());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn overlapping_sequence_ranges_are_refused() {
        let d = tmpdir("overlap");
        let mut fold = Fold::open(&d.join("fold"), FoldCfg::default()).unwrap();
        let a = part_of(&d, &mut fold, "a.part", 5, &[("x", "1")]);
        let b = part_of(&d, &mut fold, "b.part", 5, &[("y", "2")]);
        assert!(merge(&d.join("m.part"), &[a, b], 3).is_err(), "overlapping sequences must refuse");
        std::fs::remove_dir_all(&d).ok();
    }
}
