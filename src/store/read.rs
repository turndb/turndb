//! The read core: everything answerable from committed parts plus the fold.
//!
//! # Why this exists
//!
//! `Store` and `ReadStore` both read the same parts, and each had its own copy of the logic. Three
//! separate defects in one working session were "fixed in one path and not its twin":
//!
//!   * the Tier-0-only piece resolve, fixed in `flush` and missed in `rebuild`,
//!   * the same resolve again, fixed in `rebuild` and missed in `Store::locate`'s callers,
//!   * tombstones, honoured in `Store::reconstruct` and missed in `ReadStore::reconstruct`.
//!
//! Each was found by a test rather than by review, and each could as easily not have been. The
//! duplication was the defect; the individual bugs were symptoms. Both types now delegate here, so a
//! read-path fix cannot land in only half of the store.
//!
//! # What lives here and what does not
//!
//! Only what the COMMITTED state can answer. A writer's memtable is newer than every part and is
//! layered on top by `Store`; it has no place here, because `ReadStore` has no memtable and pretending
//! otherwise is how the two drifted apart before.
//!
//! # Version resolution, in one rule
//!
//! Parts are ordered oldest to newest, so the FIRST part found scanning backwards decides — and if it
//! says the id is deleted, the answer is absent. Older parts still holding the id are superseded, not
//! consulted. Every function here obeys that one rule; that is the whole reason it is one place.

use crate::fold::Fold;
use crate::part::Part;
use crate::types::Record;
use anyhow::Result;
use std::collections::HashSet;
use std::sync::Arc;

/// The live row ordinals in every part, under the store's newest-wins rule.
///
/// This is the bulk form of [`locate`]: visit parts newest-first, let the first row for an id decide,
/// and let a tombstone decide that no row is visible. Keeping it here makes point reads, refolding and
/// query scans share one definition of committed visibility.
pub(crate) struct Visibility {
    pub rows: Vec<Vec<usize>>,
    /// Older rows hidden by a newer record or tombstone.
    pub superseded: usize,
    /// Newest rows that are tombstones and therefore resolve their ids to absence.
    pub tombstones: usize,
}

pub(crate) fn visibility(parts: &[Arc<Part>]) -> Result<Visibility> {
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); parts.len()];
    let mut seen: HashSet<String> = HashSet::new();
    let (mut superseded, mut tombstones) = (0usize, 0usize);

    for (pi, p) in parts.iter().enumerate().rev() {
        let ids = p.ids()?;
        let tombs = p.tombstones()?;
        for (row, id) in ids.iter().enumerate() {
            if !seen.insert(id.clone()) {
                superseded += 1;
            } else if tombs.binary_search(&(row as u64)).is_ok() {
                tombstones += 1;
            } else {
                rows[pi].push(row);
            }
        }
    }

    Ok(Visibility { rows, superseded, tombstones })
}

/// The newest committed version of `id`, or `None` if it is absent or deleted.
pub fn get(parts: &[Arc<Part>], id: &str) -> Result<Option<Record>> {
    match locate(parts, id)? {
        Some((p, row)) => Ok(Some(p.record(row)?)),
        None => Ok(None),
    }
}

/// Byte-exact content for `id`, or `None` if it is absent or deleted.
pub fn reconstruct(parts: &[Arc<Part>], fold: &Fold, id: &str) -> Result<Option<Vec<u8>>> {
    reconstruct_content(parts, fold, id, crate::types::BODY_CONTENT)
}

/// Byte-exact named content for `id`, or `None` when either the record or value is absent.
pub fn reconstruct_content(
    parts: &[Arc<Part>],
    fold: &Fold,
    id: &str,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    match locate(parts, id)? {
        Some((p, row)) => p.reconstruct_content(row, name, fold),
        None => Ok(None),
    }
}

/// Whether `id` exists in the committed state — cheaper than [`get`], since no record is decoded.
pub fn exists(parts: &[Arc<Part>], id: &str) -> Result<bool> {
    Ok(locate(parts, id)?.is_some())
}

/// The part and row holding the LIVE version of `id`, if there is one.
///
/// The single place the newest-wins-and-tombstones-are-absent rule is written down.
fn locate<'a>(parts: &'a [Arc<Part>], id: &str) -> Result<Option<(&'a Arc<Part>, usize)>> {
    for p in parts.iter().rev() {
        if let Some(row) = p.find(id)? {
            if p.is_tombstone(row)? {
                return Ok(None);
            }
            return Ok(Some((p, row)));
        }
    }
    Ok(None)
}

/// Distinct live committed ids, sorted.
///
/// Two filters, and both are needed: a part's own tombstoned rows are skipped, and an id an OLDER part
/// still lists is dropped when a newer part deletes it.
/// Live ids in `[from, to)`, in id order (or reversed), at most `limit`.
///
/// The paged read: ids are sorted, so a range is a contiguous run in every part, and each part
/// contributes it with a binary search plus a walk of exactly that run. Only ids inside the range
/// are ever decoded, which is what separates this from [`ids`] — that one materialises the whole
/// store to answer anything.
///
/// Version resolution is the store's one rule, unchanged: an id is listed when the newest part
/// holding it is not a tombstone. Resolution happens per candidate id rather than by walking every
/// part's visibility, so the cost tracks the page, not the store.
///
/// Because ids sort lexicographically, a caller who designs ids with the query in mind — a
/// `member/timestamp/...` prefix, say — gets member-then-time paging out of this with no secondary
/// index at all. `reverse` walks the same run backwards, which is what a newest-first UI wants.
pub fn scan_ids(
    parts: &[Arc<Part>],
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
    reverse: bool,
) -> Result<Vec<String>> {
    if limit == 0 || parts.is_empty() {
        return Ok(Vec::new());
    }
    // Candidates from every part's matching run. A part contributes at most its own run, so this
    // is bounded by the range rather than by the store.
    let mut cand: Vec<String> = Vec::new();
    for p in parts {
        let rows = p.rows_in_range(from, to)?;
        if rows.is_empty() {
            continue;
        }
        let listed = p.ids()?;
        for row in rows {
            if let Some(id) = listed.get(row) {
                cand.push(id.clone());
            }
        }
    }
    cand.sort_unstable();
    cand.dedup();
    if reverse {
        cand.reverse();
    }

    // Resolve visibility per candidate, stopping as soon as the page is full — the reason this
    // does not call `visibility`, which is O(store).
    let mut out = Vec::with_capacity(limit.min(cand.len()));
    for id in cand {
        if locate(parts, &id)?.is_some() {
            out.push(id);
            if out.len() == limit {
                break;
            }
        }
    }
    Ok(out)
}

pub fn ids(parts: &[Arc<Part>]) -> Result<Vec<String>> {
    let visible = visibility(parts)?;
    let mut out = Vec::new();
    for (p, rows) in parts.iter().zip(&visible.rows) {
        let listed = p.ids()?;
        for &row in rows {
            out.push(
                listed
                    .get(row)
                    .ok_or_else(|| anyhow::anyhow!("visible row {row} is outside its part"))?
                    .clone(),
            );
        }
    }
    out.sort();
    Ok(out)
}
