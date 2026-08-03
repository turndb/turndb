//! Storage-native structured paging.
//!
//! This is the small-query interface a binding can call without SQL or Arrow. It deliberately owns
//! cursor construction, visibility, exact value comparison, and work bounds in Rust. The first stable
//! ordering is record id; additional orderings need indexes rather than JavaScript-side sorting.

use crate::store::{ReadStore, Store};
use crate::types::{AttrValue, Content, ContentHash, Record};
use anyhow::{bail, Context, Result};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::time::Instant;

pub use crate::control::{CancellationToken, InterruptionReason as ScanInterruptionReason};

const CURSOR_VERSION: u8 = 1;
const CURSOR_FINGERPRINT: usize = 16;
const CURSOR_CHECKSUM: usize = 8;
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 10_000;
pub const MAX_EXAMINED: usize = 1_000_000;
/// Default ceiling for immutable row occurrences plus memtable entries resolved by one page.
pub const DEFAULT_MAX_RESOLUTION_ENTRIES: usize = 1_000_000;
pub const MAX_RESOLUTION_ENTRIES: usize = 10_000_000;
/// Default ceiling for content bytes materialized into one structured page.
pub const DEFAULT_MAX_RECONSTRUCTED_BYTES: u64 = 32 << 20;

/// A scan stopped by its cooperative token or absolute deadline. No partial page is returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanInterrupted {
    pub reason: ScanInterruptionReason,
}

impl std::fmt::Display for ScanInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            ScanInterruptionReason::Cancelled => f.write_str("scan was cancelled"),
            ScanInterruptionReason::DeadlineExceeded => f.write_str("scan deadline exceeded"),
        }
    }
}

impl std::error::Error for ScanInterrupted {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    Forward,
    Reverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compare {
    Eq,
    Ne,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

#[derive(Clone, Debug)]
pub enum Predicate {
    Id { op: Compare, value: String },
    Attr { name: String, op: Compare, value: AttrValue },
    AttrExists { name: String, present: bool },
    ContentExists { name: String, present: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentMode {
    Metadata,
    Bytes,
}

#[derive(Clone, Debug)]
pub struct ContentSelect {
    pub name: String,
    pub mode: ContentMode,
}

#[derive(Clone, Debug)]
pub struct ScanRequest {
    /// Inclusive id lower bound.
    pub from: Option<String>,
    /// Exclusive id upper bound.
    pub to: Option<String>,
    pub direction: Direction,
    /// Opaque continuation returned by an earlier request with the same range, direction, and
    /// predicates. Projection and page size may change between pages.
    pub cursor: Option<String>,
    pub limit: usize,
    /// Hard bound on candidate records examined during this call. A partial page carries a cursor.
    pub max_examined: usize,
    /// Ceiling for pre-predicate newest-wins work. Complete equal-id groups are atomic; the first
    /// group may exceed this ceiling so pagination always makes progress.
    pub max_resolution_entries: usize,
    /// Ceiling for content bytes reconstructed into this page. A row is never split or truncated;
    /// the first matching row is admitted even when it alone exceeds this value so paging can make
    /// progress.
    pub max_reconstructed_bytes: u64,
    /// Absolute deadline. It may be created before queue submission so queue time is included.
    pub deadline: Option<Instant>,
    /// Cooperative cancellation checked before and during record/content evaluation.
    pub cancellation: Option<CancellationToken>,
    /// Attribute keys to return. Matching occurrences preserve their original order and duplicates.
    pub attrs: Vec<String>,
    /// Named content values to describe or reconstruct.
    pub contents: Vec<ContentSelect>,
    pub predicates: Vec<Predicate>,
}

/// One named-content projection in a prepared structured scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanContentPlan {
    pub name: String,
    pub mode: ContentMode,
}

/// Exact physical range scope before newest-wins resolution or predicate evaluation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanPhysicalScope {
    /// Immutable parts whose id range is initialized. Empty effective ranges initialize none.
    pub immutable_parts_considered: usize,
    /// Considered parts with at least one physical row inside the effective bounds.
    pub immutable_parts_with_rows: usize,
    /// Immutable row occurrences inside the bounds, including superseded rows and tombstones.
    pub immutable_rows_in_bounds: usize,
    /// Writer-memtable puts and deletes inside the bounds. Always zero for immutable snapshots.
    pub memtable_entries_in_bounds: usize,
}

/// Rust-owned explanation of the storage-native structured scan plan.
///
/// This reports what the engine can know before newest-wins resolution and predicate evaluation. It
/// deliberately does not estimate result cardinality or claim that semantic predicates are indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanExplanation {
    pub direction: Direction,
    pub uses_cursor: bool,
    pub effective_from: Option<String>,
    pub effective_to: Option<String>,
    pub empty_range: bool,
    pub projected_attrs: Vec<String>,
    pub required_attrs: Vec<String>,
    pub predicate_only_attrs: Vec<String>,
    pub projected_contents: Vec<ScanContentPlan>,
    pub required_contents: Vec<String>,
    pub predicate_only_contents: Vec<String>,
    pub reconstructed_contents: Vec<String>,
    pub id_predicates: usize,
    pub attr_predicates: usize,
    pub content_predicates: usize,
    pub limit: usize,
    pub max_examined: usize,
    pub max_resolution_entries: usize,
    pub max_reconstructed_bytes: u64,
    pub physical: ScanPhysicalScope,
}

impl Default for ScanRequest {
    fn default() -> Self {
        ScanRequest {
            from: None,
            to: None,
            direction: Direction::Forward,
            cursor: None,
            limit: DEFAULT_LIMIT,
            max_examined: 10_000,
            max_resolution_entries: DEFAULT_MAX_RESOLUTION_ENTRIES,
            max_reconstructed_bytes: DEFAULT_MAX_RECONSTRUCTED_BYTES,
            deadline: None,
            cancellation: None,
            attrs: Vec::new(),
            contents: Vec::new(),
            predicates: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedContent {
    pub name: String,
    pub present: bool,
    pub len: Option<u64>,
    pub pieces: Option<usize>,
    /// BLAKE3 of the exact reconstructed bytes when carried by the record's format. This remains
    /// unavailable for legacy values rather than substituting a program or piece identity.
    pub identity: Option<ContentHash>,
    /// `Some` only for a present value selected with [`ContentMode::Bytes`].
    pub bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScanRow {
    pub id: String,
    pub attrs: Vec<(String, AttrValue)>,
    pub contents: Vec<ProjectedContent>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanIoStats {
    /// Distinct `(part, section)` pairs opened through the raw-section cache during this page.
    pub part_sections_touched: usize,
    /// Raw-section cache accesses served without part-file I/O.
    pub part_section_cache_hits: u64,
    /// Raw-section cache accesses that read and decoded a part-file section.
    pub part_section_cache_misses: u64,
    /// Compressed part section bytes physically read on this page's cache misses.
    pub part_stored_bytes_read: u64,
    /// Uncompressed part section bytes produced on this page's cache misses.
    pub part_raw_bytes_decoded: u64,
    /// Distinct fold block ids containing pieces consulted during content reconstruction.
    pub fold_blocks_touched: usize,
    /// Stored fold block accesses served from the decompressed block cache.
    pub fold_block_cache_hits: u64,
    /// Stored fold block accesses that read and decoded a frame.
    pub fold_block_cache_misses: u64,
    /// Complete fold frame bytes physically read on this page's cache misses.
    pub fold_stored_bytes_read: u64,
    /// Uncompressed fold block bytes produced on this page's cache misses.
    pub fold_raw_bytes_decoded: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanResolutionStats {
    /// Immutable part-row occurrences consumed while resolving newest-wins candidates.
    pub physical_rows: usize,
    /// Older immutable occurrences hidden by the deciding occurrence for the same id.
    pub superseded_rows: usize,
    /// Newest immutable occurrences that were tombstones and yielded no candidate.
    pub tombstones: usize,
    /// Ordered live-writer memtable entries inspected while overlaying committed candidates.
    pub memtable_entries: usize,
    /// The page stopped at a complete id-group boundary because the resolution ceiling was reached.
    pub budget_exhausted: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub examined: usize,
    pub returned: usize,
    pub shadowed_attr_occurrences: usize,
    pub content_values_reconstructed: usize,
    pub reconstructed_bytes: u64,
    /// A matching row was deliberately left for the next page because adding all of its selected
    /// content bytes would have crossed the request's reconstruction ceiling.
    pub reconstruction_budget_exhausted: bool,
    /// Exact operation-local storage reads. Shared cache activity from other scans is excluded.
    pub io: ScanIoStats,
    /// Exact work performed before predicate evaluation to establish live candidate rows.
    pub resolution: ScanResolutionStats,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScanPage {
    pub rows: Vec<ScanRow>,
    pub next: Option<String>,
    pub stats: ScanStats,
}

/// A live row whose newest-wins origin was settled by the bounded range merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScanCandidate {
    Committed(crate::store::read::RowRef),
    Memtable(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CandidateBatch {
    pub candidates: Vec<ScanCandidate>,
    pub resolution: ScanResolutionStats,
    pub resolved_through: Option<String>,
    pub has_more: bool,
}

impl ScanCandidate {
    pub(crate) fn id(&self) -> &str {
        match self {
            ScanCandidate::Committed(row) => &row.id,
            ScanCandidate::Memtable(id) => id,
        }
    }

    pub(crate) fn into_id(self) -> String {
        match self {
            ScanCandidate::Committed(row) => row.id,
            ScanCandidate::Memtable(id) => id,
        }
    }
}

trait Source {
    fn scan_candidates(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        reverse: bool,
        max_resolution_entries: usize,
        allow_oversized_group: bool,
    ) -> Result<CandidateBatch>;
    fn project_batch(
        &self,
        candidates: &[ScanCandidate],
        attrs: &HashSet<&str>,
        contents: &HashSet<&str>,
    ) -> Result<Vec<Record>>;
    fn reconstruct_content(&self, candidate: &ScanCandidate, content: &Content) -> Result<Vec<u8>>;
    fn physical_scope(&self, from: Option<&str>, to: Option<&str>) -> Result<ScanPhysicalScope>;
}

impl Source for Store {
    fn scan_candidates(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        reverse: bool,
        max_resolution_entries: usize,
        allow_oversized_group: bool,
    ) -> Result<CandidateBatch> {
        Store::scan_candidates(
            self,
            from,
            to,
            limit,
            reverse,
            max_resolution_entries,
            allow_oversized_group,
        )
    }

    fn project_batch(
        &self,
        candidates: &[ScanCandidate],
        attrs: &HashSet<&str>,
        contents: &HashSet<&str>,
    ) -> Result<Vec<Record>> {
        Store::project_candidates(self, candidates, attrs, contents)
    }

    fn reconstruct_content(&self, candidate: &ScanCandidate, content: &Content) -> Result<Vec<u8>> {
        Store::reconstruct_candidate_content(self, candidate, content)
    }

    fn physical_scope(&self, from: Option<&str>, to: Option<&str>) -> Result<ScanPhysicalScope> {
        Store::scan_physical_scope(self, from, to)
    }
}

impl Source for ReadStore {
    fn scan_candidates(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        reverse: bool,
        max_resolution_entries: usize,
        allow_oversized_group: bool,
    ) -> Result<CandidateBatch> {
        ReadStore::scan_candidates(
            self,
            from,
            to,
            limit,
            reverse,
            max_resolution_entries,
            allow_oversized_group,
        )
    }

    fn project_batch(
        &self,
        candidates: &[ScanCandidate],
        attrs: &HashSet<&str>,
        contents: &HashSet<&str>,
    ) -> Result<Vec<Record>> {
        ReadStore::project_candidates(self, candidates, attrs, contents)
    }

    fn reconstruct_content(&self, candidate: &ScanCandidate, content: &Content) -> Result<Vec<u8>> {
        ReadStore::reconstruct_candidate_content(self, candidate, content)
    }

    fn physical_scope(&self, from: Option<&str>, to: Option<&str>) -> Result<ScanPhysicalScope> {
        ReadStore::scan_physical_scope(self, from, to)
    }
}

pub(crate) fn scan_store(store: &Store, request: &ScanRequest) -> Result<ScanPage> {
    scan_source(store, request)
}

pub(crate) fn scan_read_store(store: &ReadStore, request: &ScanRequest) -> Result<ScanPage> {
    scan_source(store, request)
}

pub(crate) fn explain_store(store: &Store, request: &ScanRequest) -> Result<ScanExplanation> {
    explain_source(store, request)
}

pub(crate) fn explain_read_store(
    store: &ReadStore,
    request: &ScanRequest,
) -> Result<ScanExplanation> {
    explain_source(store, request)
}

struct PreparedScan<'a> {
    fingerprint: [u8; CURSOR_FINGERPRINT],
    cursor_last_id: Option<String>,
    from: Option<String>,
    to: Option<String>,
    attr_select: HashSet<&'a str>,
    attr_needed: HashSet<&'a str>,
    content_needed: HashSet<&'a str>,
}

fn prepare(request: &ScanRequest) -> Result<PreparedScan<'_>> {
    validate(request)?;
    check_interruption(request)?;
    let fingerprint = fingerprint(request);
    let cursor = request
        .cursor
        .as_deref()
        .map(decode_cursor)
        .transpose()?
        .map(|cursor| {
            if cursor.direction != request.direction {
                bail!("scan cursor direction does not match this request");
            }
            if cursor.fingerprint != fingerprint {
                bail!("scan cursor belongs to different bounds or predicates");
            }
            Ok(cursor)
        })
        .transpose()?;

    let mut from = request.from.clone();
    let mut to = request.to.clone();
    if let Some(cursor) = &cursor {
        match request.direction {
            Direction::Forward => from = Some(after(&cursor.last_id)),
            Direction::Reverse => to = Some(cursor.last_id.clone()),
        }
    }
    let attr_select: HashSet<&str> = request.attrs.iter().map(String::as_str).collect();
    let mut attr_needed = attr_select.clone();
    let mut content_needed: HashSet<&str> =
        request.contents.iter().map(|content| content.name.as_str()).collect();
    for predicate in &request.predicates {
        match predicate {
            Predicate::Attr { name, .. } | Predicate::AttrExists { name, .. } => {
                attr_needed.insert(name);
            }
            Predicate::ContentExists { name, .. } => {
                content_needed.insert(name);
            }
            Predicate::Id { .. } => {}
        }
    }
    Ok(PreparedScan {
        fingerprint,
        cursor_last_id: cursor.map(|cursor| cursor.last_id),
        from,
        to,
        attr_select,
        attr_needed,
        content_needed,
    })
}

fn explain_source(source: &dyn Source, request: &ScanRequest) -> Result<ScanExplanation> {
    let prepared = prepare(request)?;
    let empty_range = matches!(
        (&prepared.from, &prepared.to),
        (Some(from), Some(to)) if from >= to
    );
    let physical = if empty_range {
        ScanPhysicalScope::default()
    } else {
        source.physical_scope(prepared.from.as_deref(), prepared.to.as_deref())?
    };
    check_interruption(request)?;

    let mut projected_attrs: Vec<String> =
        prepared.attr_select.iter().map(|name| (*name).to_string()).collect();
    projected_attrs.sort();
    let mut required_attrs: Vec<String> =
        prepared.attr_needed.iter().map(|name| (*name).to_string()).collect();
    required_attrs.sort();
    let mut predicate_only_attrs: Vec<String> = prepared
        .attr_needed
        .difference(&prepared.attr_select)
        .map(|name| (*name).to_string())
        .collect();
    predicate_only_attrs.sort();

    let projected_contents: Vec<ScanContentPlan> = request
        .contents
        .iter()
        .map(|content| ScanContentPlan { name: content.name.clone(), mode: content.mode })
        .collect();
    let projected_content_names: HashSet<&str> =
        request.contents.iter().map(|content| content.name.as_str()).collect();
    let mut required_contents: Vec<String> =
        prepared.content_needed.iter().map(|name| (*name).to_string()).collect();
    required_contents.sort();
    let mut predicate_only_contents: Vec<String> = prepared
        .content_needed
        .difference(&projected_content_names)
        .map(|name| (*name).to_string())
        .collect();
    predicate_only_contents.sort();
    let reconstructed_contents = request
        .contents
        .iter()
        .filter(|content| content.mode == ContentMode::Bytes)
        .map(|content| content.name.clone())
        .collect();

    let (mut id_predicates, mut attr_predicates, mut content_predicates) = (0, 0, 0);
    for predicate in &request.predicates {
        match predicate {
            Predicate::Id { .. } => id_predicates += 1,
            Predicate::Attr { .. } | Predicate::AttrExists { .. } => attr_predicates += 1,
            Predicate::ContentExists { .. } => content_predicates += 1,
        }
    }

    Ok(ScanExplanation {
        direction: request.direction,
        uses_cursor: request.cursor.is_some(),
        effective_from: prepared.from,
        effective_to: prepared.to,
        empty_range,
        projected_attrs,
        required_attrs,
        predicate_only_attrs,
        projected_contents,
        required_contents,
        predicate_only_contents,
        reconstructed_contents,
        id_predicates,
        attr_predicates,
        content_predicates,
        limit: request.limit,
        max_examined: request.max_examined,
        max_resolution_entries: request.max_resolution_entries,
        max_reconstructed_bytes: request.max_reconstructed_bytes,
        physical,
    })
}

fn scan_source(source: &dyn Source, request: &ScanRequest) -> Result<ScanPage> {
    let prepared = prepare(request)?;
    let read_trace = crate::io_trace::ReadTraceScope::start();
    let PreparedScan {
        fingerprint,
        cursor_last_id,
        mut from,
        mut to,
        attr_select,
        attr_needed,
        content_needed,
    } = prepared;
    if matches!((&from, &to), (Some(a), Some(b)) if a >= b) {
        return Ok(ScanPage { rows: Vec::new(), next: None, stats: ScanStats::default() });
    }
    let needs_record = !attr_needed.is_empty() || !content_needed.is_empty();
    let mut rows = Vec::with_capacity(request.limit);
    let mut stats = ScanStats::default();
    // A cursor identifies the last complete id group CONSUMED, not merely inspected. It may be a
    // tombstone-only group. A row deferred by the reconstruction budget remains unconsumed and must
    // be reconsidered on the next page.
    let mut last_consumed = cursor_last_id;
    let mut has_more = false;
    let mut budget_stopped = false;

    while rows.len() < request.limit
        && stats.examined < request.max_examined
        && !stats.resolution.budget_exhausted
    {
        check_interruption(request)?;
        let remaining = request.max_examined - stats.examined;
        let ask = remaining.min((request.limit - rows.len()).max(64));
        let resolution_used = stats
            .resolution
            .physical_rows
            .checked_add(stats.resolution.memtable_entries)
            .context("structured scan resolution-entry counter overflow")?;
        let resolution_remaining = request.max_resolution_entries.saturating_sub(resolution_used);
        let batch = source.scan_candidates(
            from.as_deref(),
            to.as_deref(),
            ask,
            request.direction == Direction::Reverse,
            resolution_remaining,
            resolution_used == 0,
        )?;
        check_interruption(request)?;
        let CandidateBatch { candidates, resolution, resolved_through, has_more: batch_has_more } =
            batch;
        stats.resolution.physical_rows = stats
            .resolution
            .physical_rows
            .checked_add(resolution.physical_rows)
            .context("structured scan physical-row counter overflow")?;
        stats.resolution.superseded_rows = stats
            .resolution
            .superseded_rows
            .checked_add(resolution.superseded_rows)
            .context("structured scan superseded-row counter overflow")?;
        stats.resolution.tombstones = stats
            .resolution
            .tombstones
            .checked_add(resolution.tombstones)
            .context("structured scan tombstone counter overflow")?;
        stats.resolution.memtable_entries = stats
            .resolution
            .memtable_entries
            .checked_add(resolution.memtable_entries)
            .context("structured scan memtable-entry counter overflow")?;
        stats.resolution.budget_exhausted |= resolution.budget_exhausted;
        if candidates.is_empty() {
            if let Some(resolved_through) = resolved_through {
                last_consumed = Some(resolved_through);
            }
            has_more = batch_has_more;
            break;
        }
        let fetched = candidates.len();
        let mut processed = 0usize;
        // Never project beyond remaining output demand. Every gathered candidate will therefore be
        // semantically examined before a full page can stop; corruption or cancellation cannot leak
        // in from a read-ahead row that the page otherwise would not have reached. Rejections cause
        // another bounded gather rather than speculative decoder work.
        let projection_batch_size = (request.limit - rows.len()).clamp(1, 64);
        'candidate_batches: for candidate_batch in candidates.chunks(projection_batch_size) {
            check_interruption(request)?;
            let projected = if needs_record {
                Some(source.project_batch(candidate_batch, &attr_needed, &content_needed)?)
            } else {
                None
            };
            if projected.as_ref().is_some_and(|records| records.len() != candidate_batch.len()) {
                bail!("scan source returned the wrong number of projected rows");
            }
            for (candidate_index, candidate) in candidate_batch.iter().enumerate() {
                check_interruption(request)?;
                processed += 1;
                stats.examined += 1;
                let record = projected.as_ref().map(|records| &records[candidate_index]);
                if !request.predicates.iter().all(|p| matches_predicate(candidate.id(), record, p))
                {
                    last_consumed = Some(candidate.id().to_string());
                    continue;
                }
                let row_reconstructed_bytes = projected_reconstructed_bytes(record, request)?;
                if !rows.is_empty()
                    && row_reconstructed_bytes
                        > request.max_reconstructed_bytes.saturating_sub(stats.reconstructed_bytes)
                {
                    // Do not consume `id`: the continuation must resume at this matching row. The
                    // row was examined, so it still counts against max_examined for this call.
                    stats.reconstruction_budget_exhausted = true;
                    has_more = true;
                    budget_stopped = true;
                    break 'candidate_batches;
                }
                let mut attrs = Vec::new();
                if let Some(record) = record {
                    attrs.extend(
                        record
                            .attrs
                            .iter()
                            .filter(|(name, _)| attr_select.contains(name.as_str()))
                            .cloned(),
                    );
                    stats.shadowed_attr_occurrences += shadowed_attrs(&attrs);
                }
                let mut contents = Vec::with_capacity(request.contents.len());
                for selected in &request.contents {
                    check_interruption(request)?;
                    let content = record.and_then(|r| r.content(&selected.name));
                    let bytes =
                        if let (Some(content), ContentMode::Bytes) = (content, selected.mode) {
                            let value = source.reconstruct_content(candidate, content)?;
                            check_interruption(request)?;
                            stats.content_values_reconstructed += 1;
                            stats.reconstructed_bytes = stats
                                .reconstructed_bytes
                                .checked_add(value.len() as u64)
                                .context("structured scan reconstructed-byte counter overflow")?;
                            Some(value)
                        } else {
                            None
                        };
                    contents.push(project_content(&selected.name, content, bytes));
                }
                rows.push(ScanRow { id: candidate.id().to_string(), attrs, contents });
                last_consumed = Some(rows.last().expect("row was just pushed").id.clone());
                if rows.len() == request.limit {
                    has_more = processed < fetched || batch_has_more;
                    break 'candidate_batches;
                }
            }
        }
        if processed == fetched && !budget_stopped {
            if let Some(resolved_through) = resolved_through {
                last_consumed = Some(resolved_through);
            }
        }
        if budget_stopped {
            break;
        }
        if rows.len() == request.limit {
            break;
        }
        if stats.examined == request.max_examined {
            has_more = processed < fetched || batch_has_more;
            break;
        }
        if stats.resolution.budget_exhausted {
            has_more = batch_has_more;
            break;
        }
        if !batch_has_more {
            has_more = false;
            break;
        }
        let last = last_consumed.as_ref().expect("a consumed id batch has a last id");
        match request.direction {
            Direction::Forward => from = Some(after(last)),
            Direction::Reverse => to = Some(last.clone()),
        }
        has_more = true;
    }

    check_interruption(request)?;
    stats.returned = rows.len();
    let trace = read_trace.finish();
    stats.io = ScanIoStats {
        part_sections_touched: trace.part_sections_touched(),
        part_section_cache_hits: trace.part_section_cache_hits,
        part_section_cache_misses: trace.part_section_cache_misses,
        part_stored_bytes_read: trace.part_stored_bytes_read,
        part_raw_bytes_decoded: trace.part_raw_bytes_decoded,
        fold_blocks_touched: trace.fold_blocks_touched(),
        fold_block_cache_hits: trace.fold_block_cache_hits,
        fold_block_cache_misses: trace.fold_block_cache_misses,
        fold_stored_bytes_read: trace.fold_stored_bytes_read,
        fold_raw_bytes_decoded: trace.fold_raw_bytes_decoded,
    };
    let next = if has_more {
        last_consumed
            .map(|last| encode_cursor(request.direction, fingerprint, &last))
            .transpose()?
    } else {
        None
    };
    Ok(ScanPage { rows, next, stats })
}

fn check_interruption(request: &ScanRequest) -> Result<()> {
    if let Some(reason) =
        crate::control::interruption_reason(request.deadline, request.cancellation.as_ref())
    {
        return Err(ScanInterrupted { reason }.into());
    }
    Ok(())
}

fn validate(request: &ScanRequest) -> Result<()> {
    if request.limit == 0 || request.limit > MAX_LIMIT {
        bail!("scan limit must be in 1..={MAX_LIMIT}");
    }
    if request.max_examined == 0 || request.max_examined > MAX_EXAMINED {
        bail!("scan max_examined must be in 1..={MAX_EXAMINED}");
    }
    if request.max_resolution_entries == 0
        || request.max_resolution_entries > MAX_RESOLUTION_ENTRIES
    {
        bail!("scan max_resolution_entries must be in 1..={MAX_RESOLUTION_ENTRIES}");
    }
    if request.max_reconstructed_bytes == 0 {
        bail!("scan max_reconstructed_bytes must be greater than zero");
    }
    if matches!((&request.from, &request.to), (Some(a), Some(b)) if a >= b) {
        bail!("scan lower bound must be less than its upper bound");
    }
    let mut contents = HashSet::new();
    for selected in &request.contents {
        if selected.name.is_empty() {
            bail!("selected content name must not be empty");
        }
        if !contents.insert(selected.name.as_str()) {
            bail!("content {:?} is selected more than once", selected.name);
        }
    }
    for predicate in &request.predicates {
        match predicate {
            Predicate::Attr { name, .. }
            | Predicate::AttrExists { name, .. }
            | Predicate::ContentExists { name, .. }
                if name.is_empty() =>
            {
                bail!("predicate field name must not be empty")
            }
            _ => {}
        }
    }
    Ok(())
}

fn projected_reconstructed_bytes(record: Option<&Record>, request: &ScanRequest) -> Result<u64> {
    request
        .contents
        .iter()
        .filter(|selected| selected.mode == ContentMode::Bytes)
        .filter_map(|selected| record.and_then(|record| record.content(&selected.name)))
        .try_fold(0u64, |bytes, content| {
            bytes.checked_add(content.len()).ok_or_else(|| {
                anyhow::anyhow!("selected content lengths overflow the u64 scan byte counter")
            })
        })
}

fn project_content(
    name: &str,
    content: Option<&Content>,
    bytes: Option<Vec<u8>>,
) -> ProjectedContent {
    ProjectedContent {
        name: name.to_string(),
        present: content.is_some(),
        len: content.map(Content::len),
        pieces: content.map(|c| {
            c.ops.iter().filter(|op| matches!(op, crate::types::ContentOp::Piece { .. })).count()
        }),
        identity: content.and_then(|content| content.identity),
        bytes,
    }
}

fn shadowed_attrs(attrs: &[(String, AttrValue)]) -> usize {
    let mut seen = HashSet::new();
    attrs.iter().filter(|(name, value)| !seen.insert((name.as_str(), value.type_tag()))).count()
}

fn matches_predicate(id: &str, record: Option<&Record>, predicate: &Predicate) -> bool {
    match predicate {
        Predicate::Id { op, value } => compare_order(id.as_bytes().cmp(value.as_bytes()), *op),
        Predicate::Attr { name, op, value } => record
            .and_then(|r| {
                r.attrs.iter().find(|(key, candidate)| {
                    key == name && candidate.type_tag() == value.type_tag()
                })
            })
            .is_some_and(|(_, candidate)| compare_attr(candidate, value, *op)),
        Predicate::AttrExists { name, present } => {
            record.is_some_and(|r| r.attrs.iter().any(|(key, _)| key == name)) == *present
        }
        Predicate::ContentExists { name, present } => {
            record.is_some_and(|r| r.content(name).is_some()) == *present
        }
    }
}

fn compare_attr(a: &AttrValue, b: &AttrValue, op: Compare) -> bool {
    match (a, b) {
        (AttrValue::Str(a), AttrValue::Str(b)) => compare_order(a.as_bytes().cmp(b.as_bytes()), op),
        (AttrValue::Int(a), AttrValue::Int(b)) => compare_order(a.cmp(b), op),
        (AttrValue::Bool(a), AttrValue::Bool(b)) => compare_order(a.cmp(b), op),
        (AttrValue::UInt(a), AttrValue::UInt(b)) => compare_order(a.cmp(b), op),
        (AttrValue::Bytes(a), AttrValue::Bytes(b)) => compare_order(a.cmp(b), op),
        (AttrValue::TimestampNs(a), AttrValue::TimestampNs(b)) => compare_order(a.cmp(b), op),
        (AttrValue::Null, AttrValue::Null) => op == Compare::Eq,
        (AttrValue::Float(a), AttrValue::Float(b)) => match op {
            Compare::Eq => a.to_bits() == b.to_bits(),
            Compare::Ne => a.to_bits() != b.to_bits(),
            _ => a.partial_cmp(b).is_some_and(|ordering| compare_order(ordering, op)),
        },
        _ => false,
    }
}

fn compare_order(ordering: Ordering, op: Compare) -> bool {
    match op {
        Compare::Eq => ordering == Ordering::Equal,
        Compare::Ne => ordering != Ordering::Equal,
        Compare::Lt => ordering == Ordering::Less,
        Compare::LtEq => ordering != Ordering::Greater,
        Compare::Gt => ordering == Ordering::Greater,
        Compare::GtEq => ordering != Ordering::Less,
    }
}

fn after(id: &str) -> String {
    let mut out = String::with_capacity(id.len() + 1);
    out.push_str(id);
    out.push('\0');
    out
}

#[derive(Debug)]
struct DecodedCursor {
    direction: Direction,
    fingerprint: [u8; CURSOR_FINGERPRINT],
    last_id: String,
}

fn encode_cursor(
    direction: Direction,
    fingerprint: [u8; CURSOR_FINGERPRINT],
    last_id: &str,
) -> Result<String> {
    let id_len =
        u32::try_from(last_id.len()).context("record id is too large for a scan cursor")?;
    let mut payload = Vec::with_capacity(22 + last_id.len());
    payload.push(CURSOR_VERSION);
    payload.push(u8::from(direction == Direction::Reverse));
    payload.extend_from_slice(&fingerprint);
    payload.extend_from_slice(&id_len.to_le_bytes());
    payload.extend_from_slice(last_id.as_bytes());
    let checksum = blake3::hash(&payload);
    payload.extend_from_slice(&checksum.as_bytes()[..CURSOR_CHECKSUM]);
    Ok(hex(&payload))
}

fn decode_cursor(token: &str) -> Result<DecodedCursor> {
    let bytes = unhex(token).context("invalid scan cursor")?;
    let fixed = 2 + CURSOR_FINGERPRINT + 4 + CURSOR_CHECKSUM;
    if bytes.len() < fixed {
        bail!("invalid scan cursor: truncated payload");
    }
    if bytes[0] != CURSOR_VERSION {
        bail!("unsupported scan cursor version {}", bytes[0]);
    }
    let direction = match bytes[1] {
        0 => Direction::Forward,
        1 => Direction::Reverse,
        v => bail!("invalid scan cursor direction {v}"),
    };
    let mut fingerprint = [0u8; CURSOR_FINGERPRINT];
    fingerprint.copy_from_slice(&bytes[2..2 + CURSOR_FINGERPRINT]);
    let at = 2 + CURSOR_FINGERPRINT;
    let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let payload_len = (at + 4)
        .checked_add(len)
        .ok_or_else(|| anyhow::anyhow!("invalid scan cursor length overflow"))?;
    if payload_len + CURSOR_CHECKSUM != bytes.len() {
        bail!("invalid scan cursor length");
    }
    let want = blake3::hash(&bytes[..payload_len]);
    if want.as_bytes()[..CURSOR_CHECKSUM] != bytes[payload_len..] {
        bail!("invalid scan cursor checksum");
    }
    let last_id = String::from_utf8(bytes[at + 4..payload_len].to_vec())
        .context("scan cursor id is not UTF-8")?;
    Ok(DecodedCursor { direction, fingerprint, last_id })
}

fn fingerprint(request: &ScanRequest) -> [u8; CURSOR_FINGERPRINT] {
    let mut h = blake3::Hasher::new();
    h.update(b"turndb-scan-v1");
    hash_optional(&mut h, request.from.as_deref());
    hash_optional(&mut h, request.to.as_deref());
    h.update(&[u8::from(request.direction == Direction::Reverse)]);
    h.update(&(request.predicates.len() as u64).to_le_bytes());
    for predicate in &request.predicates {
        match predicate {
            Predicate::Id { op, value } => {
                h.update(&[0, op_tag(*op)]);
                hash_bytes(&mut h, value.as_bytes());
            }
            Predicate::Attr { name, op, value } => {
                h.update(&[1, op_tag(*op), value.type_tag()]);
                hash_bytes(&mut h, name.as_bytes());
                hash_attr(&mut h, value);
            }
            Predicate::AttrExists { name, present } => {
                h.update(&[2, u8::from(*present)]);
                hash_bytes(&mut h, name.as_bytes());
            }
            Predicate::ContentExists { name, present } => {
                h.update(&[3, u8::from(*present)]);
                hash_bytes(&mut h, name.as_bytes());
            }
        }
    }
    let digest = h.finalize();
    digest.as_bytes()[..CURSOR_FINGERPRINT].try_into().unwrap()
}

fn hash_optional(h: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            h.update(&[1]);
            hash_bytes(h, value.as_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
}

fn hash_bytes(h: &mut blake3::Hasher, value: &[u8]) {
    h.update(&(value.len() as u64).to_le_bytes());
    h.update(value);
}

fn hash_attr(h: &mut blake3::Hasher, value: &AttrValue) {
    match value {
        AttrValue::Str(v) => hash_bytes(h, v.as_bytes()),
        AttrValue::Int(v) => {
            h.update(&v.to_le_bytes());
        }
        AttrValue::Float(v) => {
            h.update(&v.to_bits().to_le_bytes());
        }
        AttrValue::Bool(v) => {
            h.update(&[u8::from(*v)]);
        }
        AttrValue::UInt(v) => {
            h.update(&v.to_le_bytes());
        }
        AttrValue::Bytes(v) => hash_bytes(h, v),
        AttrValue::TimestampNs(v) => {
            h.update(&v.to_le_bytes());
        }
        AttrValue::Null => {}
    }
}

fn op_tag(op: Compare) -> u8 {
    match op {
        Compare::Eq => 0,
        Compare::Ne => 1,
        Compare::Lt => 2,
        Compare::LtEq => 3,
        Compare::Gt => 4,
        Compare::GtEq => 5,
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").unwrap();
    }
    out
}

fn unhex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        bail!("hex token has odd length");
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let s = std::str::from_utf8(pair).context("hex token is not ASCII")?;
            u8::from_str_radix(s, 16).context("hex token contains a non-hex byte")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CancelsAfterFirstRead {
        cancellation: CancellationToken,
    }

    impl Source for CancelsAfterFirstRead {
        fn scan_candidates(
            &self,
            _from: Option<&str>,
            _to: Option<&str>,
            _limit: usize,
            _reverse: bool,
            _max_resolution_entries: usize,
            _allow_oversized_group: bool,
        ) -> Result<CandidateBatch> {
            Ok(CandidateBatch {
                candidates: vec![
                    ScanCandidate::Memtable("a".into()),
                    ScanCandidate::Memtable("b".into()),
                ],
                resolution: ScanResolutionStats { memtable_entries: 2, ..Default::default() },
                ..CandidateBatch::default()
            })
        }

        fn project_batch(
            &self,
            candidates: &[ScanCandidate],
            _attrs: &HashSet<&str>,
            _contents: &HashSet<&str>,
        ) -> Result<Vec<Record>> {
            candidates
                .iter()
                .map(|candidate| {
                    let id = candidate.id();
                    if id == "a" {
                        self.cancellation.cancel();
                    }
                    Record::new(id, vec![], vec![("selected".into(), AttrValue::Bool(true))])
                })
                .collect()
        }

        fn reconstruct_content(
            &self,
            _candidate: &ScanCandidate,
            _content: &Content,
        ) -> Result<Vec<u8>> {
            unreachable!("the interruption test projects no content")
        }

        fn physical_scope(
            &self,
            _from: Option<&str>,
            _to: Option<&str>,
        ) -> Result<ScanPhysicalScope> {
            Ok(ScanPhysicalScope::default())
        }
    }

    #[test]
    fn cancellation_discards_rows_already_built_inside_the_call() {
        let cancellation = CancellationToken::new();
        let source = CancelsAfterFirstRead { cancellation: cancellation.clone() };
        let error = scan_source(
            &source,
            &ScanRequest {
                attrs: vec!["selected".into()],
                cancellation: Some(cancellation),
                ..ScanRequest::default()
            },
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ScanInterrupted>().unwrap().reason,
            ScanInterruptionReason::Cancelled
        );
    }

    #[test]
    fn cursor_roundtrips_arbitrary_valid_ids() {
        let fingerprint = [7u8; CURSOR_FINGERPRINT];
        for id in ["", "plain", "nul\0inside", "astral-\u{10ffff}"] {
            let token = encode_cursor(Direction::Reverse, fingerprint, id).unwrap();
            let got = decode_cursor(&token).unwrap();
            assert_eq!(got.direction, Direction::Reverse);
            assert_eq!(got.fingerprint, fingerprint);
            assert_eq!(got.last_id, id);
        }
    }

    #[test]
    fn malformed_cursor_text_never_panics() {
        for token in ["", "0", "gg", "é", "00é", &"ff".repeat(128)] {
            let _ = decode_cursor(token);
        }
    }
}
