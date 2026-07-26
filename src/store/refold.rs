//! The re-folding merge: the one operation allowed to rewrite content.
//!
//! Everywhere else, `MergeStats::fold_bytes_touched == 0` is asserted and load-bearing — merges
//! reorganize references and columns, never bytes, which is what decouples compaction cost from data
//! volume. This is the deliberate exception, and it is a separate operation rather than a mode of the
//! normal merge precisely so that the invariant stays a flat statement everywhere else.
//!
//! # What it is actually for
//!
//! Not compression. Measured, the three usual motives do not survive: garbage from superseded versions
//! is negligible (0 of 400k records on a real corpus), re-clustering has no headroom because fold order
//! already approximates co-reference order, and recompression buys ~5% at 3.4x the read cost.
//!
//! It exists because **`delete` reclaims nothing on its own**. The fold is append-only and shared —
//! the same bytes may still be referenced by records that are live — so a tombstone can only make
//! content unreachable, never absent. This is what makes deletion mean something on disk, which is a
//! retention requirement rather than an optimisation.
//!
//! # Why the fold is GENERATIONAL
//!
//! The new fold cannot overwrite the old one: readers hold an open manifest naming the old, and
//! rewriting under them would hand back wrong bytes rather than an error. So a fold lives in a
//! generation directory the manifest names, and the swap is the manifest commit — the store's single
//! linearization point, already atomic. Generation 0 keeps the plain `fold/` name, so a store written
//! before this existed needs no migration.
//!
//! ```text
//!   write fold-0001/ and the new parts, fsync    <- crash here: orphans, swept at open
//!   commit the manifest naming both              <- the instant it takes effect
//!   unlink fold/ and the old parts               <- crash here: orphans, swept at open
//! ```
//!
//! # What it drops
//!
//! Pieces no LIVE record references. A record is live when the newest part holding its id is not a
//! tombstone — so this discards deleted records, superseded versions, and any content only they
//! referenced. Tombstones themselves go too: a re-fold covers every part, so nothing survives for them
//! to shadow.


use crate::fold::{Fold, FoldCfg, Loc};
use crate::part::{self, Part};
use crate::types::{BodyOp, PieceHash, Record};
use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where a generation's fold lives. Generation 0 is the original `fold/`, unchanged.
pub fn fold_dir(dir: &Path, gen: u32) -> PathBuf {
    if gen == 0 {
        dir.join("fold")
    } else {
        dir.join(format!("fold-{gen:04}"))
    }
}

/// Is `name` a fold directory for some generation other than `live`?
pub fn is_stale_fold(name: &str, live: u32) -> bool {
    if name == "fold" {
        return live != 0;
    }
    match name.strip_prefix("fold-").and_then(|g| g.parse::<u32>().ok()) {
        Some(g) => g != live,
        None => false,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RefoldStats {
    pub parts_in: usize,
    pub parts_out: usize,
    pub records_kept: usize,
    /// Records discarded: deleted, or superseded by a newer version elsewhere.
    pub records_dropped: usize,
    pub tombstones_dropped: usize,
    pub pieces_kept: usize,
    /// Pieces no live record referenced. These are the bytes reclaimed.
    pub pieces_dropped: usize,
    pub fold_bytes_before: u64,
    pub fold_bytes_after: u64,
    /// The superseded fold generation could not be unlinked and is still on disk. The re-fold itself
    /// committed and is correct; the space is simply not reclaimed yet, and a caller reading
    /// [`RefoldStats::bytes_reclaimed`] needs to know that.
    pub stale_generation_left: bool,
}

impl RefoldStats {
    /// Bytes the re-fold removed — **provided [`stale_generation_left`] is false**. If the old
    /// generation could not be unlinked, this is what WILL be reclaimed once it is, not what has been.
    ///
    /// [`stale_generation_left`]: RefoldStats::stale_generation_left
    pub fn bytes_reclaimed(&self) -> u64 {
        self.fold_bytes_before.saturating_sub(self.fold_bytes_after)
    }
}

/// Rewrite the fold keeping only content live records reference, and rebuild the parts against it.
///
/// `parts` must be the store's full live list — a re-fold is all-or-nothing by construction, because
/// content it drops could otherwise still be referenced by a part it did not rebuild.
pub fn refold(
    dir: &Path,
    parts: &[Arc<Part>],
    seqs: &[(u64, u64)],
    old_fold: &Fold,
    old_gen: u32,
    cfg: FoldCfg,
) -> Result<(u32, Vec<(String, u64, u64, u32)>, RefoldStats)> {
    if parts.len() != seqs.len() {
        bail!("every part needs its committed sequence range");
    }
    let new_gen = old_gen + 1;
    let new_dir = fold_dir(dir, new_gen);
    if new_dir.exists() {
        std::fs::remove_dir_all(&new_dir)?;
    }

    let mut st = RefoldStats { parts_in: parts.len(), ..Default::default() };
    st.fold_bytes_before = dir_bytes(&fold_dir(dir, old_gen));

    let visible = super::read::visibility(parts)?;
    let live = visible.rows;
    st.records_dropped = visible.superseded + visible.tombstones;
    st.tombstones_dropped = visible.tombstones;
    st.records_kept = live.iter().map(|v| v.len()).sum();

    // Every piece a live record still references, with where it lives TODAY.
    let mut wanted: HashMap<PieceHash, Loc> = HashMap::new();
    for (pi, rows) in live.iter().enumerate() {
        for &row in rows {
            for op in parts[pi].body(row)? {
                let BodyOp::Piece { hash, .. } = op else { continue };
                if wanted.contains_key(&hash) {
                    continue;
                }
                let loc = parts[pi]
                    .lookup_piece(&hash)?
                    .ok_or_else(|| anyhow::anyhow!("live record references piece {hash}, which no part locates"))?;
                wanted.insert(hash, loc);
            }
        }
    }
    let total_pieces: usize = {
        let mut all = HashSet::new();
        for p in parts {
            for i in 0..p.piece_count()? {
                all.insert(p.piece(i)?.1);
            }
        }
        all.len()
    };
    st.pieces_kept = wanted.len();
    st.pieces_dropped = total_pieces.saturating_sub(wanted.len());

    // Copy in FOLD ORDER. That order is capture order, which is worth 1.83x over any content-derived
    // order on real traces, and rewriting in an arbitrary order would silently give it away.
    let mut order: Vec<(Loc, PieceHash)> = wanted.iter().map(|(h, l)| (*l, *h)).collect();
    order.sort_by_key(|(l, _)| (l.block_id, l.in_off));

    let mut remap: HashMap<PieceHash, Loc> = HashMap::with_capacity(order.len());
    {
        let mut nf = Fold::open(&new_dir, cfg)?;
        for (loc, hash) in &order {
            // read_verified, not read: a re-fold is exactly when to re-check that content still hashes
            // to the identity claiming it, since everything downstream is about to trust the copy.
            let bytes = old_fold.read_verified(*loc, *hash)?;
            let put = nf.put_hashed(&bytes, *hash)?;
            remap.insert(*hash, put.loc);
        }
        nf.sync()?;
    }
    st.fold_bytes_after = dir_bytes(&new_dir);

    // Rebuild each part against the new fold, keeping only live rows and no tombstones. `retain` is
    // deliberately EMPTY: carrying dictionary entries forward is right for an ordinary merge, where the
    // content still exists, and wrong here, where it has just been dropped.
    let mut out = Vec::new();
    for (pi, rows) in live.iter().enumerate() {
        if rows.is_empty() {
            continue; // every record in this part was deleted or superseded
        }
        let recs: Vec<Record> = rows.iter().map(|&r| parts[pi].record(r)).collect::<Result<_>>()?;
        let (lo, hi) = seqs[pi];
        let file = format!("part-r{new_gen:04}-{lo:08}-{hi:08}.part");
        let meta = part::build_full(
            &dir.join(&file),
            &recs,
            &[],
            lo,
            hi,
            cfg.level,
            |h| remap.get(h).copied(),
            &HashMap::new(),
        )?;
        out.push((file, lo, hi, meta.n_records));
    }
    st.parts_out = out.len();
    Ok((new_gen, out, st))
}

fn dir_bytes(d: &Path) -> u64 {
    std::fs::read_dir(d)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}
