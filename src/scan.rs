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

impl Default for ScanRequest {
    fn default() -> Self {
        ScanRequest {
            from: None,
            to: None,
            direction: Direction::Forward,
            cursor: None,
            limit: DEFAULT_LIMIT,
            max_examined: 10_000,
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
    ) -> Result<Vec<ScanCandidate>>;
    fn project(
        &self,
        candidate: &ScanCandidate,
        attrs: &HashSet<&str>,
        contents: &HashSet<&str>,
    ) -> Result<Record>;
    fn reconstruct_content(&self, candidate: &ScanCandidate, content: &Content) -> Result<Vec<u8>>;
}

impl Source for Store {
    fn scan_candidates(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<ScanCandidate>> {
        Store::scan_candidates(self, from, to, limit, reverse)
    }

    fn project(
        &self,
        candidate: &ScanCandidate,
        attrs: &HashSet<&str>,
        contents: &HashSet<&str>,
    ) -> Result<Record> {
        Store::project_candidate(self, candidate, attrs, contents)
    }

    fn reconstruct_content(&self, candidate: &ScanCandidate, content: &Content) -> Result<Vec<u8>> {
        Store::reconstruct_candidate_content(self, candidate, content)
    }
}

impl Source for ReadStore {
    fn scan_candidates(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<ScanCandidate>> {
        ReadStore::scan_candidates(self, from, to, limit, reverse)
    }

    fn project(
        &self,
        candidate: &ScanCandidate,
        attrs: &HashSet<&str>,
        contents: &HashSet<&str>,
    ) -> Result<Record> {
        ReadStore::project_candidate(self, candidate, attrs, contents)
    }

    fn reconstruct_content(&self, candidate: &ScanCandidate, content: &Content) -> Result<Vec<u8>> {
        ReadStore::reconstruct_candidate_content(self, candidate, content)
    }
}

pub(crate) fn scan_store(store: &Store, request: &ScanRequest) -> Result<ScanPage> {
    scan_source(store, request)
}

pub(crate) fn scan_read_store(store: &ReadStore, request: &ScanRequest) -> Result<ScanPage> {
    scan_source(store, request)
}

fn scan_source(source: &dyn Source, request: &ScanRequest) -> Result<ScanPage> {
    validate(request)?;
    check_interruption(request)?;
    let read_trace = crate::io_trace::ReadTraceScope::start();
    let fingerprint = fingerprint(request);
    let cursor = request
        .cursor
        .as_deref()
        .map(decode_cursor)
        .transpose()?
        .map(|c| {
            if c.direction != request.direction {
                bail!("scan cursor direction does not match this request");
            }
            if c.fingerprint != fingerprint {
                bail!("scan cursor belongs to different bounds or predicates");
            }
            Ok(c)
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
    if matches!((&from, &to), (Some(a), Some(b)) if a >= b) {
        return Ok(ScanPage { rows: Vec::new(), next: None, stats: ScanStats::default() });
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
    let needs_record = !attr_needed.is_empty() || !content_needed.is_empty();
    let mut rows = Vec::with_capacity(request.limit);
    let mut stats = ScanStats::default();
    // A cursor identifies the last candidate CONSUMED, not merely inspected. They normally coincide,
    // but a row deferred by the reconstruction budget must be reconsidered on the next page.
    let mut last_consumed = cursor.as_ref().map(|c| c.last_id.clone());
    let mut has_more = false;
    let mut budget_stopped = false;

    while rows.len() < request.limit && stats.examined < request.max_examined {
        check_interruption(request)?;
        let remaining = request.max_examined - stats.examined;
        let ask = remaining.min((request.limit - rows.len()).max(64));
        let candidates = source.scan_candidates(
            from.as_deref(),
            to.as_deref(),
            ask,
            request.direction == Direction::Reverse,
        )?;
        check_interruption(request)?;
        if candidates.is_empty() {
            has_more = false;
            break;
        }
        let fetched = candidates.len();
        let mut processed = 0usize;
        for candidate in candidates {
            check_interruption(request)?;
            processed += 1;
            stats.examined += 1;
            let record = if needs_record {
                Some(source.project(&candidate, &attr_needed, &content_needed)?)
            } else {
                None
            };
            if !request
                .predicates
                .iter()
                .all(|p| matches_predicate(candidate.id(), record.as_ref(), p))
            {
                last_consumed = Some(candidate.into_id());
                continue;
            }
            let row_reconstructed_bytes = projected_reconstructed_bytes(record.as_ref(), request)?;
            if !rows.is_empty()
                && row_reconstructed_bytes
                    > request.max_reconstructed_bytes.saturating_sub(stats.reconstructed_bytes)
            {
                // Do not consume `id`: the continuation must resume at this matching row. The row
                // was examined, so it still counts against max_examined for this call.
                stats.reconstruction_budget_exhausted = true;
                has_more = true;
                budget_stopped = true;
                break;
            }
            let mut attrs = Vec::new();
            if let Some(record) = &record {
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
                let content = record.as_ref().and_then(|r| r.content(&selected.name));
                let bytes = if let (Some(content), ContentMode::Bytes) = (content, selected.mode) {
                    let value = source.reconstruct_content(&candidate, content)?;
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
            rows.push(ScanRow { id: candidate.into_id(), attrs, contents });
            last_consumed = Some(rows.last().expect("row was just pushed").id.clone());
            if rows.len() == request.limit {
                has_more = processed < fetched || fetched == ask;
                break;
            }
        }
        if budget_stopped {
            break;
        }
        if rows.len() == request.limit {
            break;
        }
        if stats.examined == request.max_examined {
            has_more = processed < fetched || fetched == ask;
            break;
        }
        if fetched < ask {
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
        ) -> Result<Vec<ScanCandidate>> {
            Ok(vec![ScanCandidate::Memtable("a".into()), ScanCandidate::Memtable("b".into())])
        }

        fn project(
            &self,
            candidate: &ScanCandidate,
            _attrs: &HashSet<&str>,
            _contents: &HashSet<&str>,
        ) -> Result<Record> {
            let id = candidate.id();
            if id == "a" {
                self.cancellation.cancel();
            }
            Record::new(id, vec![], vec![("selected".into(), AttrValue::Bool(true))])
        }

        fn reconstruct_content(
            &self,
            _candidate: &ScanCandidate,
            _content: &Content,
        ) -> Result<Vec<u8>> {
            unreachable!("the interruption test projects no content")
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
