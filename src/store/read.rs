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
use std::collections::{BTreeMap, HashSet};
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

/// A live row resolved by the range merge, from an immutable part or its newer memtable overlay.
///
/// An immutable part index is stable for the lifetime of the `Store`/`ReadStore` borrow that produced
/// it. Carrying the origin into projection avoids repeating a newest-first point lookup for every
/// candidate and again for every reconstructed content value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowOrigin {
    Part { part: usize, row: usize },
    Memtable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowRef {
    pub id: String,
    pub origin: RowOrigin,
}

/// Work performed by one committed range-resolution call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RowBatch {
    pub rows: Vec<RowRef>,
    /// Immutable row occurrences participating in resolved id groups.
    pub physical_rows: usize,
    /// Older occurrences hidden by the deciding occurrence in their id group.
    pub superseded_rows: usize,
    /// Deciding occurrences that were tombstones and produced no live candidate.
    pub tombstones: usize,
    /// Newer memtable occurrences consumed by the merge.
    pub memtable_entries: usize,
    /// Last complete id group consumed, including a group decided by a tombstone.
    pub resolved_through: Option<String>,
    /// At least one source still has an id beyond `resolved_through` in the requested direction.
    pub has_more: bool,
    /// Resolution stopped because admitting the next complete id group would cross its work ceiling.
    pub budget_exhausted: bool,
}

pub(crate) fn part_may_match(part: &Part, predicates: &[crate::scan::Predicate]) -> Result<bool> {
    for predicate in predicates {
        let possible = match predicate {
            crate::scan::Predicate::Attr { name, op, value } => {
                part.attr_predicate_may_match(name, *op, value)?
            }
            crate::scan::Predicate::AttrExists { name, present: true } => {
                part.has_attribute_name(name)?
            }
            crate::scan::Predicate::ContentExists { name, present: true } => {
                part.has_content_name(name)?
            }
            // Absence and id predicates cannot be disproved by part-wide field metadata.
            _ => true,
        };
        if !possible {
            return Ok(false);
        }
    }
    Ok(true)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RowScan<'a> {
    pub from: Option<&'a str>,
    pub to: Option<&'a str>,
    pub limit: usize,
    pub reverse: bool,
    pub max_resolution_entries: usize,
    pub allow_oversized_group: bool,
}

/// Exact immutable physical scope for a prepared id range, before visibility resolution.
pub(crate) fn scan_physical_scope(
    parts: &[Arc<Part>],
    from: Option<&str>,
    to: Option<&str>,
) -> Result<crate::scan::ScanPhysicalScope> {
    let mut scope = crate::scan::ScanPhysicalScope {
        immutable_parts_considered: parts.len(),
        ..Default::default()
    };
    for part in parts {
        let rows = part.rows_in_range(from, to)?;
        if !rows.is_empty() {
            scope.immutable_parts_with_rows += 1;
        }
        scope.immutable_rows_in_bounds = scope
            .immutable_rows_in_bounds
            .checked_add(rows.len())
            .ok_or_else(|| anyhow::anyhow!("scan explanation physical-row count overflow"))?;
    }
    Ok(scope)
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

/// Column-selective projection for already resolved immutable rows.
///
/// Rows are grouped by physical part for decoder sharing, then restored to caller order. This is
/// deliberately below both `Store` and `ReadStore`, so writer-backed and immutable snapshots cannot
/// drift into different physical scan behavior.
pub(crate) fn project_rows(
    parts: &[Arc<Part>],
    resolved: &[&RowRef],
    attrs: &HashSet<&str>,
    contents: &HashSet<&str>,
) -> Result<Vec<Record>> {
    let mut grouped: BTreeMap<usize, Vec<(usize, usize, &RowRef)>> = BTreeMap::new();
    for (output, resolved) in resolved.iter().enumerate() {
        let RowOrigin::Part { part, row } = resolved.origin else {
            anyhow::bail!("committed projection received a memtable row")
        };
        grouped.entry(part).or_default().push((output, row, resolved));
    }

    let mut out: Vec<Option<Record>> = vec![None; resolved.len()];
    for (part_index, group) in grouped {
        let part = parts.get(part_index).ok_or_else(|| {
            anyhow::anyhow!("resolved part {part_index} is outside the immutable snapshot")
        })?;
        let rows: Vec<usize> = group.iter().map(|(_, row, _)| *row).collect();
        for &row in &rows {
            if row >= part.len() {
                anyhow::bail!(
                    "resolved row {row} is outside part {part_index} with {} rows",
                    part.len()
                );
            }
        }
        let projected_attrs = part.attrs_selected_many(&rows, attrs)?;
        let projected_contents = part.contents_selected_many(&rows, contents)?;
        for (((output, _, resolved), attrs), contents) in
            group.into_iter().zip(projected_attrs).zip(projected_contents)
        {
            out[output] = Some(Record { id: resolved.id.clone(), contents, attrs });
        }
    }
    out.into_iter()
        .map(|record| record.ok_or_else(|| anyhow::anyhow!("projected row was not produced")))
        .collect()
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

/// Byte-exact content whose program was projected from an already resolved live row.
pub(crate) fn reconstruct_projected_content(
    parts: &[Arc<Part>],
    fold: &Fold,
    resolved: &RowRef,
    content: &crate::types::Content,
) -> Result<Vec<u8>> {
    let RowOrigin::Part { part: part_index, row } = resolved.origin else {
        anyhow::bail!("committed reconstruction received a memtable row")
    };
    let part = parts.get(part_index).ok_or_else(|| {
        anyhow::anyhow!("resolved part {part_index} is outside the immutable snapshot")
    })?;
    if row >= part.len() {
        anyhow::bail!(
            "resolved row {} is outside part {} with {} rows",
            row,
            part_index,
            part.len()
        );
    }
    part.reconstruct_projected_content(content, fold)
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
/// holding it is not a tombstone. Resolution happens per candidate id rather than by materializing
/// every part's visibility. The result is live-row bounded; tombstones and repeated versions may
/// require more physical rows, which [`RowBatch`] reports exactly.
///
/// Because ids sort lexicographically, a caller who designs ids with the query in mind — a
/// `member/timestamp/...` prefix, say — gets member-then-time paging out of this with no secondary
/// index at all. `reverse` walks the same run backwards, which is what a newest-first UI wants.
pub(crate) fn scan_rows<'a, I>(
    parts: &[Arc<Part>],
    mut overlay: I,
    request: RowScan<'_>,
) -> Result<RowBatch>
where
    I: Iterator<Item = (&'a str, bool)>,
{
    if request.limit == 0 {
        return Ok(RowBatch::default());
    }
    struct Walk {
        range: std::ops::Range<usize>,
        row: usize,
        current: Option<String>,
    }

    let mut walks = Vec::with_capacity(parts.len());
    for part in parts {
        let range = part.rows_in_range(request.from, request.to)?;
        let row = if request.reverse { range.end.saturating_sub(1) } else { range.start };
        let current = if range.is_empty() { None } else { Some(part.id(row)?) };
        walks.push(Walk { range, row, current });
    }

    let mut overlay_current = overlay.next();

    // K-way walk over the already-sorted ranges plus the optional newer overlay. Equal ids are
    // resolved as one atomic group; the overlay decides when present, otherwise the newest part does.
    // Crucially, the walk stops once `limit` live ids are found instead of materialising the range.
    let mut batch = RowBatch { rows: Vec::with_capacity(request.limit), ..RowBatch::default() };
    while batch.rows.len() < request.limit {
        let part_best = walks
            .iter()
            .filter_map(|walk| walk.current.as_ref())
            .min_by(|a, b| if request.reverse { b.cmp(a) } else { a.cmp(b) })
            .cloned();
        let best = match (part_best, overlay_current.as_ref().map(|(id, _)| *id)) {
            (Some(part), Some(overlay)) => {
                let overlay_wins =
                    if request.reverse { overlay > part.as_str() } else { overlay < part.as_str() };
                if overlay_wins {
                    overlay.to_owned()
                } else {
                    part
                }
            }
            (Some(part), None) => part,
            (None, Some(overlay)) => overlay.to_owned(),
            (None, None) => break,
        };
        let matching: Vec<usize> = walks
            .iter()
            .enumerate()
            .filter_map(|(pi, walk)| (walk.current.as_deref() == Some(best.as_str())).then_some(pi))
            .collect();
        let overlay_matches = overlay_current.is_some_and(|(id, _)| id == best);
        let group_entries = matching.len() + usize::from(overlay_matches);
        let used = batch
            .physical_rows
            .checked_add(batch.memtable_entries)
            .ok_or_else(|| anyhow::anyhow!("total row-resolution counter overflow"))?;
        if group_entries > request.max_resolution_entries.saturating_sub(used)
            && (used > 0 || !request.allow_oversized_group)
        {
            batch.has_more = true;
            batch.budget_exhausted = true;
            break;
        }
        batch.physical_rows = batch
            .physical_rows
            .checked_add(matching.len())
            .ok_or_else(|| anyhow::anyhow!("physical row-resolution counter overflow"))?;
        if overlay_matches {
            batch.memtable_entries = batch
                .memtable_entries
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("memtable row-resolution counter overflow"))?;
            batch.superseded_rows = batch
                .superseded_rows
                .checked_add(matching.len())
                .ok_or_else(|| anyhow::anyhow!("superseded row-resolution counter overflow"))?;
            if overlay_current.expect("the overlay id matched").1 {
                batch.rows.push(RowRef { id: best.clone(), origin: RowOrigin::Memtable });
            }
        } else {
            batch.superseded_rows = batch
                .superseded_rows
                .checked_add(matching.len().saturating_sub(1))
                .ok_or_else(|| anyhow::anyhow!("superseded row-resolution counter overflow"))?;
            let newest = *matching.last().expect("the selected id has at least one source part");
            if !parts[newest].is_tombstone(walks[newest].row)? {
                batch.rows.push(RowRef {
                    id: best.clone(),
                    origin: RowOrigin::Part { part: newest, row: walks[newest].row },
                });
            } else {
                batch.tombstones = batch
                    .tombstones
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("tombstone row-resolution counter overflow"))?;
            }
        }
        for pi in matching {
            let walk = &mut walks[pi];
            if request.reverse {
                if walk.row == walk.range.start {
                    walk.current = None;
                } else {
                    walk.row -= 1;
                    walk.current = Some(parts[pi].id(walk.row)?);
                }
            } else {
                walk.row += 1;
                if walk.row >= walk.range.end {
                    walk.current = None;
                } else {
                    walk.current = Some(parts[pi].id(walk.row)?);
                }
            }
        }
        if overlay_matches {
            overlay_current = overlay.next();
        }
        batch.resolved_through = Some(best);
        let sources_remain =
            overlay_current.is_some() || walks.iter().any(|walk| walk.current.is_some());
        let used = batch
            .physical_rows
            .checked_add(batch.memtable_entries)
            .ok_or_else(|| anyhow::anyhow!("total row-resolution counter overflow"))?;
        if sources_remain && used >= request.max_resolution_entries {
            batch.has_more = true;
            batch.budget_exhausted = true;
            break;
        }
    }
    if !batch.has_more {
        batch.has_more =
            overlay_current.is_some() || walks.iter().any(|walk| walk.current.is_some());
    }
    Ok(batch)
}

/// Id-only projection of the same resolved-row range merge used by structured scans.
pub fn scan_ids(
    parts: &[Arc<Part>],
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
    reverse: bool,
) -> Result<Vec<String>> {
    Ok(scan_rows(
        parts,
        std::iter::empty::<(&str, bool)>(),
        RowScan {
            from,
            to,
            limit,
            reverse,
            max_resolution_entries: usize::MAX,
            allow_oversized_group: true,
        },
    )?
    .rows
    .into_iter()
    .map(|row| row.id)
    .collect())
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
