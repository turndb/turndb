//! The re-folding merge: the one operation allowed to rewrite content.
//!
//! Everywhere else, `MergeStats::fold_bytes_touched == 0` is asserted and load-bearing — merges
//! reorganize references and columns, never bytes, which is what decouples merge cost from data
//! volume. This is the deliberate exception, and it is a separate operation rather than a mode of the
//! normal merge precisely so that the invariant stays a flat statement everywhere else.
//!
//! # What it is actually for
//!
//! Not compression. Measured, the three usual motives do not survive: garbage from superseded versions
//! is negligible (0 of 400k records on a real corpus), re-clustering has no headroom because fold order
//! already approximates co-reference order, and recompression buys ~5% at 3.4x the read cost.
//!
//! It exists because **`delete` reclaims nothing on its own**. Ordinary fold writes append and the
//! stored pieces are shared —
//! the same bytes may still be referenced by records that are live — so a tombstone can only make
//! content unreachable, never absent. This is what makes deletion mean something on disk, which is a
//! retention requirement rather than an optimisation.
//!
//! # Why the fold is GENERATIONAL
//!
//! The new fold cannot overwrite the old one: read views remain pinned to their store authority,
//! and rewriting required bytes under them would hand back wrong content rather than an error. A
//! fold therefore lives in a generation namespace named by a manifest revision, and one
//! container-state publication atomically selects the replacement authority.
//!
//! ```text
//!   stage the new generation and rebuilt parts; synchronize their bytes
//!   stage the manifest revision and removal of superseded retained authority
//!   flip one container state                      <- the instant the replacement takes effect
//! ```
//! Bytes beyond the selected container tail are unpublished and ignored. Extents abandoned by the
//! selected directory are recorded as free in that same atomic state; writer open performs no
//! directory-era orphan sweep.
//!
//! # What it drops
//!
//! Pieces no record resolved by the current manifest revision references. A record is present when the newest part holding its id is not a
//! tombstone — so this discards deleted records, superseded versions, and any content only they
//! referenced. Tombstones themselves go too: a re-fold covers every part, so nothing survives for them
//! to shadow.

use crate::fold::{Fold, FoldCfg, Loc};
use crate::part::Part;
use crate::types::{ContentOp, PieceHash, Record};
use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default)]
pub struct RefoldStats {
    pub parts_in: usize,
    pub parts_out: usize,
    pub records_kept: usize,
    /// Records discarded: deleted, or superseded by a newer version elsewhere.
    pub records_dropped: usize,
    pub tombstones_dropped: usize,
    pub pieces_kept: usize,
    /// Pieces no record resolved by current authority referenced. These are the bytes reclaimed.
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

/// The home-neutral half of a refold: which rows survive, which pieces they still reference,
/// and the fold-order copy plan. Everything after this is placement.
#[allow(clippy::type_complexity)]
fn plan_refold(
    parts: &[Arc<Part>],
    control: &crate::control::OperationControl,
) -> Result<(Vec<Vec<usize>>, Vec<(Loc, PieceHash)>, RefoldStats)> {
    let mut st = RefoldStats { parts_in: parts.len(), ..Default::default() };
    let visible = super::read::visibility(parts)?;
    let live = visible.rows;
    st.records_dropped = visible.superseded + visible.tombstones;
    st.tombstones_dropped = visible.tombstones;
    st.records_kept = live.iter().map(|v| v.len()).sum();

    // Every piece a record resolved by current authority still references, with its present location.
    let mut wanted: HashMap<PieceHash, Loc> = HashMap::new();
    for (pi, rows) in live.iter().enumerate() {
        for &row in rows {
            control.check("content refold")?;
            for content in parts[pi].record(row)?.contents {
                for op in content.ops {
                    let ContentOp::Piece { hash, .. } = op else { continue };
                    if wanted.contains_key(&hash) {
                        continue;
                    }
                    let loc = parts[pi].find_piece(&hash)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "a record resolved by current authority references piece {hash}, which no part locates"
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
    Ok((live, order, st))
}

/// A rebuilt part as the in-file refold names it: file, sequence range, rows, and the BLAKE3 pin
/// computed in the pass that wrote it.
pub(crate) type RefoldedMemberPart = (String, u64, u64, u32, String);

/// The refold's single-file form: the new generation grows as members beside the old one, the
/// rebuilt parts stream into members pinned in-pass, and NOTHING here publishes — the caller's
/// one superblock flip performs the swap, the retained-log purge, and the sweep's frees as one
/// atomic state. On any error the container's staged state is discarded whole: the bytes written
/// are uncommitted noise, which is all a crash would have left too.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn refold_into_container_with_control_and_limits(
    container: std::sync::Arc<std::sync::Mutex<crate::container::Container>>,
    parts: &[Arc<Part>],
    seqs: &[(u64, u64)],
    old_fold: &Fold,
    old_gen: u32,
    cfg: FoldCfg,
    control: &crate::control::OperationControl,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<(u32, Vec<RefoldedMemberPart>, RefoldStats, Fold)> {
    let build = (|| -> Result<(u32, Vec<RefoldedMemberPart>, RefoldStats, Fold)> {
        control.check("content refold")?;
        if parts.len() != seqs.len() {
            bail!("every part needs its committed sequence range");
        }
        let new_gen = old_gen
            .checked_add(1)
            .filter(|generation| *generation <= crate::fold::MAX_FOLD_GENERATION)
            .ok_or_else(|| anyhow::anyhow!("fold generation namespace exhausted"))?;
        let (live, order, mut st) = plan_refold(parts, control)?;
        st.fold_bytes_before = old_fold.disk_bytes();

        let mut nf =
            Fold::open_container_writer(container.clone(), new_gen, cfg, None, &[], read_limits)?;
        let mut remap: HashMap<PieceHash, Loc> = HashMap::with_capacity(order.len());
        for (loc, hash) in &order {
            control.check("content refold")?;
            // read_verified, not read: a re-fold is exactly when to re-check that content still
            // hashes to the identity claiming it — everything downstream will trust the copy.
            let bytes = old_fold.read_verified(*loc, *hash)?;
            let put = nf.put_hashed(&bytes, *hash)?;
            remap.insert(*hash, put.loc);
        }
        control.check("content refold")?;
        nf.sync()?;
        st.fold_bytes_after = nf.disk_bytes();

        let mut out = Vec::new();
        let last_surviving = live.iter().rposition(|rows| !rows.is_empty());
        // A refold may eliminate an entire part because every version in it was superseded or
        // tombstoned, and that part can sit anywhere in the input run. Its sequence interval is
        // still resolution history, and the manifest requires the surviving parts to describe one
        // contiguous history from the first input's `seq_lo` to the last input's `seq_hi`. So an
        // eliminated interval is folded into the next surviving output, and the last surviving
        // output absorbs every eliminated interval after it, keeping `seq_hi` the manifest's exact
        // published mutation cursor. Resolution is unaffected: intervals only order parts, and an
        // eliminated part had no version left to order.
        let mut pending_lo = seqs.first().expect("refold has at least one input part").0;
        for (pi, rows) in live.iter().enumerate() {
            control.check("content refold")?;
            if rows.is_empty() {
                continue; // every record in this part was deleted or superseded
            }
            let recs: Vec<Record> =
                rows.iter().map(|&r| parts[pi].record(r)).collect::<Result<_>>()?;
            let lo = pending_lo;
            let hi = if Some(pi) == last_surviving {
                seqs.last().expect("parts and sequence ranges are parallel").1
            } else {
                seqs[pi].1
            };
            pending_lo = hi.saturating_add(1);
            let file = format!("part-r{new_gen:04}-{lo:08}-{hi:08}.part");
            let member = container.lock().expect("container lock poisoned").begin_member(&file)?;
            let built = crate::part::build_full_into(
                member,
                &recs,
                &[],
                lo,
                hi,
                cfg.level,
                |h| remap.get(h).copied(),
                &HashMap::new(),
                read_limits,
            );
            let (meta, member) = match built {
                Ok(v) => v,
                Err(error) => {
                    container.lock().expect("container lock poisoned").abandon_open_member();
                    return Err(error);
                }
            };
            let digest = {
                let mut c = container.lock().expect("container lock poisoned");
                PieceHash(c.finish_member(member)?).to_hex()
            };
            out.push((file, lo, hi, meta.n_records, digest));
        }
        if out.is_empty() {
            // A persisted manifest with no parts is the first empty authority and therefore has
            // cursor zero. Once record-version sequences have existed, even erasing every row must
            // not make the cursor reusable. A canonical zero-row spanning part is the physical
            // evidence that carries the old sequence domain through the empty refold result.
            let lo = seqs.first().expect("refold has at least one input part").0;
            let hi = seqs.last().expect("refold has at least one input part").1;
            let file = format!("part-r{new_gen:04}-{lo:08}-{hi:08}.part");
            let member = container.lock().expect("container lock poisoned").begin_member(&file)?;
            let built = crate::part::build_full_into(
                member,
                &[],
                &[],
                lo,
                hi,
                cfg.level,
                |_| None,
                &HashMap::new(),
                read_limits,
            );
            let (meta, member) = match built {
                Ok(value) => value,
                Err(error) => {
                    container.lock().expect("container lock poisoned").abandon_open_member();
                    return Err(error);
                }
            };
            let digest = {
                let mut container = container.lock().expect("container lock poisoned");
                PieceHash(container.finish_member(member)?).to_hex()
            };
            out.push((file, lo, hi, meta.n_records, digest));
        }
        control.check("content refold")?;
        st.parts_out = out.len();
        Ok((new_gen, out, st, nf))
    })();
    match build {
        Ok(v) => Ok(v),
        Err(error) => {
            // The unwind is one statement: the staged view snaps back to the committed one, and
            // every byte this refold wrote is uncommitted noise past the tail.
            let _ = container.lock().expect("container lock poisoned").discard_staged();
            Err(error)
        }
    }
}
