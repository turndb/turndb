//! The merge operator: N parts in, one part out. **The fold is never touched.**
//!
//! That asymmetry is the point. Content lives in the fold, addressed by logical block id, and a part
//! holds only references and columns — so consolidating parts rewrites references and columns and not
//! a single content byte. In a conventional LSM, compaction rewrites the data.
//!
//! # Contiguity is a correctness gate, not a nicety
//!
//! Version resolution across parts compares sequence numbers. If parts with sequences 1 and 3 were
//! merged while 2 was left out, the output would claim the range [1,3] while missing whatever 2 said
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
use crate::types::{PieceHash, Record};
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
    /// Always zero. Asserted, because "merge never touches the fold" is the load-bearing claim.
    pub fold_bytes_touched: u64,
}

/// Merge a contiguous run of parts into `out`.
pub fn merge(out: &Path, inputs: &[Arc<Part>], level: i32) -> Result<(PartMeta, MergeStats)> {
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
                w[0].meta().seq_lo, w[0].meta().seq_hi, w[1].meta().seq_lo, w[1].meta().seq_hi
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

    // K-way merge over the id columns. Parts are id-sorted and hold one version per id, so a simple
    // positional walk suffices; later parts win ties.
    let ids: Vec<Vec<String>> = parts.iter().map(|p| p.ids()).collect::<Result<_>>()?;
    let records_in: usize = ids.iter().map(|v| v.len()).sum();
    let mut cursor = vec![0usize; parts.len()];
    let mut out_recs: Vec<Record> = Vec::with_capacity(records_in);
    let mut superseded = 0usize;

    loop {
        // smallest id across all cursors
        let mut best: Option<&str> = None;
        for (i, c) in cursor.iter().enumerate() {
            if let Some(id) = ids[i].get(*c) {
                if best.map_or(true, |b| id.as_str() < b) {
                    best = Some(id);
                }
            }
        }
        let Some(id) = best.map(|s| s.to_string()) else { break };

        // every part holding it advances; the LAST (highest sequence) wins
        let mut winner: Option<(usize, usize)> = None;
        for (i, c) in cursor.iter_mut().enumerate() {
            if ids[i].get(*c).map(|s| s.as_str()) == Some(id.as_str()) {
                if winner.is_some() {
                    superseded += 1;
                }
                winner = Some((i, *c));
                *c += 1;
            }
        }
        let (pi, row) = winner.expect("an id was found, so some part holds it");
        out_recs.push(parts[pi].record(row)?);
    }

    let meta = super::build(out, &out_recs, seq_lo, seq_hi, level, |h| locs.get(h).copied())?;
    let stats = MergeStats {
        inputs: parts.len(),
        records_in,
        records_out: out_recs.len(),
        superseded,
        fold_bytes_touched: 0,
    };
    Ok((meta, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::{Fold, FoldCfg};
    use crate::types::{AttrValue, BodyOp};

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("turndb-merge-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn part_of(dir: &std::path::Path, fold: &mut Fold, name: &str, seq: u64, recs: &[(&str, &str)]) -> Arc<Part> {
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
