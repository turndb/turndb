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

/// The generation a fold directory name denotes: `fold` is 0, `fold-NNNN` is N. `None` for a name
/// that is not a fold directory at all.
pub fn parse_fold_gen(name: &str) -> Option<u32> {
    if name == "fold" {
        return Some(0);
    }
    name.strip_prefix("fold-").and_then(|g| g.parse::<u32>().ok())
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
/// A part the re-fold rebuilt: `(file name, seq_lo, seq_hi, record count)` — what the caller needs
/// to write a new manifest entry for it.
pub type RefoldedPart = (String, u64, u64, u32);

pub fn refold(
    dir: &Path,
    parts: &[Arc<Part>],
    seqs: &[(u64, u64)],
    old_fold: &Fold,
    old_gen: u32,
    cfg: FoldCfg,
) -> Result<(u32, Vec<RefoldedPart>, RefoldStats)> {
    refold_with_control(
        dir,
        parts,
        seqs,
        old_fold,
        old_gen,
        cfg,
        &crate::control::OperationControl::default(),
    )
}

/// [`refold`] with cooperative checkpoints throughout its unpublished generation build.
pub fn refold_with_control(
    dir: &Path,
    parts: &[Arc<Part>],
    seqs: &[(u64, u64)],
    old_fold: &Fold,
    old_gen: u32,
    cfg: FoldCfg,
    control: &crate::control::OperationControl,
) -> Result<(u32, Vec<RefoldedPart>, RefoldStats)> {
    refold_with_control_and_limits(
        dir,
        parts,
        seqs,
        old_fold,
        old_gen,
        cfg,
        control,
        crate::read_limits::ReadLimits::default(),
    )
}

/// [`refold_with_control`] with frame and object-count admission on the replacement generation.
#[allow(clippy::too_many_arguments)]
pub fn refold_with_control_and_limits(
    dir: &Path,
    parts: &[Arc<Part>],
    seqs: &[(u64, u64)],
    old_fold: &Fold,
    old_gen: u32,
    cfg: FoldCfg,
    control: &crate::control::OperationControl,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<(u32, Vec<RefoldedPart>, RefoldStats)> {
    control.check("content refold")?;
    if parts.len() != seqs.len() {
        bail!("every part needs its committed sequence range");
    }
    let new_gen = old_gen + 1;
    let new_dir = fold_dir(dir, new_gen);
    if new_dir.exists() {
        crate::vfs::remove_tree(&new_dir)?;
    }
    let mut cleanup = StagedRefold::new(dir, new_gen, seqs);

    let mut st = RefoldStats { parts_in: parts.len(), ..Default::default() };
    st.fold_bytes_before = dir_bytes(&fold_dir(dir, old_gen), read_limits)?;

    let visible = super::read::visibility(parts)?;
    let live = visible.rows;
    st.records_dropped = visible.superseded + visible.tombstones;
    st.tombstones_dropped = visible.tombstones;
    st.records_kept = live.iter().map(|v| v.len()).sum();

    // Every piece a live record still references, with where it lives TODAY.
    let mut wanted: HashMap<PieceHash, Loc> = HashMap::new();
    for (pi, rows) in live.iter().enumerate() {
        for &row in rows {
            control.check("content refold")?;
            for content in parts[pi].record(row)?.contents {
                for op in content.ops {
                    let BodyOp::Piece { hash, .. } = op else { continue };
                    if wanted.contains_key(&hash) {
                        continue;
                    }
                    let loc = parts[pi].lookup_piece(&hash)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "live record references piece {hash}, which no part locates"
                        )
                    })?;
                    wanted.insert(hash, loc);
                }
            }
        }
    }
    let total_pieces: usize = {
        let mut all = HashSet::new();
        for p in parts {
            for i in 0..p.piece_count()? {
                control.check("content refold")?;
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
        let mut nf = Fold::open_with_limits(&new_dir, cfg, read_limits)?;
        for (loc, hash) in &order {
            control.check("content refold")?;
            // read_verified, not read: a re-fold is exactly when to re-check that content still hashes
            // to the identity claiming it, since everything downstream is about to trust the copy.
            let bytes = old_fold.read_verified(*loc, *hash)?;
            let put = nf.put_hashed(&bytes, *hash)?;
            remap.insert(*hash, put.loc);
        }
        control.check("content refold")?;
        nf.sync()?;
    }
    st.fold_bytes_after = dir_bytes(&new_dir, read_limits)?;

    // Rebuild each part against the new fold, keeping only live rows and no tombstones. `retain` is
    // deliberately EMPTY: carrying dictionary entries forward is right for an ordinary merge, where the
    // content still exists, and wrong here, where it has just been dropped.
    let mut out = Vec::new();
    for (pi, rows) in live.iter().enumerate() {
        control.check("content refold")?;
        if rows.is_empty() {
            continue; // every record in this part was deleted or superseded
        }
        let recs: Vec<Record> = rows.iter().map(|&r| parts[pi].record(r)).collect::<Result<_>>()?;
        let (lo, hi) = seqs[pi];
        let file = format!("part-r{new_gen:04}-{lo:08}-{hi:08}.part");
        let meta = part::build_full_with_limits(
            &dir.join(&file),
            &recs,
            &[],
            lo,
            hi,
            cfg.level,
            |h| remap.get(h).copied(),
            &HashMap::new(),
            read_limits,
        )?;
        out.push((file, lo, hi, meta.n_records));
    }
    control.check("content refold")?;
    st.parts_out = out.len();
    cleanup.disarm();
    Ok((new_gen, out, st))
}

struct StagedRefold {
    fold: PathBuf,
    parts: Vec<PathBuf>,
    armed: bool,
}

impl StagedRefold {
    fn new(dir: &Path, generation: u32, seqs: &[(u64, u64)]) -> StagedRefold {
        StagedRefold {
            fold: fold_dir(dir, generation),
            parts: seqs
                .iter()
                .map(|&(lo, hi)| dir.join(format!("part-r{generation:04}-{lo:08}-{hi:08}.part")))
                .collect(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagedRefold {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.fold.exists() {
            let _ = crate::vfs::remove_tree(&self.fold);
        }
        for part in &self.parts {
            if part.exists() {
                let _ = crate::vfs::unlink(part);
            }
        }
    }
}

fn dir_bytes(d: &Path, read_limits: crate::read_limits::ReadLimits) -> Result<u64> {
    let mut bytes = 0u64;
    let mut visited = 0u64;
    for entry in std::fs::read_dir(d)? {
        visited = visited.saturating_add(1);
        read_limits.admit_directory_entries("refold directory", visited)?;
        let metadata = entry?.metadata()?;
        if metadata.is_file() {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_stage_guard_removes_every_unpublished_artifact() {
        let root = std::env::temp_dir().join(format!(
            "turndb-refold-stage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let seqs = [(1, 2), (3, 4)];
        let stage = StagedRefold::new(&root, 7, &seqs);
        std::fs::create_dir_all(fold_dir(&root, 7)).unwrap();
        std::fs::write(fold_dir(&root, 7).join("partial"), b"partial").unwrap();
        for path in &stage.parts {
            std::fs::write(path, b"partial").unwrap();
        }
        drop(stage);
        assert!(!fold_dir(&root, 7).exists());
        assert!(std::fs::read_dir(&root).unwrap().next().is_none());
        std::fs::remove_dir_all(root).ok();
    }
}
