//! The store: WAL, pending change set, publication, manifest revisions, and WAL replay — the layer
//! that turns a fold and parts into a database.
//!
//! # Substrate
//!
//! A store is one container file plus a transient WAL sidecar while a writer has pending changes.
//! There is no daemon in this design; a server is a *role* a process takes when it holds the writer
//! lock, not a thing the format depends on. (That lock is enforced by the OS on native Unix and
//! Windows and **not enforced on `wasm32-wasip1`** — there the single-writer invariant is the
//! embedder's to maintain, since there is no advisory lock to hold. See `src/sys.rs` and FORMAT.md.)
//!
//! [`open_read_container`] takes no lock, replays nothing, and is safe to run
//! concurrently with a writer — parts are immutable and ordinary fold writes append without moving
//! existing locations, so a reader pinned to a store authority sees a consistent store with no
//! coordination at all. Declared content punch is the explicit operation that can end an older
//! view's readability.
//!
//! # Publication authority
//!
//! The container superblock flip is the only publication point. Its directory selects one manifest
//! revision (or the canonical origin), which names the referenced parts, fold tail, and WAL sequence
//! cursor. Everything else — block indexes, dedup indexes, and part contents — is subordinate evidence.
//!
//! # Ordering and WAL replay
//!
//! ```text
//! acceptance      -> fold.put (no fsync) + WAL append
//! synchronization -> acknowledge selected container authority if needed -> WAL fsync
//! publication     -> fold.sync -> write part -> stage manifest revision -> flip container state
//! settlement      -> truncate WAL replay input
//! ```
//! Writer open resolves the current store authority without truncating committed container members,
//! then WAL replay reconstructs the pending change set from complete valid input. Each frame carries
//! the bytes of every piece that was new, so replay never depends on unpublished fold bytes.

pub mod debris;
pub mod read;
pub mod refold;
pub mod wal;

pub use debris::{debris_report, debris_report_with_limits, DebrisEntry, DebrisKind, DebrisReport};

use crate::fold::{Fold, FoldCfg, FoldTail, Loc};
use crate::part::cache::SectionCache;
use crate::part::{self, Part};
use crate::read_limits::ReadLimits;
use crate::types::{AttrValue, BodyOp, Content, ContentHash, PieceHash, Record, BODY_CONTENT};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wal::Wal;

/// Per-writer admission policy. These limits are runtime policy, not store format: reopening with
/// different values changes which future writes are accepted and never changes or invalidates
/// records already in the WAL or immutable parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteLimits {
    /// Worst-case complete WAL frame bytes for one put or delete.
    pub max_record_bytes: u64,
    /// Sum of member frames plus the commit-marker frame for one atomic batch.
    pub max_batch_bytes: u64,
    /// Number of ordered put/delete members in one atomic batch.
    pub max_batch_records: usize,
    /// UTF-8 bytes in a record id, attribute name, or content name.
    pub max_identifier_bytes: usize,
}

/// Default worst-case framed-WAL ceiling for one record: 64 MiB.
pub const DEFAULT_MAX_RECORD_BYTES: u64 = 64 << 20;
/// Default worst-case framed-WAL ceiling for one atomic batch: 256 MiB.
pub const DEFAULT_MAX_BATCH_BYTES: u64 = 256 << 20;
/// Default ordered member ceiling for one atomic batch.
pub const DEFAULT_MAX_BATCH_RECORDS: usize = 4_096;
/// Default UTF-8 byte ceiling for ids and field/content names: 4 KiB.
pub const DEFAULT_MAX_IDENTIFIER_BYTES: usize = 4 << 10;
/// Maximum committed-manifest bytes accepted from a container under the default format reader.
/// A maintained store's manifest is orders of magnitude smaller; this prevents an untrusted sparse
/// file from becoming an unbounded `read_to_end` allocation before JSON parsing can refuse it.
pub const MAX_MANIFEST_BYTES: u64 = 64 << 20;

impl Default for WriteLimits {
    fn default() -> Self {
        WriteLimits {
            max_record_bytes: DEFAULT_MAX_RECORD_BYTES,
            max_batch_bytes: DEFAULT_MAX_BATCH_BYTES,
            max_batch_records: DEFAULT_MAX_BATCH_RECORDS,
            max_identifier_bytes: DEFAULT_MAX_IDENTIFIER_BYTES,
        }
    }
}

/// How much of the store a writer open verifies before it accepts a mutation.
///
/// Writer open always proves the current store authority intelligible before any cleanup or
/// mutation: the container directory and its checksum, the current manifest revision, every
/// retained revision's canonical name, parse, adjacency, `prev` link, cursor and tail order, the
/// presence of every part any of them names, every current part's schema, every fold segment's
/// framing, and the identity of every WAL frame that replay will apply. That work is proportional
/// to metadata, fold framing, and WAL size, never to content. Deep verification is the same
/// evidence [`Store::verify`] produces, obtained before the first mutation instead of on request:
/// every part pin, section, and physical row, every piece-dictionary entry against the fold, and
/// every visible content value reconstructed, for the current revision and each retained one. Its
/// cost is proportional to the whole store multiplied by the retained window, which is why it is
/// not the default for a database open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpenVerification {
    /// Structural evidence only, proportional to metadata and framing.
    #[default]
    Structural,
    /// Everything [`Store::verify`] checks, before the writer accepts a mutation.
    Deep,
}

/// Runtime writer configuration. None of these values are persisted format commitments.
#[derive(Clone, Copy, Debug)]
pub struct StoreOptions {
    pub fold: FoldCfg,
    pub write_limits: WriteLimits,
    /// Admission applied before atomic frame allocation and persistent collection growth.
    pub read_limits: ReadLimits,
    /// One decompressed-section cache budget shared by every immutable part in this handle.
    pub part_cache_bytes: usize,
    /// Verification performed by writer open before the first mutation.
    pub open_verification: OpenVerification,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            fold: FoldCfg::default(),
            write_limits: WriteLimits::default(),
            read_limits: ReadLimits::default(),
            part_cache_bytes: crate::part::cache::BUDGET_DEFAULT,
            open_verification: OpenVerification::Structural,
        }
    }
}

impl WriteLimits {
    /// Return this policy when every ceiling is usable, or a typed invalid-policy error.
    pub fn validate(self) -> std::result::Result<Self, WriteAdmissionError> {
        if self.max_record_bytes == 0 {
            return Err(WriteAdmissionError::InvalidLimits(
                "max_record_bytes must be greater than zero",
            ));
        }
        if self.max_batch_bytes == 0 {
            return Err(WriteAdmissionError::InvalidLimits(
                "max_batch_bytes must be greater than zero",
            ));
        }
        if self.max_batch_records == 0 {
            return Err(WriteAdmissionError::InvalidLimits(
                "max_batch_records must be greater than zero",
            ));
        }
        if self.max_identifier_bytes == 0 {
            return Err(WriteAdmissionError::InvalidLimits(
                "max_identifier_bytes must be greater than zero",
            ));
        }
        Ok(self)
    }
}

/// Stable write refusal classes. Invalid names/settings are caller errors; size/count refusals are
/// resource exhaustion and can be handled by splitting input or reopening with a larger policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteAdmissionError {
    InvalidLimits(&'static str),
    EmptyIdentifier { kind: &'static str, item: Option<usize> },
    IdentifierTooLong { kind: &'static str, item: Option<usize>, actual: usize, allowed: usize },
    DuplicateContentName { item: Option<usize>, name: String },
    RecordTooLarge { item: Option<usize>, actual: u64, allowed: u64 },
    BatchTooLarge { actual: u64, allowed: u64 },
    TooManyBatchRecords { actual: usize, allowed: usize },
}

impl std::fmt::Display for WriteAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let where_ = |item: &Option<usize>| {
            item.map_or_else(String::new, |item| format!(" in batch item {item}"))
        };
        match self {
            WriteAdmissionError::InvalidLimits(reason) => write!(f, "invalid write limits: {reason}"),
            WriteAdmissionError::EmptyIdentifier { kind, item } => {
                write!(f, "{kind}{} must not be empty", where_(item))
            }
            WriteAdmissionError::IdentifierTooLong { kind, item, actual, allowed } => write!(
                f,
                "{kind}{} is {actual} UTF-8 bytes, exceeding the configured limit of {allowed}",
                where_(item)
            ),
            WriteAdmissionError::DuplicateContentName { item, name } => {
                write!(f, "duplicate content name {name:?}{}", where_(item))
            }
            WriteAdmissionError::RecordTooLarge { item, actual, allowed } => write!(
                f,
                "record{} has a worst-case WAL frame of {actual} bytes, exceeding the configured limit of {allowed}",
                where_(item)
            ),
            WriteAdmissionError::BatchTooLarge { actual, allowed } => write!(
                f,
                "atomic batch has a worst-case WAL representation of {actual} bytes, exceeding the configured limit of {allowed}"
            ),
            WriteAdmissionError::TooManyBatchRecords { actual, allowed } => write!(
                f,
                "atomic batch has {actual} records, exceeding the configured limit of {allowed}"
            ),
        }
    }
}

impl std::error::Error for WriteAdmissionError {}

/// A carved span handed to the store: content to fold, or bytes to inline.
#[derive(Clone, Copy)]
pub enum Span<'a> {
    /// Connective tissue too small to be worth folding.
    Lit(&'a [u8]),
    /// Content — deduped by identity across the whole store.
    Piece(&'a [u8]),
}

/// One named content value handed to [`Store::put_record`] or [`Batch::put_record`].
///
/// The spans are already carved, preserving the existing escape hatch: a consumer can accept the
/// engine's default opinion, choose a [`crate::carve::Carve`] per value, or supply its own boundaries.
pub struct ContentSpans<'a> {
    pub name: &'a str,
    pub spans: Vec<Span<'a>>,
}

impl<'a> ContentSpans<'a> {
    pub fn new(name: &'a str, spans: Vec<Span<'a>>) -> ContentSpans<'a> {
        ContentSpans { name, spans }
    }

    pub fn carve(name: &'a str, bytes: &'a [u8], carve: &crate::carve::Carve) -> ContentSpans<'a> {
        ContentSpans { name, spans: carve.carve(bytes) }
    }
}

/// A group of writes that commits ATOMICALLY: after a crash, either every member replays or none
/// does. A lone `put` is durable per record, which means a crash can land between the records of
/// one logical ingest — half an export survived is an anomaly the source then has to reconcile.
/// A batch is the unit the source actually sent.
///
/// A `Batch` is pure staging: it owns copies of its spans and touches neither the fold nor the log
/// until [`Store::apply`], so a batch that is dropped instead of applied leaves NOTHING behind —
/// no fold content, no dedup-window entries, no frames.
#[derive(Default)]
pub struct Batch {
    items: Vec<BatchItem>,
}

enum BatchItem {
    Put { id: String, contents: Vec<OwnedContent>, attrs: Vec<(String, AttrValue)> },
    Delete { id: String },
}

struct OwnedContent {
    name: String,
    spans: Vec<OwnedSpan>,
}

enum OwnedSpan {
    Lit(Vec<u8>),
    Piece(Vec<u8>),
}

impl Batch {
    pub fn new() -> Batch {
        Batch::default()
    }

    /// Stage a put. Same shape as [`Store::put`]; nothing happens until [`Store::apply`].
    pub fn put(&mut self, id: &str, spans: &[Span], attrs: Vec<(String, AttrValue)>) {
        let spans = own_spans(spans);
        self.items.push(BatchItem::Put {
            id: id.to_string(),
            contents: vec![OwnedContent { name: BODY_CONTENT.to_string(), spans }],
            attrs,
        });
    }

    /// Stage a general record. Invalid ids or content maps are refused before the batch owns bytes.
    pub fn put_record(
        &mut self,
        id: &str,
        contents: &[ContentSpans<'_>],
        attrs: Vec<(String, AttrValue)>,
    ) -> Result<()> {
        let item = Some(self.items.len());
        validate_content_inputs(id, contents, item)?;
        if attrs.iter().any(|(name, _)| name.is_empty()) {
            return Err(
                WriteAdmissionError::EmptyIdentifier { kind: "attribute name", item }.into()
            );
        }
        let contents = contents
            .iter()
            .map(|content| OwnedContent {
                name: content.name.to_string(),
                spans: own_spans(&content.spans),
            })
            .collect();
        self.items.push(BatchItem::Put { id: id.to_string(), contents, attrs });
        Ok(())
    }

    /// Stage a put carved by the engine's default opinion. See [`crate::carve`].
    pub fn put_body(&mut self, id: &str, body: &[u8], attrs: Vec<(String, AttrValue)>) {
        self.put(id, &crate::carve::Carve::default().carve(body), attrs);
    }

    /// Stage a deletion.
    pub fn delete(&mut self, id: &str) {
        self.items.push(BatchItem::Delete { id: id.to_string() });
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

fn own_spans(spans: &[Span<'_>]) -> Vec<OwnedSpan> {
    spans
        .iter()
        .map(|s| match s {
            Span::Lit(b) => OwnedSpan::Lit(b.to_vec()),
            Span::Piece(b) => OwnedSpan::Piece(b.to_vec()),
        })
        .collect()
}

const WAL_FRAME_OVERHEAD: u64 = 1 + 8 + 4 + 4;

fn varint_bytes(mut value: u64) -> u64 {
    let mut bytes = 1;
    while value >= 0x80 {
        value >>= 7;
        bytes += 1;
    }
    bytes
}

fn add_size(total: &mut u64, value: u64) {
    *total = total.saturating_add(value);
}

fn bytes_field_size(len: usize) -> u64 {
    let len = len as u64;
    varint_bytes(len).saturating_add(len)
}

fn attr_encoded_size(name: &str, value: &AttrValue) -> u64 {
    let value = match value {
        AttrValue::Str(value) => bytes_field_size(value.len()),
        AttrValue::Int(_)
        | AttrValue::Float(_)
        | AttrValue::UInt(_)
        | AttrValue::TimestampNs(_) => 8,
        AttrValue::Bool(_) => 1,
        AttrValue::Bytes(value) => bytes_field_size(value.len()),
        AttrValue::Null => 0,
    };
    bytes_field_size(name.len()).saturating_add(1).saturating_add(value)
}

fn validate_identifier(
    kind: &'static str,
    value: &str,
    limits: WriteLimits,
    item: Option<usize>,
) -> Result<()> {
    if value.is_empty() {
        return Err(WriteAdmissionError::EmptyIdentifier { kind, item }.into());
    }
    if value.len() > limits.max_identifier_bytes {
        return Err(WriteAdmissionError::IdentifierTooLong {
            kind,
            item,
            actual: value.len(),
            allowed: limits.max_identifier_bytes,
        }
        .into());
    }
    Ok(())
}

fn validate_attr_names(
    attrs: &[(String, AttrValue)],
    limits: WriteLimits,
    item: Option<usize>,
) -> Result<()> {
    for (name, _) in attrs {
        validate_identifier("attribute name", name, limits, item)?;
    }
    Ok(())
}

fn input_record_admission_bytes(
    id: &str,
    contents: &[ContentSpans<'_>],
    attrs: &[(String, AttrValue)],
    limits: WriteLimits,
    read_limits: ReadLimits,
    item: Option<usize>,
) -> Result<u64> {
    validate_identifier("record id", id, limits, item)?;
    validate_attr_names(attrs, limits, item)?;
    let mut names = std::collections::BTreeSet::new();
    let mut size = WAL_FRAME_OVERHEAD;
    add_size(&mut size, bytes_field_size(id.len()));
    add_size(&mut size, varint_bytes(contents.len() as u64));
    for content in contents {
        validate_identifier("content name", content.name, limits, item)?;
        if !names.insert(content.name) {
            return Err(WriteAdmissionError::DuplicateContentName {
                item,
                name: content.name.to_string(),
            }
            .into());
        }
        validate_spans(&content.spans)?;
        add_size(&mut size, bytes_field_size(content.name.len()));
        add_size(&mut size, 32);
        add_size(&mut size, varint_bytes(content.spans.len() as u64));
        let mut novel = 0u64;
        for span in &content.spans {
            match span {
                Span::Lit(bytes) => {
                    add_size(&mut size, 1u64.saturating_add(bytes_field_size(bytes.len())))
                }
                Span::Piece(bytes) => {
                    if bytes.is_empty() {
                        add_size(&mut size, 1u64.saturating_add(bytes_field_size(0)));
                    } else {
                        read_limits.admit(
                            "new fold block",
                            bytes.len() as u64,
                            bytes.len() as u64,
                        )?;
                        add_size(
                            &mut size,
                            1u64.saturating_add(32)
                                .saturating_add(varint_bytes(bytes.len() as u64)),
                        );
                        add_size(&mut novel, 32u64.saturating_add(bytes_field_size(bytes.len())));
                    }
                }
            }
        }
        // Novel pieces are one record-level list; accumulating the entries here is equivalent.
        add_size(&mut size, novel);
    }
    add_size(&mut size, varint_bytes(attrs.len() as u64));
    for (name, value) in attrs {
        add_size(&mut size, attr_encoded_size(name, value));
    }
    let piece_count = contents
        .iter()
        .flat_map(|content| content.spans.iter())
        .filter(|span| matches!(span, Span::Piece(bytes) if !bytes.is_empty()))
        .count() as u64;
    add_size(&mut size, varint_bytes(piece_count));
    if size > limits.max_record_bytes {
        return Err(WriteAdmissionError::RecordTooLarge {
            item,
            actual: size,
            allowed: limits.max_record_bytes,
        }
        .into());
    }
    read_limits.admit("new WAL frame", size, size)?;
    Ok(size)
}

fn validate_content_inputs(
    id: &str,
    contents: &[ContentSpans<'_>],
    item: Option<usize>,
) -> Result<()> {
    if id.is_empty() {
        return Err(WriteAdmissionError::EmptyIdentifier { kind: "record id", item }.into());
    }
    let mut names = std::collections::BTreeSet::new();
    for content in contents {
        if content.name.is_empty() {
            return Err(WriteAdmissionError::EmptyIdentifier { kind: "content name", item }.into());
        }
        if !names.insert(content.name) {
            return Err(WriteAdmissionError::DuplicateContentName {
                item,
                name: content.name.to_string(),
            }
            .into());
        }
        validate_spans(&content.spans)?;
    }
    Ok(())
}

fn validate_spans(spans: &[Span<'_>]) -> Result<()> {
    for span in spans {
        if let Span::Piece(bytes) = span {
            u32::try_from(bytes.len())
                .context("one folded piece exceeds the format's u32 length")?;
        }
    }
    Ok(())
}

fn owned_record_admission_bytes(
    id: &str,
    contents: &[OwnedContent],
    attrs: &[(String, AttrValue)],
    limits: WriteLimits,
    read_limits: ReadLimits,
    item: Option<usize>,
) -> Result<u64> {
    validate_identifier("record id", id, limits, item)?;
    validate_attr_names(attrs, limits, item)?;
    let mut names = std::collections::BTreeSet::new();
    let mut size = WAL_FRAME_OVERHEAD;
    add_size(&mut size, bytes_field_size(id.len()));
    add_size(&mut size, varint_bytes(contents.len() as u64));
    let mut piece_count = 0u64;
    let mut novel = 0u64;
    for content in contents {
        validate_identifier("content name", &content.name, limits, item)?;
        if !names.insert(content.name.as_str()) {
            return Err(WriteAdmissionError::DuplicateContentName {
                item,
                name: content.name.clone(),
            }
            .into());
        }
        add_size(&mut size, bytes_field_size(content.name.len()));
        add_size(&mut size, 1 + 32);
        add_size(&mut size, varint_bytes(content.spans.len() as u64));
        for span in &content.spans {
            match span {
                OwnedSpan::Lit(bytes) => {
                    add_size(&mut size, 1u64.saturating_add(bytes_field_size(bytes.len())))
                }
                OwnedSpan::Piece(bytes) => {
                    if bytes.is_empty() {
                        add_size(&mut size, 1u64.saturating_add(bytes_field_size(0)));
                    } else {
                        read_limits.admit(
                            "new fold block",
                            bytes.len() as u64,
                            bytes.len() as u64,
                        )?;
                        u32::try_from(bytes.len())
                            .context("one folded piece exceeds the format's u32 length")?;
                        piece_count = piece_count.saturating_add(1);
                        add_size(
                            &mut size,
                            1u64.saturating_add(32)
                                .saturating_add(varint_bytes(bytes.len() as u64)),
                        );
                        add_size(&mut novel, 32u64.saturating_add(bytes_field_size(bytes.len())));
                    }
                }
            }
        }
    }
    add_size(&mut size, varint_bytes(attrs.len() as u64));
    for (name, value) in attrs {
        add_size(&mut size, attr_encoded_size(name, value));
    }
    add_size(&mut size, varint_bytes(piece_count));
    add_size(&mut size, novel);
    if size > limits.max_record_bytes {
        return Err(WriteAdmissionError::RecordTooLarge {
            item,
            actual: size,
            allowed: limits.max_record_bytes,
        }
        .into());
    }
    read_limits.admit("new WAL frame", size, size)?;
    Ok(size)
}

fn delete_admission_bytes(
    id: &str,
    limits: WriteLimits,
    read_limits: ReadLimits,
    item: Option<usize>,
) -> Result<u64> {
    validate_identifier("record id", id, limits, item)?;
    let size = WAL_FRAME_OVERHEAD.saturating_add(id.len() as u64);
    if size > limits.max_record_bytes {
        return Err(WriteAdmissionError::RecordTooLarge {
            item,
            actual: size,
            allowed: limits.max_record_bytes,
        }
        .into());
    }
    read_limits.admit("new WAL frame", size, size)?;
    Ok(size)
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PartRef {
    pub member: String,
    pub seq_lo: u64,
    pub seq_hi: u64,
    pub records: u32,
    /// BLAKE3 of the part member's logical bytes, hex — the manifest PINNING the part. Content is pinned
    /// transitively from here: this digest covers `pdict.hash`, which carries per-piece BLAKE3,
    /// so a fold that drifted from what a part expects is detectable without any segment-level
    /// digest. Every current part reference carries one.
    pub b3: String,
}

/// Identity of the one draft manifest schema this build writes and accepts.
pub const DRAFT_FORMAT_EPOCH: u8 = 1;

/// How many manifest revisions are retained beside the current one, as `MANIFEST.<commit>`.
///
/// Retention is what makes recent authority reopenable: every member a retained manifest revision
/// names survives the sweep, so a read view pinned within the window keeps its required bytes, and
/// corrupt current authority can be replaced by explicit manifest promotion. The window is a count
/// of manifest revisions, not time — each publication by flush, merge, or refold advances it by one.
pub const MANIFEST_RETAIN: usize = 4;

/// In-memory fields used for store authority.
///
/// A persisted value is a manifest revision and has `commit > 0`. The default value with
/// `commit == 0` is the implementation and public-result encoding of the canonical origin; it is
/// never persisted or interpreted as manifest revision zero.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Required schema discriminator. This is an exact identity, not a compatibility range.
    pub draft_epoch: u8,
    pub parts: Vec<PartRef>,
    /// Which fold generation this revision references. A refold writes a new one and names it here,
    /// so the swap is one publication.
    pub fold_gen: u32,
    pub fold_seg: u32,
    pub fold_off: u32,
    pub next_seq: u64,
    /// Monotonic manifest-revision counter — the retained-history namespace. `next_seq` cannot
    /// serve here: it only advances at flush, while merge and refold can publish without flushing.
    pub commit: u64,
    /// Block ids whose bytes were PUNCHED out of the fold, as inclusive `[lo, hi]` ranges (erasure
    /// tends to hit runs of blocks, and ranges keep the manifest small). Authoritative, and that
    /// is the point: a punched block reads back as zeros, which is indistinguishable from
    /// corruption unless something says otherwise. This says otherwise.
    ///
    /// Ranges are ascending and disjoint.
    pub punched: Vec<(u32, u32)>,
    /// BLAKE3 of the previous manifest revision's exact bytes, hex — retained history as a hash chain, at
    /// zero marginal cost. `None` only on a store's first commit.
    ///
    /// This is an INTEGRITY check, not a security claim: it catches a manifest that was replaced,
    /// reordered, or restored out of band, which section checksums cannot see because each one is
    /// individually valid. Pruned manifests take their bytes with them, so the chain is verifiable
    /// across the retained window and says nothing about what is no longer there.
    pub prev: Option<String>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            draft_epoch: DRAFT_FORMAT_EPOCH,
            parts: Vec::new(),
            fold_gen: 0,
            fold_seg: 0,
            fold_off: 0,
            next_seq: 0,
            commit: 0,
            punched: Vec::new(),
            prev: None,
        }
    }
}

fn canonical_part_member_shape(name: &str) -> bool {
    let Some(body) = name.strip_prefix("part-").and_then(|rest| rest.strip_suffix(".part")) else {
        return false;
    };
    let fields = body.split('-').collect::<Vec<_>>();
    match fields.as_slice() {
        [one] => one.parse::<u64>().is_ok_and(|seq| name == format!("part-{seq:08}.part")),
        [lo, hi] => lo
            .parse::<u64>()
            .ok()
            .zip(hi.parse::<u64>().ok())
            .is_some_and(|(lo, hi)| name == format!("part-{lo:08}-{hi:08}.part")),
        [generation, lo, hi] => generation
            .strip_prefix('r')
            .filter(|value| value.len() == 4)
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|generation| (1..=crate::fold::MAX_FOLD_GENERATION).contains(generation))
            .zip(lo.parse::<u64>().ok())
            .zip(hi.parse::<u64>().ok())
            .is_some_and(|((generation, lo), hi)| {
                name == format!("part-r{generation:04}-{lo:08}-{hi:08}.part")
            }),
        _ => false,
    }
}

fn valid_part_member_name(name: &str, seq_lo: u64, seq_hi: u64, fold_gen: u32) -> bool {
    let ordinary = if seq_lo == seq_hi {
        name == format!("part-{seq_lo:08}.part")
    } else {
        name == format!("part-{seq_lo:08}-{seq_hi:08}.part")
    };
    ordinary
        || (fold_gen != 0 && name == format!("part-r{fold_gen:04}-{seq_lo:08}-{seq_hi:08}.part"))
}

fn valid_blake3_hex(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_fold_dictionary_member(name: &str) -> bool {
    let Some(hash) = name.strip_prefix("zdict-").and_then(|rest| rest.strip_suffix(".zd")) else {
        return false;
    };
    hash.len() == 64
        && hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn fold_sidecar_member(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("seg-")?.strip_suffix(".dir")?;
    if digits.len() != 8 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn exact_fold_member_name(name: &str) -> bool {
    let Some((_, rest)) = name.split_once('/') else { return false };
    fold_generation_of_member(name).is_some()
        && (crate::fold::segment::parse_seg_name(rest).is_some()
            || fold_sidecar_member(rest).is_some()
            || valid_fold_dictionary_member(rest))
}

fn validate_manifest_promotion_member_namespace(names: &[String]) -> Result<()> {
    for name in names {
        let retained = name.strip_prefix("MANIFEST.").is_some_and(|suffix| {
            suffix.parse::<u64>().is_ok_and(|revision| *name == format!("MANIFEST.{revision:08}"))
        });
        if name == "MANIFEST"
            || retained
            || canonical_part_member_shape(name)
            || exact_fold_member_name(name)
        {
            continue;
        }
        bail!("container member {name:?} is outside the current Store member namespace");
    }
    Ok(())
}

/// Enforce the exhaustive member namespace of one current-format Store container.
///
/// `Container` remains a generic low-level envelope, but a Store admits only its current manifest,
/// canonical retained-manifest names, parts referenced by current or retained authority, and exact
/// fold-member forms inside a referenced generation. An unrecognized member is not an extension:
/// it is a different physical format identity and must fail closed.
fn validate_store_member_namespace(
    names: &[String],
    current: Option<&Manifest>,
    mut read_member: impl FnMut(&str) -> Result<Vec<u8>>,
) -> Result<()> {
    let mut admitted = HashSet::new();
    let mut generations = HashSet::new();
    if let Some(manifest) = current {
        admitted.insert("MANIFEST".to_string());
        admitted.extend(manifest.parts.iter().map(|part| part.member.clone()));
        generations.insert(manifest.fold_gen);
    } else if !names.is_empty() {
        bail!("canonical-origin store carries container members");
    }

    for name in names.iter().filter(|name| name.starts_with("MANIFEST.")) {
        let suffix = &name["MANIFEST.".len()..];
        let commit = suffix
            .parse::<u64>()
            .with_context(|| format!("retained manifest member {name:?} has no revision number"))?;
        if *name != format!("MANIFEST.{commit:08}") {
            bail!("retained manifest member {name:?} is not in canonical revision form");
        }
        let manifest = Manifest::parse(&read_member(name)?)
            .with_context(|| format!("retained manifest member {name:?} is invalid"))?;
        if manifest.commit != commit {
            bail!("retained member {name:?} contains manifest revision {}", manifest.commit);
        }
        admitted.insert(name.clone());
        admitted.extend(manifest.parts.into_iter().map(|part| part.member));
        generations.insert(manifest.fold_gen);
    }

    for name in names {
        if admitted.contains(name) {
            continue;
        }
        if let Some(generation) = fold_generation_of_member(name) {
            let rest = name.split_once('/').expect("fold member has a slash").1;
            let exact_member = crate::fold::segment::parse_seg_name(rest).is_some()
                || fold_sidecar_member(rest).is_some()
                || valid_fold_dictionary_member(rest);
            if generations.contains(&generation) && exact_member {
                continue;
            }
        }
        bail!("container member {name:?} is outside the current Store member namespace");
    }
    Ok(())
}

impl Manifest {
    /// Parse manifest bytes, requiring and verifying its checksum trailer.
    ///
    /// The manifest is the authority whose corruption could otherwise hide durable content with no
    /// error: a flipped bit that still parses — a shortened `fold_off`, a wrong generation — must
    /// not become a believable read boundary. Every other structure refuses corruption; this
    /// closes the authority gap.
    ///
    pub(crate) fn parse(bytes: &[u8]) -> Result<Manifest> {
        let (payload, want) = checksum_trailer(bytes)
            .ok_or_else(|| anyhow::anyhow!("MANIFEST lacks the required checksum trailer"))?;
        let got = crc32fast::hash(payload);
        if got != want {
            bail!(
                "MANIFEST fails its checksum (crc32 {got:08x}, recorded {want:08x}) — \
                 refusing to open from corrupt manifest authority"
            );
        }
        if payload.len() as u64 > MAX_MANIFEST_BYTES {
            bail!(
                "MANIFEST is {} bytes, exceeding the supported {MAX_MANIFEST_BYTES}-byte limit",
                payload.len()
            );
        }
        let manifest: Manifest = serde_json::from_slice(payload).context("corrupt MANIFEST")?;
        if serde_json::to_vec(&manifest)? != payload {
            bail!("MANIFEST JSON is not in the one canonical current encoding");
        }
        manifest.validate()
    }

    /// Validate semantic fields before any one of them becomes a filesystem path or allocation
    /// input. JSON syntax and a checksum prove faithful bytes, not safe meaning.
    fn validate(self) -> Result<Manifest> {
        if self.draft_epoch != DRAFT_FORMAT_EPOCH {
            bail!(
                "MANIFEST declares draft epoch {}; this build accepts exactly {}",
                self.draft_epoch,
                DRAFT_FORMAT_EPOCH
            );
        }
        match (self.commit, self.prev.is_some()) {
            (0, _) => bail!("a persisted MANIFEST cannot have commit zero"),
            (1, false) => {}
            (1, true) => bail!("the origin MANIFEST must not name a predecessor"),
            (_, true) => {}
            (_, false) => bail!("MANIFEST revision {} must name its predecessor", self.commit),
        }
        if self.fold_gen > crate::fold::MAX_FOLD_GENERATION {
            bail!("MANIFEST fold generation {} is outside the current namespace", self.fold_gen);
        }
        if self.fold_seg > crate::fold::segment::MAX_SEGMENT_NUMBER {
            bail!("MANIFEST fold segment {} is outside the current namespace", self.fold_seg);
        }
        if u64::from(self.fold_off) < crate::fold::segment::SEG_HDR_LEN {
            bail!("a persisted MANIFEST must commit a complete fold segment header");
        }
        let mut members = HashSet::with_capacity(self.parts.len());
        let mut previous_part_hi: Option<u64> = None;
        for part in &self.parts {
            if !valid_part_member_name(&part.member, part.seq_lo, part.seq_hi, self.fold_gen) {
                bail!(
                    "MANIFEST part member {:?} is not the canonical name for sequence interval {}..={}",
                    part.member,
                    part.seq_lo,
                    part.seq_hi
                );
            }
            if !members.insert(part.member.as_str()) {
                bail!("MANIFEST names part member {:?} more than once", part.member);
            }
            if part.seq_lo > part.seq_hi {
                bail!(
                    "MANIFEST part {:?} has inverted sequence range {}..{}",
                    part.member,
                    part.seq_lo,
                    part.seq_hi
                );
            }
            let expected_lo = match previous_part_hi {
                Some(previous) => match previous.checked_add(1) {
                    Some(next) => next,
                    None => {
                        bail!("MANIFEST part sequence intervals exceed the u64 sequence domain")
                    }
                },
                None => 1,
            };
            if part.seq_lo != expected_lo {
                bail!(
                    "MANIFEST part sequence intervals must begin at one and remain contiguous; expected {expected_lo}, found {}",
                    part.seq_lo
                );
            }
            previous_part_hi = Some(part.seq_hi);
            if !valid_blake3_hex(&part.b3) {
                bail!("MANIFEST part {:?} lacks a valid BLAKE3 digest", part.member);
            }
        }
        match previous_part_hi {
            Some(highest) if self.next_seq != highest => bail!(
                "MANIFEST next_seq {} differs from the highest published part sequence {highest}",
                self.next_seq
            ),
            None if self.next_seq != 0 => {
                bail!("a MANIFEST without parts must have next_seq zero")
            }
            _ => {}
        }
        if self.prev.as_deref().is_some_and(|digest| !valid_blake3_hex(digest)) {
            bail!("MANIFEST carries an invalid previous-manifest BLAKE3 digest");
        }
        let mut previous_hi = None;
        for &(lo, hi) in &self.punched {
            if lo > hi || previous_hi.is_some_and(|previous| lo <= previous) {
                bail!("MANIFEST punched ranges must be ascending, disjoint, and non-empty");
            }
            previous_hi = Some(hi);
        }
        Ok(self)
    }

    /// The bytes as committed: compact JSON, then a `\ncrc32=XXXXXXXX` trailer over the JSON.
    fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = serde_json::to_vec(self)?;
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(format!("\ncrc32={crc:08x}").as_bytes());
        Ok(buf)
    }

    /// Stage a container manifest revision and its retained copy. The
    /// caller's superblock flip — not this — is the linearization point, so everything a flush
    /// staged (fold extents, the part, these manifests, the sweep's frees) publishes as ONE
    /// atomic state. Retention pruning is part of the same commit it belongs to.
    fn commit_into_container(&mut self, c: &mut crate::container::Container) -> Result<()> {
        // Chain onto whatever is being replaced — from the member's bytes, because the chain's
        // claim is about what a verifier can read back. Only actual absence means origin; an I/O
        // failure while reading existing authority must never fabricate a predecessor-free
        // revision.
        let predecessor = if c.contains("MANIFEST") {
            Some(c.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)
        } else {
            None
        };
        self.commit = self.commit.checked_add(1).context("MANIFEST revision space exhausted")?;
        self.prev = predecessor.map(|bytes| blake3::hash(&bytes).to_hex().to_string());
        self.clone().validate()?;
        let bytes = self.encode()?;
        c.put_bytes(&format!("MANIFEST.{:08}", self.commit), &bytes)?;
        c.put_bytes("MANIFEST", &bytes)?;
        let oldest_retained = self.commit.saturating_sub(MANIFEST_RETAIN as u64);
        for commit in container_retained_commits(c) {
            if commit <= oldest_retained {
                c.remove(&format!("MANIFEST.{commit:08}"))?;
            }
        }
        Ok(())
    }

    fn fold_tail(&self) -> Option<FoldTail> {
        if self.commit == 0 {
            None
        } else {
            Some(FoldTail { seg: self.fold_seg, off: self.fold_off })
        }
    }
}

fn open_manifest_part(
    reader: Box<dyn crate::readat::ReadAt>,
    reference: &PartRef,
    cache: Arc<SectionCache>,
    read_limits: ReadLimits,
) -> Result<Part> {
    let part = Part::open_reader_with_limits(reader, cache, read_limits)?;
    let meta = part.meta();
    if meta.n_records != reference.records
        || meta.seq_lo != reference.seq_lo
        || meta.seq_hi != reference.seq_hi
    {
        bail!(
            "MANIFEST metadata for {} disagrees with its part footer: manifest rows/range \
             {}/{}..{}, part rows/range {}/{}..{}",
            reference.member,
            reference.records,
            reference.seq_lo,
            reference.seq_hi,
            meta.n_records,
            meta.seq_lo,
            meta.seq_hi
        );
    }
    Ok(part)
}

/// The `(payload, recorded crc32)` of a checksummed manifest. Recognition is by exact shape: a
/// final line `crc32=` plus eight hex digits.
fn checksum_trailer(bytes: &[u8]) -> Option<(&[u8], u32)> {
    let pos = bytes.iter().rposition(|&b| b == b'\n')?;
    let tail = &bytes[pos + 1..];
    if tail.len() != 14 || !tail.starts_with(b"crc32=") {
        return None;
    }
    let hex = std::str::from_utf8(&tail[6..]).ok()?;
    if !hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        return None;
    }
    let want = u32::from_str_radix(hex, 16).ok()?;
    Some((&bytes[..pos], want))
}

fn container_sidecar_bytes(
    container: &crate::container::Container,
    name: &str,
    segment_len: u64,
    read_limits: ReadLimits,
) -> Result<Option<Vec<u8>>> {
    let Some(stored) = container.member_len(name) else { return Ok(None) };
    read_limits.admit_stored(format!("fold directory sidecar {name}"), stored)?;
    Ok(Some(
        container
            .read_file_bounded(name, crate::fold::segment::max_dir_sidecar_bytes(segment_len))?,
    ))
}

fn container_reader_sidecar_bytes(
    container: &crate::container::ContainerReader,
    name: &str,
    segment_len: u64,
    read_limits: ReadLimits,
) -> Result<Option<Vec<u8>>> {
    let Some(extent) = container.extent(name) else { return Ok(None) };
    let stored = crate::readat::ReadAt::len(&extent)?;
    read_limits.admit_stored(format!("fold directory sidecar {name}"), stored)?;
    Ok(Some(
        container
            .read_file_bounded(name, crate::fold::segment::max_dir_sidecar_bytes(segment_len))?,
    ))
}

/// Open the exact fold view described by `manifest` from the supplied container handle. Keeping
/// the extents tied to this handle matters during verification: a concurrent path replacement
/// must never cause bytes from another inode to determine which checksums are exempt here.
fn open_fold_from_container(
    container: &crate::container::Container,
    manifest: &Manifest,
    cfg: FoldCfg,
    label: &Path,
    read_limits: ReadLimits,
) -> Result<Fold> {
    let fold_rel = crate::fold::fold_member_prefix(manifest.fold_gen);
    let fold_tail = manifest.fold_tail();
    let names = container.names().map(String::from).collect::<Vec<_>>();
    let mut segs = Vec::new();
    let mut present_segments = HashSet::new();
    let mut dict_files = Vec::new();
    let mut sidecars = HashSet::new();
    for name in names {
        let Some(rest) = name.strip_prefix(&format!("{fold_rel}/")) else { continue };
        if let Some(number) = crate::fold::segment::parse_seg_name(rest) {
            present_segments.insert(number);
            let extent = container.extent(&name).expect("name came from this container");
            let segment_len = crate::readat::ReadAt::len(&extent)?;
            let tail = fold_tail.ok_or_else(|| {
                anyhow::anyhow!(
                    "container has fold segment {number} but its manifest declares no fold tail"
                )
            })?;
            if number > tail.seg {
                bail!(
                    "container has fold segment {number} beyond manifest tail segment {}",
                    tail.seg
                );
            }
            if number == tail.seg && segment_len != u64::from(tail.off) {
                bail!(
                    "manifest fold tail is segment {}, offset {}, but that member is {segment_len} bytes",
                    tail.seg,
                    tail.off
                );
            }
            let reader: Arc<dyn crate::readat::ReadAt> = if number == tail.seg {
                Arc::new(crate::readat::Slice::new(extent, 0, u64::from(tail.off)))
            } else {
                Arc::new(extent)
            };
            segs.push(crate::fold::SegmentInput {
                seg: number,
                reader,
                sidecar: container_sidecar_bytes(
                    container,
                    &format!("{fold_rel}/seg-{number:08}.dir"),
                    segment_len,
                    read_limits,
                )?,
            });
        } else if let Some(number) = fold_sidecar_member(rest) {
            sidecars.insert(number);
        } else if valid_fold_dictionary_member(rest) {
            let bytes = container.read_file_bounded(&name, crate::fold::MAX_DICTIONARY_BYTES)?;
            let exact = format!("zdict-{}.zd", PieceHash::of(&bytes).to_hex());
            if rest != exact {
                bail!("fold dictionary member {name:?} does not match its content identity");
            }
            dict_files.push(bytes);
        } else {
            bail!("container member {name:?} is not in the current fold namespace");
        }
    }
    if let Some(orphan) = sidecars.iter().find(|number| !segs.iter().any(|seg| seg.seg == **number))
    {
        bail!("fold sidecar seg-{orphan:08}.dir has no corresponding segment");
    }
    if let Some(tail) = fold_tail {
        if !present_segments.contains(&tail.seg) {
            bail!("manifest fold tail names absent segment {}", tail.seg);
        }
    }
    Fold::open_read_from_with_limits(segs, dict_files, cfg, label, &manifest.punched, read_limits)
}

/// Open a reader over a container, the store's single physical form.
pub fn open_read_container(path: &Path, cfg: FoldCfg) -> Result<ReadStore> {
    open_read_container_with_limits(path, cfg, ReadLimits::default())
}

/// Open a container reader with explicit frame and persistent object-count admission.
pub fn open_read_container_with_limits(
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    debris::validate_store_path(path)?;
    open_read_container_with_limits_internal(path, cfg, read_limits)
}

fn open_read_container_with_limits_internal(
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    crate::fold::validate_cfg(cfg)?;
    if !path.exists() && crate::container::reclaim_names(path).anchor.exists() {
        bail!(
            "{} is absent but its reclaim anchor {} exists: a reclaim's replace was interrupted; \
             a writer open recovers the store from the anchor (readers never mutate)",
            path.display(),
            crate::container::reclaim_names(path).anchor.display()
        );
    }
    let read_limits = read_limits.validate()?;
    let container = crate::container::Container::open_internal_with_limits(path, read_limits)?;
    open_read_container_handle(&container, cfg, path, read_limits)
}

/// Build the current read view entirely from one already-open container handle. Integrity and
/// writer-preflight callers must never combine a directory/manifest from one inode with parts or a
/// fold reopened from another path identity.
fn open_read_container_handle(
    container: &crate::container::Container,
    cfg: FoldCfg,
    label: &Path,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    crate::fold::validate_cfg(cfg)?;
    let read_limits = read_limits.validate()?;
    // Absent means a store nothing has flushed yet, with one tripwire: retained commits beside a
    // missing manifest are damage, not emptiness.
    let has_manifest = container.contains("MANIFEST");
    let manifest = if has_manifest {
        Manifest::parse(&container.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?
    } else if container.committed_is_empty_birth() {
        Manifest::default()
    } else {
        bail!(
            "container {} has no MANIFEST authority but is not the exact empty sequence-zero \
             birth state",
            label.display(),
        );
    };
    let names = container.names().map(String::from).collect::<Vec<_>>();
    validate_store_member_namespace(&names, has_manifest.then_some(&manifest), |name| {
        container.read_file_bounded(name, MAX_MANIFEST_BYTES)
    })?;

    let fold = open_fold_from_container(container, &manifest, cfg, label, read_limits)?;

    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    for p in &manifest.parts {
        let ext = container.extent(&p.member).ok_or_else(|| {
            anyhow::anyhow!(
                "container manifest names {} but the container does not hold it",
                p.member
            )
        })?;
        parts.push(Arc::new(open_manifest_part(Box::new(ext), p, pcache.clone(), read_limits)?));
    }
    Ok(ReadStore { fold: Arc::new(fold), parts, manifest, read_limits })
}

/// Open a read-only container over an arbitrary positioned byte source.
///
/// The source may be memory, an object-store range cache, or a browser callback. It receives the
/// same admission checks, part readers, fold readers, visibility, and query implementation as a
/// filesystem-backed snapshot.
pub fn open_read_container_source(
    source: std::sync::Arc<dyn crate::readat::ReadAt>,
    label: &str,
    cfg: FoldCfg,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    let read_limits = read_limits.validate()?;
    let container =
        crate::container::ContainerReader::open_with_limits(source, label, read_limits)?;
    let has_manifest = container.contains("MANIFEST");
    let manifest = if has_manifest {
        Manifest::parse(&container.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?
    } else if container.committed_is_empty_birth() {
        Manifest::default()
    } else {
        bail!("container {label} has no MANIFEST authority but is not the exact empty sequence-zero birth state")
    };
    let names = container.names().map(String::from).collect::<Vec<_>>();
    validate_store_member_namespace(&names, has_manifest.then_some(&manifest), |name| {
        container.read_file_bounded(name, MAX_MANIFEST_BYTES)
    })?;
    let fold_rel = if manifest.fold_gen == 0 {
        "fold".to_string()
    } else {
        format!("fold-{:04}", manifest.fold_gen)
    };
    let fold_tail = manifest.fold_tail();
    let mut segs = Vec::new();
    let mut present_segments = HashSet::new();
    let mut dict_files = Vec::new();
    let mut sidecars = HashSet::new();
    for name in names {
        let Some(rest) = name.strip_prefix(&format!("{fold_rel}/")) else { continue };
        if let Some(number) = crate::fold::segment::parse_seg_name(rest) {
            present_segments.insert(number);
            let extent = container.extent(&name).expect("name came from this directory");
            let segment_len = crate::readat::ReadAt::len(&extent)?;
            let tail = fold_tail.ok_or_else(|| {
                anyhow::anyhow!(
                    "container has fold segment {number} but its manifest declares no fold tail"
                )
            })?;
            if number > tail.seg {
                bail!(
                    "container has fold segment {number} beyond manifest tail segment {}",
                    tail.seg
                );
            }
            if number == tail.seg && segment_len != u64::from(tail.off) {
                bail!(
                    "manifest fold tail is segment {}, offset {}, but that member is {segment_len} bytes",
                    tail.seg,
                    tail.off
                );
            }
            let reader: Arc<dyn crate::readat::ReadAt> = if number == tail.seg {
                Arc::new(crate::readat::Slice::new(extent, 0, u64::from(tail.off)))
            } else {
                Arc::new(extent)
            };
            segs.push(crate::fold::SegmentInput {
                seg: number,
                reader,
                sidecar: container_reader_sidecar_bytes(
                    &container,
                    &format!("{fold_rel}/seg-{number:08}.dir"),
                    segment_len,
                    read_limits,
                )?,
            });
        } else if let Some(number) = fold_sidecar_member(rest) {
            // Opened alongside its exact segment name above; advisory absence is allowed.
            sidecars.insert(number);
        } else if valid_fold_dictionary_member(rest) {
            let bytes = container.read_file_bounded(&name, crate::fold::MAX_DICTIONARY_BYTES)?;
            let exact = format!("zdict-{}.zd", PieceHash::of(&bytes).to_hex());
            if rest != exact {
                bail!("fold dictionary member {name:?} does not match its content identity");
            }
            dict_files.push(bytes);
        } else {
            bail!("container member {name:?} is not in the current fold namespace");
        }
    }
    if let Some(orphan) = sidecars.iter().find(|number| !segs.iter().any(|seg| seg.seg == **number))
    {
        bail!("fold sidecar seg-{orphan:08}.dir has no corresponding segment");
    }
    if let Some(tail) = fold_tail {
        if !present_segments.contains(&tail.seg) {
            bail!("manifest fold tail names absent segment {}", tail.seg);
        }
    }
    let fold = Fold::open_read_from_with_limits(
        segs,
        dict_files,
        cfg,
        Path::new(label),
        &manifest.punched,
        read_limits,
    )?;
    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    for part in &manifest.parts {
        let extent = container.extent(&part.member).ok_or_else(|| {
            anyhow::anyhow!(
                "container manifest names {} but the container does not hold it",
                part.member
            )
        })?;
        parts.push(Arc::new(open_manifest_part(
            Box::new(extent),
            part,
            pcache.clone(),
            read_limits,
        )?));
    }
    Ok(ReadStore { fold: Arc::new(fold), parts, manifest, read_limits })
}

/// Prove the whole-value identities asserted by WAL replay before writer open mutates any store byte.
/// A frame carries only newly introduced piece bytes, so resolution spans the current store authority and
/// every earlier frame in this same WAL.
fn verify_replay_identities(frames: &[wal::Frame], current: &ReadStore) -> Result<()> {
    let pending_sequence = current.manifest.next_seq.checked_add(1);
    let mut saw_pending = false;
    let mut pending: HashMap<PieceHash, Vec<u8>> = HashMap::new();
    for frame in frames {
        let redundant = current.manifest.commit != 0 && frame.seq == current.manifest.next_seq;
        let is_pending = Some(frame.seq) == pending_sequence;
        if !redundant && !is_pending {
            bail!(
                "WAL frame carries sequence {}, expected the current record-version sequence {} or its representable successor",
                frame.seq,
                current.manifest.next_seq,
            );
        }
        if redundant && saw_pending {
            bail!("WAL returns from pending sequence to already-published sequence");
        }
        saw_pending |= is_pending;
        for (hash, bytes) in &frame.novel {
            if pending.insert(*hash, bytes.clone()).is_some() {
                bail!("WAL repeats novel piece identity {hash}");
            }
        }
        for content in &frame.record.contents {
            let mut hasher = blake3::Hasher::new();
            for op in &content.ops {
                match op {
                    BodyOp::Lit(bytes) => {
                        hasher.update(bytes);
                    }
                    BodyOp::Piece { hash, len } => {
                        let owned;
                        let bytes = if let Some(bytes) = pending.get(hash) {
                            bytes.as_slice()
                        } else {
                            let loc = locate_verified_piece(&current.parts, &current.fold, hash)?
                                .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "WAL content {:?} references absent piece {hash}",
                                    content.name
                                )
                            })?;
                            owned = current.fold.read_verified(loc, *hash)?;
                            owned.as_slice()
                        };
                        let declared_len = usize::try_from(*len)
                            .context("WAL piece length exceeds this platform")?;
                        if bytes.len() != declared_len {
                            bail!(
                                "WAL content {:?} declares piece {hash} length {len}, actual {}",
                                content.name,
                                bytes.len()
                            );
                        }
                        hasher.update(bytes);
                    }
                }
            }
            let declared = content.identity.ok_or_else(|| {
                anyhow::anyhow!("WAL content {:?} has no whole-value identity", content.name)
            })?;
            let actual = ContentHash(hasher.finalize().into());
            if actual != declared {
                bail!(
                    "WAL content {:?} identity is {declared}, reconstructed bytes are {actual}",
                    content.name
                );
            }
        }
    }
    Ok(())
}

/// Resolve one piece through the pending Fold window and then the authoritative Part dictionaries.
/// A punched dictionary entry is historical residue, not a lookup result; continue through older
/// parts until a readable mapping is found. Acceptance and WAL replay must use this same rule or a
/// mutation can be accepted without bytes that its next open knows how to recover.
fn locate_verified_piece(
    parts: &[Arc<Part>],
    fold: &Fold,
    hash: &PieceHash,
) -> Result<Option<Loc>> {
    fold.ensure_no_failed_write()?;
    if let Some(location) = fold.lookup(*hash) {
        return Ok(Some(location));
    }
    for part in parts.iter().rev() {
        if let Some(location) = part.find_piece(hash)? {
            if fold.is_punched(location.block_id) {
                continue;
            }
            fold.read_verified(location, *hash)
                .with_context(|| format!("piece dictionary mapped {hash} to invalid fold bytes"))?;
            return Ok(Some(location));
        }
    }
    Ok(None)
}

/// Exact physical piece locations required by the rows visible under one store authority.
///
/// Programs are interpreted through their owning Part dictionaries, exactly like reconstruction.
/// A content identity may legitimately occur at several Fold locations; keying by `Loc` preserves
/// every such location so maintenance cannot punch one merely because another copy is readable.
fn live_fold_pieces_with_control(
    parts: &[Arc<Part>],
    fold: &Fold,
    control: &crate::control::OperationControl,
) -> Result<BTreeMap<Loc, PieceHash>> {
    let visible = read::visibility(parts)?;
    let mut pieces = BTreeMap::new();
    for (part_index, rows) in visible.rows.iter().enumerate() {
        let part = &parts[part_index];
        for &row in rows {
            control.check("content reachability")?;
            for content in part.record(row)?.contents {
                for operation in content.ops {
                    let BodyOp::Piece { hash, len } = operation else { continue };
                    let location = part.find_piece(&hash)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "a record resolved by current authority references absent piece {hash} in its owning Part"
                        )
                    })?;
                    if location.raw != len {
                        bail!(
                            "live piece {hash} is {} bytes but its record says {len}",
                            location.raw
                        );
                    }
                    fold.visit_verified(location, hash, |_| {}).with_context(|| {
                        format!(
                            "record resolved by current authority maps piece {hash} to invalid Fold bytes"
                        )
                    })?;
                    if let Some(existing) = pieces.insert(location, hash) {
                        if existing != hash {
                            bail!(
                                "live Fold location {location:?} carries conflicting piece identities {existing} and {hash}"
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(pieces)
}

/// Open a read view at retained manifest revision `commit`.
///
/// The retained manifest names the state; `punched` comes from the current manifest revision because
/// content deallocation is declared by current authority and a retained copy predates later content
/// punches. Fold readers are bounded to the retained revision's exact tail.
pub fn open_read_container_at(path: &Path, cfg: FoldCfg, commit: u64) -> Result<ReadStore> {
    open_read_container_at_with_limits(path, cfg, commit, ReadLimits::default())
}

/// [`open_read_container_at`] with explicit frame and object-count admission.
pub fn open_read_container_at_with_limits(
    path: &Path,
    cfg: FoldCfg,
    commit: u64,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    let read_limits = read_limits.validate()?;
    debris::validate_store_path(path)?;
    let c = crate::container::Container::open_internal_with_limits(path, read_limits)?;
    let bytes =
        c.read_file_bounded(&format!("MANIFEST.{commit:08}"), MAX_MANIFEST_BYTES).with_context(
            || format!("retained manifest revision {commit} is not held by {}", path.display()),
        )?;
    let manifest = Manifest::parse(&bytes)?;
    let current = Manifest::parse(&c.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)
        .with_context(|| {
            format!(
                "retained read view at manifest revision {commit} needs current manifest authority in {} to tell \
                 erased blocks from damaged ones, and it could not be read",
                path.display()
            )
        })?;
    let names = c.names().map(String::from).collect::<Vec<_>>();
    validate_store_member_namespace(&names, Some(&current), |name| {
        c.read_file_bounded(name, MAX_MANIFEST_BYTES)
    })?;
    verification_integrity(
        "verify retained authority chain before opening a retained read view",
        verify_chain_container(&c, read_limits, &crate::control::OperationControl::default()),
    )?;
    open_retained_from_container(&c, path, cfg, commit, manifest, &current, names, read_limits)
}

#[allow(clippy::too_many_arguments)]
fn open_retained_from_container(
    c: &crate::container::Container,
    path: &Path,
    cfg: FoldCfg,
    commit: u64,
    mut manifest: Manifest,
    current: &Manifest,
    names: Vec<String>,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    if current.fold_gen != manifest.fold_gen {
        bail!(
            "retained manifest revision {commit} is from fold generation {} but current authority references {} — \
             a refold purges retained history, so this read view has no content-punch authority",
            manifest.fold_gen,
            current.fold_gen
        );
    }
    manifest.punched.clone_from(&current.punched);

    let prefix = crate::fold::fold_member_prefix(manifest.fold_gen);
    let tail = manifest.fold_tail();
    let mut segs = Vec::new();
    let mut dict_files = Vec::new();
    let mut present_segments = HashSet::new();
    let mut sidecars = HashSet::new();
    for name in names {
        let Some(rest) = name.strip_prefix(&format!("{prefix}/")) else { continue };
        if let Some(n) = crate::fold::segment::parse_seg_name(rest) {
            present_segments.insert(n);
            let extent = c.extent(&name).expect("name came from this container");
            let full_len = crate::readat::ReadAt::len(&extent)?;
            let (reader, len, whole): (Arc<dyn crate::readat::ReadAt>, u64, bool) = match tail {
                Some(t) if n > t.seg => continue,
                Some(t) if n == t.seg => (
                    Arc::new(crate::readat::Slice::new(extent, 0, u64::from(t.off))),
                    u64::from(t.off),
                    u64::from(t.off) == full_len,
                ),
                _ => (Arc::new(extent), full_len, true),
            };
            segs.push(crate::fold::SegmentInput {
                seg: n,
                reader,
                sidecar: if whole {
                    container_sidecar_bytes(
                        c,
                        &format!("{prefix}/seg-{n:08}.dir"),
                        len,
                        read_limits,
                    )?
                } else {
                    None
                },
            });
        } else if let Some(number) = fold_sidecar_member(rest) {
            sidecars.insert(number);
        } else if valid_fold_dictionary_member(rest) {
            let bytes = c.read_file_bounded(&name, crate::fold::MAX_DICTIONARY_BYTES)?;
            let exact = format!("zdict-{}.zd", PieceHash::of(&bytes).to_hex());
            if rest != exact {
                bail!("fold dictionary member {name:?} does not match its content identity");
            }
            dict_files.push(bytes);
        } else {
            bail!("container member {name:?} is not in the current fold namespace");
        }
    }
    if let Some(orphan) = sidecars.iter().find(|number| !present_segments.contains(number)) {
        bail!("fold sidecar seg-{orphan:08}.dir has no corresponding segment");
    }
    if let Some(tail) = tail {
        if !present_segments.contains(&tail.seg) {
            bail!("retained manifest fold tail names absent segment {}", tail.seg);
        }
    }
    let fold = Fold::open_retained_read_from_with_limits(
        segs,
        dict_files,
        cfg,
        path,
        &manifest.punched,
        read_limits,
    )?;
    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    for p in &manifest.parts {
        let ext = c.extent(&p.member).ok_or_else(|| {
            anyhow::anyhow!(
                "retained commit {commit} names {} but the container does not hold it",
                p.member
            )
        })?;
        parts.push(Arc::new(open_manifest_part(Box::new(ext), p, pcache.clone(), read_limits)?));
    }
    Ok(ReadStore { fold: Arc::new(fold), parts, manifest, read_limits })
}

/// Open a reader over the current TurnDB container format.
pub fn open_read_file(path: &Path, cfg: FoldCfg) -> Result<ReadStore> {
    open_read_file_with_limits(path, cfg, ReadLimits::default())
}

/// Open a single-file reader with explicit frame and persistent object-count admission.
pub fn open_read_file_with_limits(
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    open_read_container_with_limits(path, cfg, read_limits)
}

/// What [`verify_chain_file`] checked.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChainReport {
    /// Retained manifest members parsed and checked. Zero is an explicit empty/new-store result.
    pub retained_manifests: usize,
    /// Predecessor links verified across the retained window, including equality between the newest
    /// retained revision and current `MANIFEST`.
    pub links: usize,
    /// Part digests verified against their members, across every retained manifest revision.
    pub part_digests: usize,
}

/// Evidence returned by complete verification of one current store authority.
#[derive(Clone, Copy, Debug, Default)]
pub struct StoreVerification {
    pub chain: ChainReport,
    pub fold: crate::fold::FoldScrub,
    /// Distinct parts referenced by the current manifest revision. Retained-only parts are also
    /// verified but are not included in this current-authority count.
    pub parts: usize,
    /// Sections in parts referenced by the current manifest revision. Retained-only section work
    /// is evidenced by the retained-chain checks rather than added to this count.
    pub part_sections: usize,
    /// Distinct record slots resolved by the read view pinned to current store authority.
    pub records: usize,
    /// Named content values reconstructed byte-exactly.
    pub content_values: usize,
    /// Exact bytes reconstructed across all named content values.
    pub content_bytes: u64,
    /// Reconstructed values whose stored whole-value BLAKE3 identity was checked.
    pub content_identities: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct VerifiedVisibleContent {
    records: usize,
    values: usize,
    bytes: u64,
    identities: usize,
}

fn verify_visible_content(
    parts: &[Arc<Part>],
    fold: &Fold,
    control: &crate::control::OperationControl,
) -> Result<VerifiedVisibleContent> {
    let visible = verification_integrity("enumerate committed records", read::visibility(parts))?;
    let mut report = VerifiedVisibleContent::default();
    for (part_index, rows) in visible.rows.iter().enumerate() {
        let part = &parts[part_index];
        for &row in rows {
            control.check("store verification")?;
            let record = verification_integrity("decode committed record", part.record(row))?;
            report.records = report
                .records
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("verified record count overflow"))?;
            for content in &record.contents {
                control.check("store verification")?;
                let bytes = verification_integrity(
                    "verify committed content",
                    part.verify_projected_content_with_control(content, fold, control),
                )?;
                report.values = report
                    .values
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("verified content value count overflow"))?;
                report.bytes = report
                    .bytes
                    .checked_add(bytes)
                    .ok_or_else(|| anyhow::anyhow!("verified content byte count overflow"))?;
                report.identities = report
                    .identities
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("verified content identity count overflow"))?;
            }
        }
    }
    Ok(report)
}

fn verify_retained_visible_content(
    parts: &[Arc<Part>],
    fold: &Fold,
    control: &crate::control::OperationControl,
) -> Result<()> {
    let visible = verification_integrity("enumerate retained records", read::visibility(parts))?;
    for (part_index, rows) in visible.rows.iter().enumerate() {
        let part = &parts[part_index];
        for &row in rows {
            control.check("retained-authority verification")?;
            let record = verification_integrity("decode retained record", part.record(row))?;
            for content in &record.contents {
                verification_integrity(
                    "verify retained content",
                    part.verify_retained_projected_content_with_control(content, fold, control),
                )?;
            }
        }
    }
    Ok(())
}

fn verify_committed_store(
    parts: &[Arc<Part>],
    fold_store: &Fold,
    chain: ChainReport,
    control: &crate::control::OperationControl,
) -> Result<StoreVerification> {
    let fold =
        verification_integrity("verify fold frames", fold_store.scrub_with_control(control))?;
    let mut part_sections = 0usize;
    for part in parts {
        control.check("store verification")?;
        let sections = verification_integrity(
            "verify immutable part sections",
            part.verify_sections_with_control(control),
        )?;
        verification_integrity(
            "verify every physical part row",
            part.verify_semantics_with_control(control),
        )?;
        verification_integrity(
            "verify every operational piece dictionary entry",
            part.verify_piece_dictionary_with_control(fold_store, control),
        )?;
        part_sections = part_sections
            .checked_add(sections)
            .ok_or_else(|| anyhow::anyhow!("verified part section count overflow"))?;
    }
    let content = verify_visible_content(parts, fold_store, control)?;
    Ok(StoreVerification {
        chain,
        fold,
        parts: parts.len(),
        part_sections,
        records: content.records,
        content_values: content.values,
        content_bytes: content.bytes,
        content_identities: content.identities,
    })
}

fn verify_retained_piece_dictionaries(
    container: &crate::container::Container,
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<usize> {
    if !container.contains("MANIFEST") {
        return Ok(0);
    }
    let current = Manifest::parse(&container.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?;
    let names = container.names().map(String::from).collect::<Vec<_>>();
    let mut verified_pieces = 0usize;
    for commit in container_retained_commits(container) {
        control.check("piece dictionary verification")?;
        // A retained authority ends at its own fold tail. Checking its dictionary against the
        // current, longer fold would falsely prove a location introduced only by a later revision.
        // The retained opener also applies the current manifest's punched declaration, which is
        // the authority for intentionally unavailable historical payloads.
        let manifest = Manifest::parse(
            &container.read_file_bounded(&format!("MANIFEST.{commit:08}"), MAX_MANIFEST_BYTES)?,
        )?;
        let retained = open_retained_from_container(
            container,
            path,
            cfg,
            commit,
            manifest,
            &current,
            names.clone(),
            read_limits,
        )?;
        for part in &retained.parts {
            verified_pieces = verified_pieces
                .checked_add(part.verify_piece_dictionary_with_control(&retained.fold, control)?)
                .ok_or_else(|| anyhow::anyhow!("verified piece count overflow"))?;
        }
        verification_integrity(
            "verify retained-authority content",
            verify_retained_visible_content(&retained.parts, &retained.fold, control),
        )?;
    }
    Ok(verified_pieces)
}

/// Best-effort cleanup ownership for an artifact that has not crossed its installation rename.
struct UninstalledArtifact {
    path: PathBuf,
    armed: bool,
}

fn artifact_staging_path(out: &Path, operation: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut staging = out.as_os_str().to_os_string();
    staging.push(format!(".{operation}-{}-{serial}", crate::vfs::protocol_process_id()));
    PathBuf::from(staging)
}

impl UninstalledArtifact {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn installed(&mut self) {
        self.armed = false;
    }
}

impl Drop for UninstalledArtifact {
    fn drop(&mut self) {
        if self.armed {
            let _ = crate::vfs::unlink(&self.path);
        }
    }
}

/// Copy a single-file store's current store authority into a fresh container at `out`.
///
/// The backup carries `MANIFEST`, every part it names, and the fold generation referenced by the
/// selected authority — no
/// retained log and no writer state. Staged beside the destination,
/// committed, fully verified as a store, then installed with a rename that refuses to replace. A
/// crash leaves staging litter and an untouched destination.
fn backup_container_copy(
    container: &std::sync::Arc<std::sync::Mutex<crate::container::Container>>,
    manifest: &Manifest,
    cfg: FoldCfg,
    read_limits: ReadLimits,
    out: &Path,
    staging: &Path,
    control: &crate::control::OperationControl,
) -> Result<crate::backup::BackupStats> {
    let mut fresh = crate::container::Container::create_staging(staging)?;
    let mut uninstalled = UninstalledArtifact::new(staging.to_path_buf());

    let c = container.lock().expect("container lock poisoned");
    let prefix = crate::fold::fold_member_prefix(manifest.fold_gen);
    let mut names = Vec::new();
    if manifest.commit != 0 {
        names.push("MANIFEST".to_string());
        names.extend(manifest.parts.iter().map(|p| p.member.clone()));
        let mut fold_members: Vec<String> =
            c.names().filter(|n| n.starts_with(&format!("{prefix}/"))).map(String::from).collect();
        fold_members.sort();
        names.extend(fold_members);
    }

    let mut bytes = 0u64;
    for name in &names {
        control.check("backup")?;
        let reader = c.extent(name).ok_or_else(|| {
            anyhow::anyhow!("the current manifest revision names {name} but the container lost it")
        })?;
        let len = crate::readat::ReadAt::len(&reader)?;
        fresh.put_stream(name, len, |at, into| {
            crate::control::OperationControl::check(control, "backup")
                .map_err(std::io::Error::other)?;
            crate::readat::ReadAt::read_exact_at(&reader, into, at)
        })?;
        bytes += len;
    }
    drop(c);
    control.check("backup")?;
    fresh.commit()?;
    drop(fresh);
    verify_container_artifact(staging, cfg, read_limits, control)
        .with_context(|| format!("verify staged backup {}", staging.display()))?;
    crate::vfs::rename_noreplace(staging, out)?;
    uninstalled.installed();
    if let Some(parent) = out.parent() {
        // The installed name is the result; a failed directory sync means it may not survive a
        // crash, and the operation reports that rather than success.
        crate::vfs::sync_dir(parent).with_context(|| {
            format!("sync {} after installing {}", parent.display(), out.display())
        })?;
    }
    Ok(crate::backup::BackupStats { members: names.len(), bytes, commit: manifest.commit })
}

/// [`verify_chain`] over a single-file store's members: the same walk and claims, with no
/// filesystem namespace between the evidence and the check.
fn verify_chain_container(
    c: &crate::container::Container,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<ChainReport> {
    verify_chain_container_scoped(c, read_limits, control, true)
}

/// [`verify_chain_container`] with the part checks optional. Without them the walk still proves
/// every retained revision's name, parse, adjacency, `prev` link, cursor and tail order, generation,
/// equality between current and newest retained bytes, and the presence of every named part; it
/// reads no part bytes, so its cost is proportional to the retained manifests alone.
fn verify_chain_container_scoped(
    c: &crate::container::Container,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
    check_parts: bool,
) -> Result<ChainReport> {
    let names = c.names().map(String::from).collect::<Vec<_>>();
    let current = if c.contains("MANIFEST") {
        Some(Manifest::parse(&c.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?)
    } else {
        None
    };
    validate_store_member_namespace(&names, current.as_ref(), |name| {
        c.read_file_bounded(name, MAX_MANIFEST_BYTES)
    })?;
    let mut report = ChainReport::default();
    let part_cache = SectionCache::shared();
    let mut verified_parts = HashSet::new();
    for name in c.names().filter(|name| name.starts_with("MANIFEST.")) {
        let rest = &name["MANIFEST.".len()..];
        let commit = rest
            .parse::<u64>()
            .with_context(|| format!("retained manifest member {name:?} has no commit number"))?;
        if name != format!("MANIFEST.{commit:08}") {
            bail!("retained manifest member {name:?} is not in canonical commit form");
        }
    }
    let commits = container_retained_commits(c);
    if commits.len() > MANIFEST_RETAIN {
        bail!(
            "container retains {} manifest revisions, over the current-format limit of {MANIFEST_RETAIN}",
            commits.len()
        );
    }
    report.retained_manifests = commits.len();
    let mut prev_bytes: Option<Vec<u8>> = None;
    let mut prev_commit: Option<u64> = None;
    let mut prev_next_seq: Option<u64> = None;
    let mut prev_fold_tail: Option<(u32, u32)> = None;
    for &commit in &commits {
        control.check("manifest verification")?;
        let bytes = c.read_file_bounded(&format!("MANIFEST.{commit:08}"), MAX_MANIFEST_BYTES)?;
        let m = Manifest::parse(&bytes)
            .with_context(|| format!("retained manifest {commit} is corrupt"))?;
        if m.commit != commit {
            bail!("retained member MANIFEST.{commit:08} contains manifest revision {}", m.commit);
        }
        if current.as_ref().is_some_and(|authority| m.fold_gen != authority.fold_gen) {
            bail!(
                "retained manifest revision {commit} references fold generation {}, but current authority references {}; refold must purge its predecessors",
                m.fold_gen,
                current.as_ref().expect("checked as present").fold_gen
            );
        }
        if let Some(previous) = prev_commit {
            let expected = previous
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("retained manifest revision order overflows"))?;
            if commit != expected {
                bail!(
                    "manifest chain has a revision gap: retained revision {previous} is followed by {commit}, expected {expected}"
                );
            }
        }
        if prev_next_seq.is_some_and(|previous| m.next_seq < previous) {
            bail!(
                "manifest chain moves the record-version cursor backward from {} to {} at revision {commit}",
                prev_next_seq.expect("checked as present"),
                m.next_seq
            );
        }
        let fold_tail = (m.fold_seg, m.fold_off);
        if prev_fold_tail.is_some_and(|previous| fold_tail < previous) {
            bail!(
                "manifest chain moves the fold tail backward from {:?} to {:?} at revision {commit}",
                prev_fold_tail.expect("checked as present"),
                fold_tail
            );
        }
        if let Some(pb) = &prev_bytes {
            let want = m.prev.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "manifest chain broken: retained commit {commit} has no predecessor digest"
                )
            })?;
            let got = blake3::hash(pb).to_hex().to_string();
            if *want != got {
                bail!(
                    "manifest chain broken: commit {commit} names prev {want} but the previous \
                     retained commit hashes to {got}"
                );
            }
            report.links += 1;
        }
        for p in &m.parts {
            control.check("manifest verification")?;
            if !check_parts {
                if !c.contains(&p.member) {
                    bail!(
                        "part {} named by commit {commit} is not held by the container",
                        p.member
                    );
                }
                continue;
            }
            {
                let want = &p.b3;
                let reader = c.extent(&p.member).ok_or_else(|| {
                    anyhow::anyhow!(
                        "part {} named by commit {commit} is not held by the container",
                        p.member
                    )
                })?;
                let got = hash_reader_with_control(&reader, control, "manifest verification")
                    .with_context(|| format!("part {} named by commit {commit}", p.member))?
                    .to_hex()
                    .to_string();
                if *want != got {
                    bail!("part {} drifted from the digest commit {commit} pinned", p.member);
                }
                report.part_digests += 1;
            }
            if verified_parts.insert(p.member.clone()) {
                let reader = c.extent(&p.member).expect("part presence proved while hashing");
                let part = open_manifest_part(Box::new(reader), p, part_cache.clone(), read_limits)
                    .with_context(|| {
                        format!("open part {} named by retained manifest {commit}", p.member)
                    })?;
                part.verify_sections_with_control(control)
                    .with_context(|| format!("verify sections of retained part {}", p.member))?;
                part.verify_semantics_with_control(control).with_context(|| {
                    format!("verify physical rows of retained part {}", p.member)
                })?;
            }
        }
        prev_bytes = Some(bytes);
        prev_commit = Some(commit);
        prev_next_seq = Some(m.next_seq);
        prev_fold_tail = Some(fold_tail);
    }
    if let (Some(&newest), Some(pb)) = (commits.last(), &prev_bytes) {
        let live = c.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?;
        if live != *pb {
            bail!("MANIFEST diverges from its retained copy at commit {newest}");
        }
        report.links += 1;
    } else if c.contains("MANIFEST") {
        // Backup artifacts intentionally omit retained history. Their current manifest is still
        // authority, so its part pins must be checked directly rather than disappearing with the
        // chain that normally duplicates the live revision.
        let bytes = c.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?;
        let manifest = Manifest::parse(&bytes).context("current manifest is corrupt")?;
        for part in &manifest.parts {
            control.check("manifest verification")?;
            if !check_parts {
                if !c.contains(&part.member) {
                    bail!(
                        "part {} named by the current manifest revision is not held by the container",
                        part.member
                    );
                }
                continue;
            }
            {
                let want = &part.b3;
                let reader = c.extent(&part.member).ok_or_else(|| {
                    anyhow::anyhow!(
                        "part {} named by the current manifest revision is not held by the container",
                        part.member
                    )
                })?;
                let got = hash_reader_with_control(&reader, control, "manifest verification")?
                    .to_hex()
                    .to_string();
                if *want != got {
                    bail!(
                        "part {} drifted from the digest the current manifest revision pinned",
                        part.member
                    );
                }
                report.part_digests += 1;
            }
            if verified_parts.insert(part.member.clone()) {
                let reader = c.extent(&part.member).expect("part presence proved while hashing");
                let opened =
                    open_manifest_part(Box::new(reader), part, part_cache.clone(), read_limits)
                        .with_context(|| {
                            format!(
                                "open part {} named by the current manifest revision",
                                part.member
                            )
                        })?;
                opened
                    .verify_sections_with_control(control)
                    .with_context(|| format!("verify sections of current part {}", part.member))?;
                opened.verify_semantics_with_control(control).with_context(|| {
                    format!("verify physical rows of current part {}", part.member)
                })?;
            }
        }
    } else if !c.committed_is_empty_birth() {
        // Sequence zero is the one valid manifestless state: a newly created store before its
        // first publication. Once any container state has committed, losing MANIFEST is losing
        // the authority for every remaining member, never a return to emptiness.
        bail!("committed container sequence {} has no MANIFEST authority", c.seq());
    }
    Ok(report)
}

/// Validate exactly the container bytes that artifact installation would make reachable.
/// This is read-only: it verifies the member directory and payload checksums, opens every
/// manifest-named part and fold member, walks the retained chain, scrubs every section/frame, and
/// reconstructs every live named content value.
pub(crate) fn verify_container_artifact(
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<(usize, u64, u64, StoreVerification)> {
    let container = verification_integrity(
        "open container for verification",
        crate::container::Container::open_internal_with_limits(path, read_limits),
    )?;
    read_limits.admit_directory_entries(
        format!("container {} member directory", path.display()),
        container.len() as u64,
    )?;
    let members = verification_integrity(
        "verify container members",
        container.verify_with_store_profile_and_control(cfg, read_limits, control),
    )?;
    let member_bytes = container.member_bytes();
    let chain = verification_integrity(
        "verify retained manifest chain",
        verify_chain_container(&container, read_limits, control),
    )?;
    let store = verification_integrity(
        "open container store for verification",
        open_read_container_handle(&container, cfg, path, read_limits),
    )?;
    verification_integrity(
        "verify retained-authority piece dictionaries",
        verify_retained_piece_dictionaries(&container, path, cfg, read_limits, control),
    )?;
    drop(container);
    let commit = store.manifest.commit;
    let report = verification_integrity(
        "verify complete container store",
        verify_committed_store(&store.parts, &store.fold, chain, control),
    )?;
    Ok((members, member_bytes, commit, report))
}

fn verification_integrity<T>(context: &'static str, result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if crate::error::classify(&error) == crate::error::ErrorClass::Internal {
            crate::error::IntegrityError::new(context, error).into()
        } else {
            error
        }
    })
}

/// Explicit rollback bounds for checked manifest promotion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ManifestPromotionOptions {
    /// Maximum number of newer retained commits that may be abandoned. Zero repairs only to the
    /// newest retained commit and is the safe default.
    pub max_rollback_commits: u64,
}

/// Evidence returned after checked manifest promotion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ManifestPromotionReport {
    pub commit: u64,
    pub rollback_commits: u64,
    /// Retained revisions older than the promoted one that were abandoned because they, or the
    /// link joining them to the survivors, no longer validated. Retention shrinks by this many.
    pub abandoned_retained_revisions: usize,
    pub records: usize,
    pub content_values: usize,
    pub parts: usize,
    pub part_sections: usize,
    pub fold_segments: u32,
    pub fold_blocks: usize,
    pub fold_bytes: u64,
}

#[derive(Debug)]
pub enum ManifestPromotionError {
    Healthy(PathBuf),
    RollbackLimit { needed: u64, allowed: u64 },
    NoUsableCandidate { examined: usize, reason: String },
}

impl std::fmt::Display for ManifestPromotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestPromotionError::Healthy(path) => write!(
                f,
                "MANIFEST at {} is intact and its retained history validates; refusing manifest promotion for an intact store",
                path.display()
            ),
            ManifestPromotionError::RollbackLimit { needed, allowed } => write!(
                f,
                "manifest promotion needs to abandon {needed} newer retained revisions but only {allowed} were authorized"
            ),
            ManifestPromotionError::NoUsableCandidate { examined, reason } => write!(
                f,
                "none of {examined} retained manifest revisions is a fully readable promotion candidate: {reason}"
            ),
        }
    }
}

impl std::error::Error for ManifestPromotionError {}

/// Failures that say nothing about the bytes: interruption, admission, and the filesystem itself.
/// Manifest promotion propagates these instead of treating them as damage to work around.
fn environmental_failure(error: &anyhow::Error) -> bool {
    matches!(
        crate::error::classify(error),
        crate::error::ErrorClass::Cancelled
            | crate::error::ErrorClass::ResourceExhausted
            | crate::error::ErrorClass::Io
            | crate::error::ErrorClass::NotFound
            | crate::error::ErrorClass::Unsupported
    )
}

fn validate_surviving_promotion_history(
    container: &crate::container::Container,
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
    selected: &Manifest,
) -> Result<SurvivingHistory> {
    let mut admitted_parts: HashSet<String> =
        selected.parts.iter().map(|part| part.member.clone()).collect();
    let mut abandoned_older = Vec::new();
    let mut newer = selected.clone();
    let older = container_retained_commits(container)
        .into_iter()
        .filter(|commit| *commit < selected.commit)
        .rev();
    for commit in older {
        control.check("manifest promotion retained-history validation")?;
        if !abandoned_older.is_empty() {
            // Nothing older than a break can be linked to the survivors, so it is abandoned
            // with the revision that broke the chain.
            abandoned_older.push(commit);
            continue;
        }
        let bytes =
            container.read_file_bounded(&format!("MANIFEST.{commit:08}"), MAX_MANIFEST_BYTES)?;
        match validate_retained_predecessor(
            container,
            path,
            cfg,
            read_limits,
            control,
            &bytes,
            &newer,
        ) {
            Ok(manifest) => {
                admitted_parts.extend(manifest.parts.iter().map(|part| part.member.clone()));
                newer = manifest;
            }
            Err(error) if environmental_failure(&error) => return Err(error),
            Err(_) => abandoned_older.push(commit),
        }
    }
    Ok(SurvivingHistory { admitted_parts, abandoned_older })
}

/// The retained history that survives beside a promotion candidate.
struct SurvivingHistory {
    /// Parts named by the candidate or by any surviving older revision.
    admitted_parts: HashSet<String>,
    /// Older retained revisions abandoned because they, or the link that would join them to the
    /// survivors, no longer validate. Retention ends for them in the promoting flip.
    abandoned_older: Vec<u64>,
}

/// Prove that `bytes` is the immediate, fully usable predecessor of `newer`: adjacent revision
/// number, valid current-format manifest, the same fold generation, named by `newer`'s `prev`,
/// monotonic cursor and tail, and content that reconstructs at its own tail.
fn validate_retained_predecessor(
    container: &crate::container::Container,
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
    bytes: &[u8],
    newer: &Manifest,
) -> Result<Manifest> {
    let manifest = Manifest::parse(bytes).context("retained manifest revision is invalid")?;
    let expected = newer.commit.checked_sub(1).context("retained revision order underflows")?;
    if manifest.commit != expected {
        bail!(
            "retained revision {} does not immediately precede revision {}",
            manifest.commit,
            newer.commit
        );
    }
    if manifest.fold_gen != newer.fold_gen {
        bail!(
            "retained revision {} references fold generation {} but revision {} references {}",
            manifest.commit,
            manifest.fold_gen,
            newer.commit,
            newer.fold_gen
        );
    }
    let link = blake3::hash(bytes).to_hex().to_string();
    if newer.prev.as_deref() != Some(link.as_str()) {
        bail!(
            "retained revision {} does not name revision {} as its predecessor",
            newer.commit,
            manifest.commit
        );
    }
    if manifest.next_seq > newer.next_seq {
        bail!(
            "retained history moves the record-version cursor backward from {} to {} at revision {}",
            manifest.next_seq,
            newer.next_seq,
            newer.commit
        );
    }
    if (manifest.fold_seg, manifest.fold_off) > (newer.fold_seg, newer.fold_off) {
        bail!(
            "retained history moves the fold tail backward from {:?} to {:?} at revision {}",
            (manifest.fold_seg, manifest.fold_off),
            (newer.fold_seg, newer.fold_off),
            newer.commit
        );
    }
    validate_recovery_candidate_container(
        container,
        path,
        cfg,
        manifest.clone(),
        read_limits,
        control,
    )?;
    Ok(manifest)
}

/// Checked manifest promotion for a single-file store: OS-enforced writer role on native container
/// handles, a
/// healthy refusal by the same rule, candidates validated whole at their exact tails, and
/// promotion as ONE flip that restages `MANIFEST` verbatim and removes the abandoned newer
/// retained members in the same atomic state.
pub fn promote_manifest_file(
    path: &Path,
    cfg: FoldCfg,
    options: ManifestPromotionOptions,
) -> Result<ManifestPromotionReport> {
    promote_manifest_file_with_limits_and_control(
        path,
        cfg,
        options,
        ReadLimits::default(),
        &crate::control::OperationControl::default(),
    )
}

/// [`promote_manifest_file`] with explicit admission and cooperative cancellation; the last
/// checkpoint is immediately before the publishing flip.
pub fn promote_manifest_file_with_limits_and_control(
    path: &Path,
    cfg: FoldCfg,
    options: ManifestPromotionOptions,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<ManifestPromotionReport> {
    debris::validate_store_path(path)?;
    crate::fold::validate_cfg(cfg)?;
    let read_limits = read_limits.validate()?;
    control.check("manifest promotion")?;
    let mut container = crate::container::Container::open_internal_with_limits(path, read_limits)?
        .lock_writer_current()?;
    control.check("manifest promotion")?;
    if container.contains("MANIFEST") {
        let current = container.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?;
        if Manifest::parse(&current).is_ok() {
            // Intact current authority alone is not health: retained history that no longer
            // validates refuses every open just as surely, and promotion at rollback zero is the
            // one operation authorized to end retention of what cannot be reopened.
            match verify_chain_container(&container, read_limits, control) {
                Ok(_) => return Err(ManifestPromotionError::Healthy(path.to_path_buf()).into()),
                Err(error) if environmental_failure(&error) => return Err(error),
                Err(_) => {}
            }
        }
    }
    let commits = container_retained_commits(&container);
    let member_names = container.names().map(String::from).collect::<Vec<_>>();
    validate_manifest_promotion_member_namespace(&member_names)?;
    let mut retained_generation = None;
    for &revision in &commits {
        let bytes =
            container.read_file_bounded(&format!("MANIFEST.{revision:08}"), MAX_MANIFEST_BYTES)?;
        let Ok(candidate) = Manifest::parse(&bytes) else { continue };
        if candidate.commit != revision {
            bail!(
                "retained member MANIFEST.{revision:08} contains manifest revision {}",
                candidate.commit
            );
        }
        if let Some(expected) = retained_generation {
            if candidate.fold_gen != expected {
                bail!(
                    "retained manifest history crosses fold generations {expected} and {}; refold must purge its predecessors",
                    candidate.fold_gen
                );
            }
        } else {
            retained_generation = Some(candidate.fold_gen);
        }
    }
    let newest = commits.last().copied().unwrap_or(0);
    let mut examined = 0usize;
    let mut last_reason = "no retained manifests exist".to_string();
    for c in commits.into_iter().rev() {
        control.check("manifest promotion validation")?;
        examined += 1;
        let bytes = container.read_file_bounded(&format!("MANIFEST.{c:08}"), MAX_MANIFEST_BYTES)?;
        let manifest = match Manifest::parse(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                last_reason = error.to_string();
                continue;
            }
        };
        let (cand_gen, cand_seg, cand_off) =
            (manifest.fold_gen, manifest.fold_seg, manifest.fold_off);
        match validate_recovery_candidate_container(
            &container,
            path,
            cfg,
            manifest.clone(),
            read_limits,
            control,
        ) {
            Ok(mut report) => {
                let rollback_commits = newest.saturating_sub(c);
                if rollback_commits > options.max_rollback_commits {
                    return Err(ManifestPromotionError::RollbackLimit {
                        needed: rollback_commits,
                        allowed: options.max_rollback_commits,
                    }
                    .into());
                }
                let history = match validate_surviving_promotion_history(
                    &container,
                    path,
                    cfg,
                    read_limits,
                    control,
                    &manifest,
                ) {
                    Ok(history) => history,
                    Err(error) => {
                        if environmental_failure(&error) {
                            return Err(error);
                        }
                        last_reason = error.to_string();
                        continue;
                    }
                };
                let abandoned: HashSet<u64> = history.abandoned_older.iter().copied().collect();
                // No cancellation checkpoint after this point: promotion is one flip, and its
                // selected authority or an explicit ambiguous-durability error must be reported.
                control.check("manifest promotion publication")?;
                container.put_bytes("MANIFEST", &bytes)?;
                for name in container.names().map(String::from).collect::<Vec<_>>() {
                    if let Some(revision) =
                        name.strip_prefix("MANIFEST.").and_then(|suffix| suffix.parse::<u64>().ok())
                    {
                        if revision > c || abandoned.contains(&revision) {
                            container.remove(&name)?;
                        }
                        continue;
                    }
                    let unreferenced_part = canonical_part_member_shape(&name)
                        && !history.admitted_parts.contains(&name);
                    let abandoned_fold = exact_fold_member_name(&name)
                        && fold_generation_of_member(&name) != Some(cand_gen);
                    if unreferenced_part || abandoned_fold {
                        container.remove(&name)?;
                    }
                }
                // The abandoned commits may have grown the fold past the candidate's tail —
                // advanced its active segment, sealed it, rolled new ones. The promoted state
                // must AGREE with its own manifest, so the same flip truncates the active
                // segment member to the candidate's tail and drops every same-generation
                // member past it. (Rollback across a re-fold cannot happen: a re-fold purges
                // the retained log, so no candidate from the old generation survives.)
                let fold_rel =
                    if cand_gen == 0 { "fold".to_string() } else { format!("fold-{cand_gen:04}") };
                let keep_seg = cand_seg;
                let keep_len = u64::from(cand_off);
                for name in container.names().map(String::from).collect::<Vec<_>>() {
                    let Some(rest) = name.strip_prefix(&format!("{fold_rel}/")) else { continue };
                    if let Some(n) = crate::fold::segment::parse_seg_name(rest) {
                        if n > keep_seg {
                            container.remove(&name)?;
                        } else if n == keep_seg && container.member_len(&name) != Some(keep_len) {
                            let ext = container.extent(&name).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "container lost member {name} during manifest promotion"
                                )
                            })?;
                            container.put_stream(&name, keep_len, |at, into| {
                                crate::readat::ReadAt::read_exact_at(&ext, into, at)
                            })?;
                            // A sidecar describes the sealed segment this member no longer is.
                            container.remove(&format!("{fold_rel}/seg-{n:08}.dir"))?;
                        }
                    } else if let Some(n) = rest
                        .strip_prefix("seg-")
                        .and_then(|r| r.strip_suffix(".dir"))
                        .and_then(|digits| digits.parse::<u32>().ok())
                    {
                        if n > keep_seg {
                            container.remove(&name)?;
                        }
                    }
                }
                let final_names = container.names().map(String::from).collect::<Vec<_>>();
                validate_store_member_namespace(&final_names, Some(&manifest), |name| {
                    container.read_file_bounded(name, MAX_MANIFEST_BYTES)
                })?;
                if let Err(error) = container.commit() {
                    if container.failed_publication_selected() == Some(true) {
                        return Err(error).context(
                            "manifest promotion is selected by the current container state, but its final synchronization failed; reopen to determine crash durability",
                        );
                    }
                    return Err(error).context("publish the promoted manifest revision");
                }
                report.commit = c;
                report.rollback_commits = rollback_commits;
                report.abandoned_retained_revisions = history.abandoned_older.len();
                return Ok(report);
            }
            Err(error) => {
                if environmental_failure(&error) {
                    return Err(error);
                }
                last_reason = error.to_string();
            }
        }
    }
    Err(ManifestPromotionError::NoUsableCandidate { examined, reason: last_reason }.into())
}

/// The home-neutral half of candidate validation: every visible record's every content value
/// reconstructed piece by piece, lengths checked, whole-value identities checked where carried.
fn validate_candidate_records(
    reader: &ReadStore,
    control: &crate::control::OperationControl,
) -> Result<(usize, usize)> {
    let visible = read::visibility(&reader.parts)?;
    let mut records = 0usize;
    let mut content_values = 0usize;
    for (part_index, rows) in visible.rows.iter().enumerate() {
        let part = &reader.parts[part_index];
        for &row in rows {
            control.check("manifest promotion validation")?;
            let record = part.record(row)?;
            records = records
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("candidate record count overflow"))?;
            for content in &record.contents {
                control.check("manifest promotion validation")?;
                // Interpret the program through the owning Part—the same authority ordinary reads
                // use—and hash each literal/piece incrementally rather than allocating the value.
                part.verify_projected_content_with_control(content, &reader.fold, control)?;
                content_values = content_values
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("candidate content count overflow"))?;
            }
        }
    }
    Ok((records, content_values))
}

/// [`validate_recovery_candidate`] for a candidate held as members: the fold opens over extent
/// readers bounded to the candidate's EXACT tail — the active segment sliced to `fold_off`,
/// segments above it excluded entirely — so validation sees precisely the fold the candidate
/// committed, not whatever later commits appended to the same members.
fn validate_recovery_candidate_container(
    c: &crate::container::Container,
    path: &Path,
    cfg: FoldCfg,
    manifest: Manifest,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<ManifestPromotionReport> {
    control.check("manifest promotion validation")?;
    let prefix = crate::fold::fold_member_prefix(manifest.fold_gen);
    let tail = manifest.fold_tail();
    let mut segs = Vec::new();
    let mut dict_files = Vec::new();
    let mut present_segments = HashSet::new();
    let mut sidecars = HashSet::new();
    for name in c.names().map(String::from).collect::<Vec<_>>() {
        let Some(rest) = name.strip_prefix(&format!("{prefix}/")) else { continue };
        if let Some(n) = crate::fold::segment::parse_seg_name(rest) {
            present_segments.insert(n);
            let extent = c.extent(&name).expect("name came from this container");
            let full_len = crate::readat::ReadAt::len(&extent)?;
            let (reader, len, whole): (Arc<dyn crate::readat::ReadAt>, u64, bool) = match tail {
                Some(t) if n > t.seg => continue,
                Some(t) if n == t.seg => (
                    Arc::new(crate::readat::Slice::new(extent, 0, u64::from(t.off))),
                    u64::from(t.off),
                    u64::from(t.off) == full_len,
                ),
                _ => (Arc::new(extent), full_len, true),
            };
            segs.push(crate::fold::SegmentInput {
                seg: n,
                reader,
                // A sidecar describes a segment's full length; for a candidate-bounded segment
                // it would fail its own staleness gate, so it is only offered when whole.
                sidecar: if whole {
                    container_sidecar_bytes(
                        c,
                        &format!("{prefix}/seg-{n:08}.dir"),
                        len,
                        read_limits,
                    )?
                } else {
                    None
                },
            });
        } else if let Some(number) = fold_sidecar_member(rest) {
            sidecars.insert(number);
        } else if valid_fold_dictionary_member(rest) {
            let bytes = c.read_file_bounded(&name, crate::fold::MAX_DICTIONARY_BYTES)?;
            let exact = format!("zdict-{}.zd", PieceHash::of(&bytes).to_hex());
            if rest != exact {
                bail!("fold dictionary member {name:?} does not match its content identity");
            }
            dict_files.push(bytes);
        } else {
            bail!("container member {name:?} is not in the current fold namespace");
        }
    }
    if let Some(orphan) = sidecars.iter().find(|number| !present_segments.contains(number)) {
        bail!("fold sidecar seg-{orphan:08}.dir has no corresponding segment");
    }
    if let Some(tail) = tail {
        if !present_segments.contains(&tail.seg) {
            bail!("manifest-promotion candidate tail names absent segment {}", tail.seg);
        }
    }
    let fold = Fold::open_read_from_with_limits(
        segs,
        dict_files,
        cfg,
        path,
        &manifest.punched,
        read_limits,
    )?;
    let scrub = fold.scrub_with_control(control)?;
    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    let mut part_sections = 0usize;
    for part_ref in &manifest.parts {
        control.check("manifest promotion validation")?;
        let reader = c.extent(&part_ref.member).ok_or_else(|| {
            anyhow::anyhow!(
                "candidate commit {} names part {} but the container does not hold it",
                manifest.commit,
                part_ref.member
            )
        })?;
        let got = hash_reader_with_control(&reader, control, "manifest promotion validation")?
            .to_hex()
            .to_string();
        if got != part_ref.b3 {
            bail!(
                "candidate commit {} names part {} with the wrong digest",
                manifest.commit,
                part_ref.member
            );
        }
        let part =
            Arc::new(open_manifest_part(Box::new(reader), part_ref, pcache.clone(), read_limits)?);
        part_sections += part.verify_sections_with_control(control)?;
        part.verify_semantics_with_control(control)?;
        part.verify_piece_dictionary_with_control(&fold, control)?;
        parts.push(part);
    }
    let reader = ReadStore { fold: Arc::new(fold), parts, manifest, read_limits };
    let (records, content_values) = validate_candidate_records(&reader, control)?;
    Ok(ManifestPromotionReport {
        records,
        content_values,
        parts: reader.parts.len(),
        part_sections,
        fold_segments: scrub.segments,
        fold_blocks: scrub.blocks,
        fold_bytes: scrub.bytes,
        ..ManifestPromotionReport::default()
    })
}

/// Hash bytes that live behind a bounded reader.
fn hash_reader_with_control(
    reader: &dyn crate::readat::ReadAt,
    control: &crate::control::OperationControl,
    operation: &'static str,
) -> Result<blake3::Hash> {
    let len = reader.len()?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut at = 0u64;
    while at < len {
        control.check(operation)?;
        let take = buf.len().min((len - at) as usize);
        reader.read_exact_at(&mut buf[..take], at)?;
        hasher.update(&buf[..take]);
        at += take as u64;
    }
    Ok(hasher.finalize())
}

/// `<store>` is absent and `<store>.reclaimed` — a reclaim's anchor — is present: reinstate the
/// anchor to the store's name, or refuse. One contender at a time: the anchor's own writer lock
/// is the exclusion, so a second recoverer sees `WriterLocked`. The anchor is validated whole
/// (fold at its tail scrubbed, every part by digest and sections, every record reconstructed —
/// the same bar as manifest promotion) BEFORE anything is created; a corrupt or incomplete anchor
/// refuses and leaves everything as found. Reinstatement is a copy to a fresh candidate, fsynced,
/// writer-locked, and placed at `<store>` by a write-through no-replace rename — a name taken
/// meanwhile refuses with the anchor intact — after which the anchor is unlinked (laggable). A
/// crash at any point re-enters here on the next writer open and converges.
fn restore_store_from_reclaim_anchor(
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
) -> Result<()> {
    let names = crate::container::reclaim_names(path);
    let anchor = crate::container::Container::open_internal_with_limits(&names.anchor, read_limits)
        .with_context(|| format!("open reclaim anchor {}", names.anchor.display()))?
        .lock_writer_current()
        .with_context(|| "another interrupted-reclaim restoration is in progress")?;
    let control = crate::control::OperationControl::default();
    verify_container_artifact(&names.anchor, cfg, read_limits, &control).with_context(|| {
        format!(
            "reclaim anchor {} does not validate whole; the store is not recreated from it",
            names.anchor.display()
        )
    })?;
    if !anchor.is_current_path_file(&names.anchor)? {
        bail!("reclaim anchor changed identity while it was being validated");
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    crate::vfs::unlink_if_exists(&names.candidate_tmp).with_context(|| {
        format!("remove stale reclaim candidate {}", names.candidate_tmp.display())
    })?;
    crate::vfs::unlink_if_exists(&names.candidate)
        .with_context(|| format!("remove stale reclaim candidate {}", names.candidate.display()))?;
    crate::container::copy_container_bytes_pub(&names.anchor, &names.candidate_tmp)?;
    crate::vfs::sync_dir(&parent)?;
    crate::vfs::rename_noreplace(&names.candidate_tmp, &names.candidate)?;
    crate::vfs::sync_dir(&parent)?;
    let new_store = crate::vfs::open_rw(&names.candidate)?;
    if !crate::sys::lock_exclusive(&new_store)? {
        return Err(crate::fold::WriterLocked { path: names.candidate.clone() }.into());
    }
    verify_container_artifact(&names.candidate, cfg, read_limits, &control)?;
    crate::vfs::rename_noreplace(&names.candidate, path).with_context(|| {
        format!("{} was taken while the reclaim anchor was being promoted", path.display())
    })?;
    crate::vfs::sync_dir(&parent)?;
    drop(anchor);
    crate::vfs::unlink_if_exists(&names.anchor)
        .with_context(|| format!("remove reclaim anchor {}", names.anchor.display()))?;
    crate::vfs::sync_dir(&parent)
        .with_context(|| format!("sync {} after removing the reclaim anchor", parent.display()))?;
    drop(new_store);
    Ok(())
}

/// The retained manifest revisions a single-file store holds, oldest first — the revisions
/// [`open_read_container_at`] can still serve.
pub fn retained_commits_file(path: &Path) -> Result<Vec<u64>> {
    let c = crate::container::Container::open(path)?;
    verify_chain_container(
        &c,
        ReadLimits::default(),
        &crate::control::OperationControl::default(),
    )?;
    Ok(container_retained_commits(&c))
}

/// Verify a single-file store's retained manifest chain: prev-links across the retained
/// members, every part pin hashed against the extents the file actually holds, and the live
/// manifest checked byte-identical to its newest retained copy.
pub fn verify_chain_file(path: &Path) -> Result<ChainReport> {
    let c = crate::container::Container::open(path)?;
    verify_chain_container(&c, ReadLimits::default(), &crate::control::OperationControl::default())
}

/// Restore a single-file backup: copy `src` into staging, fully verify that exact staged store,
/// then install it at `dst` with a rename that refuses to replace. The backup is itself a store,
/// so restoring is verified copying, and a crash leaves staging litter and an untouched
/// destination.
pub fn restore_file(src: &Path, dst: &Path) -> Result<crate::backup::RestoreStats> {
    restore_file_with_control_and_limits(
        src,
        dst,
        ReadLimits::default(),
        &crate::control::OperationControl::default(),
    )
}

/// [`restore_file`] with explicit admission limits for the staged artifact.
pub fn restore_file_with_limits(
    src: &Path,
    dst: &Path,
    read_limits: ReadLimits,
) -> Result<crate::backup::RestoreStats> {
    restore_file_with_control_and_limits(
        src,
        dst,
        read_limits,
        &crate::control::OperationControl::default(),
    )
}

/// [`restore_file`] with cooperative cancellation; the last checkpoint is immediately before the
/// installing rename, and a cancelled restore removes its staging and never installs.
pub fn restore_file_with_control(
    src: &Path,
    dst: &Path,
    control: &crate::control::OperationControl,
) -> Result<crate::backup::RestoreStats> {
    restore_file_with_control_and_limits(src, dst, ReadLimits::default(), control)
}

/// [`restore_file`] with both explicit staged-artifact admission and cooperative cancellation.
pub fn restore_file_with_control_and_limits(
    src: &Path,
    dst: &Path,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<crate::backup::RestoreStats> {
    if !crate::backup::ATOMIC_RESTORE {
        return Err(crate::backup::BackupError::Unsupported(
            "this target has no atomic no-replace artifact-installation primitive".to_string(),
        )
        .into());
    }
    let read_limits = read_limits.validate()?;
    debris::validate_store_path(src)?;
    debris::validate_store_path(dst)?;
    control.check("backup restore")?;
    crate::backup::ensure_destination_available(dst)?;
    let staging = artifact_staging_path(dst, "restoring");
    crate::backup::ensure_source_is_not_staging(src, &staging)?;
    // The copy goes through the vfs seam in bounded chunks and is fsynced before anything
    // depends on it: the installing rename must never make bytes reachable that a crash could
    // still take back — and a copy the crash simulator cannot see is a copy the crash-safety
    // argument does not cover.
    let mut uninstalled = {
        use std::io::Read;
        let mut from =
            crate::vfs::open_read(src).with_context(|| format!("open backup {}", src.display()))?;
        let to = crate::vfs::create_new_staging(&staging)
            .with_context(|| format!("stage restore at {}", staging.display()))?;
        let cleanup = UninstalledArtifact::new(staging.clone());
        let mut buf = vec![0u8; 1 << 20];
        let mut at = 0u64;
        loop {
            let n = from.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crate::vfs::write_all_at(&to, &staging, &buf[..n], at)?;
            at += n as u64;
        }
        crate::vfs::sync_file(&to, &staging)?;
        cleanup
    };
    let (members, bytes, commit, _) =
        verify_container_artifact(&staging, FoldCfg::default(), read_limits, control)
            .with_context(|| format!("verify staged restore {}", staging.display()))?;
    control.check("backup restore installation")?;
    crate::vfs::rename_noreplace(&staging, dst)?;
    uninstalled.installed();
    if let Some(parent) = dst.parent() {
        crate::vfs::sync_dir(parent).with_context(|| {
            format!("sync {} after installing {}", parent.display(), dst.display())
        })?;
    }
    Ok(crate::backup::RestoreStats { members, bytes, commit })
}

/// Retained commit numbers held as `MANIFEST.NNNNNNNN` members, parsed numerically exactly as
/// their file forms are, ascending.
fn container_retained_commits(c: &crate::container::Container) -> Vec<u64> {
    let mut commits: Vec<u64> = c
        .names()
        .filter_map(|name| name.strip_prefix("MANIFEST."))
        .filter(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|rest| rest.parse::<u64>().ok())
        .collect();
    commits.sort_unstable();
    commits
}

/// The fold generation a member name belongs to, if it is a fold member at all.
fn fold_generation_of_member(name: &str) -> Option<u32> {
    let (prefix, _) = name.split_once('/')?;
    if prefix == "fold" {
        Some(0)
    } else {
        let digits = prefix.strip_prefix("fold-")?;
        if digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_digit()) {
            digits
                .parse::<u32>()
                .ok()
                .filter(|generation| (1..=crate::fold::MAX_FOLD_GENERATION).contains(generation))
        } else {
            None
        }
    }
}

/// Fold segment members whose outer container checksum was intentionally invalidated by content
/// punch. Their frame-level validation remains mandatory; only the redundant whole-member CRC is
/// inapplicable after an authorized payload has been deallocated or partially deallocated.
pub(crate) fn verified_punched_fold_members(
    container: &crate::container::Container,
    cfg: FoldCfg,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<HashSet<String>> {
    if !container.contains("MANIFEST") {
        return Ok(HashSet::new());
    }
    let manifest = Manifest::parse(&container.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?;
    if manifest.punched.is_empty() {
        return Ok(HashSet::new());
    }
    let fold = open_fold_from_container(container, &manifest, cfg, container.path(), read_limits)?;
    fold.scrub_with_control(control)?;
    let prefix = crate::fold::fold_member_prefix(manifest.fold_gen);
    let punched =
        |block_id: u32| manifest.punched.iter().any(|&(lo, hi)| (lo..=hi).contains(&block_id));
    Ok(fold
        .block_inventory_with_control(control)?
        .into_iter()
        .filter(|block| punched(block.block_id))
        .map(|block| format!("{prefix}/{}", crate::fold::segment::seg_name(block.segment)))
        .collect())
}

/// The sweep's single-file form: move members no manifest — live or retained — names onto the
/// free list. Same keep-set rule as the directory sweep, with "unlink" replaced by "free": the
/// bytes stay where they are, never reused, reclaimable by punch or rewrite. Staged only; the
/// caller's flip publishes the frees with everything else.
fn sweep_unreachable_container(
    c: &mut crate::container::Container,
    live: &Manifest,
    _read_limits: ReadLimits,
) -> Result<()> {
    let mut keep_parts: std::collections::HashSet<String> =
        live.parts.iter().map(|p| p.member.clone()).collect();
    let mut keep_gens: std::collections::HashSet<u32> = std::iter::once(live.fold_gen).collect();
    for commit in container_retained_commits(c) {
        let name = format!("MANIFEST.{commit:08}");
        let bytes = c
            .read_file_bounded(&name, MAX_MANIFEST_BYTES)
            .with_context(|| format!("read retained manifest {commit} before member sweep"))?;
        let m = Manifest::parse(&bytes)
            .with_context(|| format!("validate retained manifest {commit} before member sweep"))?;
        keep_parts.extend(m.parts.into_iter().map(|p| p.member));
        keep_gens.insert(m.fold_gen);
    }
    let names: Vec<String> = c.names().map(String::from).collect();
    for name in names {
        let unreachable = if name.starts_with("part-") && name.ends_with(".part") {
            !keep_parts.contains(&name)
        } else if let Some(gen) = fold_generation_of_member(&name) {
            !keep_gens.contains(&gen)
        } else {
            false
        };
        if unreachable {
            c.remove(&name)?;
        }
    }
    Ok(())
}

/// The WAL a single-file store keeps beside it while hot: `<store>.turndb-wal`, mirroring
/// SQLite's `-wal`. Present at open means a crash; removed on clean close.
fn file_wal_path(store: &Path) -> PathBuf {
    let mut p = store.as_os_str().to_os_string();
    p.push("-wal");
    PathBuf::from(p)
}

/// The transient scratch directory a single-file store's maintenance uses for merge spools:
/// `<store>.turndb-tmp/`. Exists only while an operation runs; never holds durable state; swept
/// whole at writer open, which is what makes crashed-merge cleanup O(1) instead of a scan of
/// whatever directory the store file happens to live in.
fn file_tmp_dir(store: &Path) -> PathBuf {
    let mut p = store.as_os_str().to_os_string();
    p.push("-tmp");
    PathBuf::from(p)
}

pub struct Store {
    path: PathBuf,
    container: std::sync::Arc<std::sync::Mutex<crate::container::Container>>,
    fold: Fold,
    parts: Vec<Arc<Part>>,
    manifest: Manifest,
    /// Uncommitted records, last-write-wins by id.
    /// Uncommitted records. `None` is a staged DELETION — it must be a value rather than an absence,
    /// because it has to shadow whatever older parts still say about the id.
    mem: BTreeMap<String, Option<Record>>,
    mem_bytes: usize,
    wal: Wal,
    cfg: FoldCfg,
    write_limits: WriteLimits,
    read_limits: ReadLimits,
    retained_commit_count: usize,
    /// ONE budget for every part in this store, not one per part. Section caches are what make a
    /// whole-part walk linear, so they cannot be removed — but unbounded they pinned 9.5x each part's
    /// on-disk size, which is a per-part cost that multiplies by part count.
    pcache: Arc<SectionCache>,
    metrics: crate::observability::StoreMetrics,
    events: crate::observability::EventJournal,
    /// Fold state may already contain content for a mutation whose WAL append failed. Reusing that
    /// dedup state could let a later WAL frame omit bytes replay cannot find, so only reopen may
    /// discard the unpublished fold tail and restore a coherent acceptance boundary.
    requires_reopen: bool,
}

/// Cheap operational state for an embedder's health and metrics endpoint.
///
/// This reports engine facts, not a telemetry format or consumer policy. Counters that require a
/// full visibility walk (for example exact live-record count) are deliberately absent from this
/// cheap snapshot rather than hidden behind a surprisingly expensive getter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreHealth {
    /// Public store-authority encoding: zero is the canonical origin; a positive value is the
    /// current manifest revision.
    pub commit: u64,
    pub fold_generation: u32,
    pub parts: usize,
    pub part_rows: u64,
    pub memtable_entries: usize,
    pub memtable_bytes: usize,
    pub wal_bytes: u64,
    pub wal_frames: u64,
    pub fold_disk_bytes: u64,
    pub fold_segments: u32,
    pub fold_cache_hits: u64,
    pub fold_cache_misses: u64,
    pub fold_cache_bytes: usize,
    pub fold_cache_budget: usize,
    pub fold_block_target_bytes: usize,
    pub fold_segment_max_bytes: u32,
    pub fold_compression_level: i32,
    pub fold_compression_threads: usize,
    pub part_cache_bytes: usize,
    pub part_cache_budget: usize,
    pub max_stored_frame_bytes: u64,
    pub max_decoded_frame_bytes: u64,
    pub max_directory_entries: u64,
    pub max_wal_frames: u64,
    pub max_fold_blocks: u64,
    pub dedup_window_entries: usize,
    pub retained_commits: usize,
    pub punched_blocks: u64,
}

/// Member bytes in one reachability class.
///
/// `logical_bytes` is portable member length. `allocated_bytes` is `None` until single-file extent
/// attribution can report real filesystem allocation rather than inventing a structural zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceAmount {
    pub members: usize,
    pub logical_bytes: u64,
    pub allocated_bytes: Option<u64>,
}

/// Exact reachability-aware storage facts for preflight and operational reporting.
///
/// Categories are disjoint. `retained_only` is not garbage: bounded time-travel manifests still
/// require it. `unclassified` is deliberately not called reclaimable: it includes both unnamed
/// free extents and named members for which neither live nor retained manifest grants authority.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreSpaceUsage {
    pub live: SpaceAmount,
    pub retained_only: SpaceAmount,
    pub unclassified: SpaceAmount,
    pub total: SpaceAmount,
    /// Bytes available to the current user on the containing filesystem, or `None` when unavailable.
    pub filesystem_available_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Space facts and an explicitly advisory duplicate-generation estimate for refold.
pub struct RefoldSpaceEstimate {
    pub source_fold_logical_bytes: u64,
    pub source_part_bytes: u64,
    pub source_part_sections: usize,
    pub source_part_raw_section_bytes: u64,
    pub retained_only_bytes_before: u64,
    pub estimated_stage_bytes: u64,
    pub estimate_is_hard_bound: bool,
    pub filesystem_available_bytes: Option<u64>,
}

/// Refuse an inverted scan range before it reaches `BTreeMap::range`, which PANICS on
/// `start > end` rather than returning empty.
///
/// The corruption storm holds every on-disk parser to "errors, never panics". That discipline was
/// applied rigorously to bytes on disk and not at all to API arguments — so a store that refuses to
/// panic on a corrupt file would panic on a reversed range, which draws the trust boundary in the
/// wrong place: disk bytes are hostile, but the embedder is merely capable of being mistaken, and
/// only one of those can be fixed by refusing. Equal bounds are a legitimately empty half-open
/// range and are allowed.
fn check_range(from: Option<&str>, to: Option<&str>) -> Result<()> {
    if let (Some(f), Some(t)) = (from, to) {
        if f > t {
            bail!("scan range is inverted: from {f:?} sorts after to {t:?}");
        }
    }
    Ok(())
}

impl Store {
    /// Part count at which [`Store::auto_compact`] runs a total merge. Chosen by measurement, not
    /// taste — see that method's numbers.
    pub const AUTO_COMPACT_K: usize = 8;

    fn ensure_writer_usable(&self) -> Result<()> {
        if self.requires_reopen {
            bail!("writer requires reopen after a failed staged operation");
        }
        self.fold.ensure_no_failed_write()?;
        self.container.lock().expect("container lock poisoned").ensure_store_writer_usable()
    }

    fn discard_staged_and_require_reopen(
        &mut self,
        operation: &str,
        error: anyhow::Error,
    ) -> anyhow::Error {
        self.requires_reopen = true;
        match self.container.lock().expect("container lock poisoned").discard_staged() {
            Ok(()) => error.context(format!(
                "{operation} failed after staging began; staged authority discarded and writer requires reopen"
            )),
            Err(cleanup) => cleanup.context(format!(
                "{operation} failed ({error:#}); discarding its staged authority also failed and writer requires reopen"
            )),
        }
    }

    /// Open a writer over the single-file store — THE way a store opens for writing.
    ///
    /// `path` names a container; an absent path becomes a new, empty store. Beside it while open:
    /// `<path>-wal`, replayed here if a crash left it. Native writer exclusion uses `flock` on Unix
    /// and `LockFileEx` on Windows, on the container handle, and is OS-released on death.
    pub fn open_file(path: &Path, cfg: FoldCfg) -> Result<Store> {
        Self::open_file_with_options(path, StoreOptions { fold: cfg, ..StoreOptions::default() })
    }

    /// [`Store::open_file`] with explicit write-admission policy.
    pub fn open_file_with_limits(
        path: &Path,
        cfg: FoldCfg,
        write_limits: WriteLimits,
    ) -> Result<Store> {
        Self::open_file_with_options(
            path,
            StoreOptions { fold: cfg, write_limits, ..StoreOptions::default() },
        )
    }

    /// [`Store::open_file`] with explicit storage, cache, and admission configuration.
    pub fn open_file_with_options(path: &Path, options: StoreOptions) -> Result<Store> {
        let recovery_started = std::time::Instant::now();
        let StoreOptions {
            fold: cfg,
            write_limits,
            read_limits,
            part_cache_bytes,
            open_verification,
        } = options;
        debris::validate_store_path(path)?;
        crate::fold::validate_cfg(cfg)?;
        let write_limits = write_limits.validate()?;
        let read_limits = read_limits.validate()?;
        if part_cache_bytes < crate::part::cache::BUDGET_MIN {
            bail!("part_cache_bytes must be at least {}", crate::part::cache::BUDGET_MIN);
        }
        if path.is_dir() {
            bail!("{} is a directory; a TurnDB store is one current-format file", path.display());
        }
        let reclaim = crate::container::reclaim_names(path);
        if !path.exists() && !reclaim.anchor.exists() {
            // Transient names beside an ABSENT store — a pending publish that never landed,
            // reclaim material without its anchor — are not proven dead, so nothing is removed;
            // the ones that mean "a store was being published here" refuse creation.
            let refusing = debris::names_refusing_creation(path, read_limits)?;
            if !refusing.is_empty() {
                bail!(
                    "{} is absent but transient files sit beside it ({}); not creating a new \
                     store over them — inspect and remove them first",
                    path.display(),
                    refusing.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                );
            }
        }
        if !path.exists() && reclaim.anchor.exists() {
            // A reclaim's replace crashed in the state the ANCHOR protocol admits — the store's
            // name gone, the anchor intact. That protocol runs where a replace over an open
            // destination is not durable (`sys::replace_open_durability`), but the anchor is a
            // file beside the store and travels with it, so interrupted-reclaim completion runs on
            // every platform. Restore from the anchor, or refuse; never create.
            restore_store_from_reclaim_anchor(path, cfg, read_limits)?;
        }
        let container = if !path.exists() {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    crate::vfs::mkdir_all(parent)?;
                }
            }
            crate::container::Container::create_internal_with_limits(path, read_limits)?
        } else {
            crate::container::Container::open_internal_with_limits(path, read_limits)?
        };
        let container = container.lock_writer_current()?;

        // The manifest is a member. Missing means a new store — UNLESS retained commits exist,
        // This store has committed before
        // and MANIFEST was lost, and opening it as new buries the loss.
        let manifest = if container.contains("MANIFEST") {
            let bytes = container.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?;
            verification_integrity("open current manifest revision", Manifest::parse(&bytes))?
        } else {
            if container.committed_is_empty_birth() {
                Manifest::default()
            } else {
                return verification_integrity(
                    "open current manifest revision",
                    Err(anyhow::anyhow!(
                        "container {} has no MANIFEST authority but is not the exact empty \
                         sequence-zero birth state",
                        path.display(),
                    )),
                );
            }
        };
        // Validate every structure referenced by current authority and the complete current WAL
        // before any cleanup or writer-open mutation. An unknown/corrupt artifact is refused byte-for-
        // byte, including its adjacent evidence.
        let current = verification_integrity(
            "preflight current store authority",
            open_read_container_handle(&container, cfg, path, read_limits),
        )?;
        let wal_path = file_wal_path(path);
        let mut replay = Wal::replay_state_with_limits(&wal_path, read_limits)?;
        verification_integrity(
            "preflight WAL content identities",
            verify_replay_identities(&replay.frames, &current),
        )?;
        let verification_control = crate::control::OperationControl::default();
        // Publication can become current before the now-redundant WAL prefix is truncated. Its
        // frames remain integrity input above, but they are already represented by the current
        // manifest and must not become pending record versions or be published a second time.
        if manifest.commit != 0 {
            replay.frames.retain(|frame| frame.seq != manifest.next_seq);
        }
        let deep = open_verification == OpenVerification::Deep;
        let chain = verification_integrity(
            "preflight retained manifest chain",
            verify_chain_container_scoped(&container, read_limits, &verification_control, deep),
        )?;
        if deep {
            verification_integrity(
                "preflight complete current store authority",
                verify_committed_store(&current.parts, &current.fold, chain, &verification_control),
            )?;
            verification_integrity(
                "preflight retained-authority piece dictionaries",
                verify_retained_piece_dictionaries(
                    &container,
                    path,
                    cfg,
                    read_limits,
                    &verification_control,
                ),
            )?;
        }
        drop(current);
        let retained_commit_count = container_retained_commits(&container).len();
        let container = std::sync::Arc::new(std::sync::Mutex::new(container));

        // No residue reconciliation and no fold truncation: a retained manifest revision newer than
        // current cannot exist (one flip names everything), and the selected extent lists are the
        // complete authority. No writer-open mutation begins until current state and WAL validate.
        let fold = verification_integrity(
            "open fold referenced by current authority",
            Fold::open_container_writer(
                container.clone(),
                manifest.fold_gen,
                cfg,
                manifest.fold_tail(),
                &manifest.punched,
                read_limits,
            ),
        )?;

        let pcache = Arc::new(SectionCache::new(part_cache_bytes));
        let mut parts = Vec::with_capacity(manifest.parts.len());
        for p in &manifest.parts {
            let reader =
                container.lock().expect("container lock poisoned").extent(&p.member).ok_or_else(
                    || {
                        anyhow::anyhow!(
                            "MANIFEST names part {} but {} does not hold it",
                            p.member,
                            path.display()
                        )
                    },
                )?;
            parts.push(Arc::new(verification_integrity(
                "open committed part",
                open_manifest_part(Box::new(reader), p, pcache.clone(), read_limits),
            )?));
        }

        // Only now has every committed storage plane and WAL frame proved intelligible.
        let debris_removed = debris::remove_beside_present_store(path, read_limits)?;

        // No member sweep here: the selected container state was already proved to name only
        // members some current or retained manifest revision requires, so there is nothing to
        // free. Every publication that prunes retention frees what it stops retaining in the same
        // container state, which is what keeps that proof true.
        // A crashed merge leaves its spool scratch beside the store — pre-commit garbage, removed
        // whole. The member it was assembling is uncommitted noise needing nothing.
        let tmp_dir = file_tmp_dir(path);
        if tmp_dir.exists() {
            let _ = crate::vfs::remove_tree(&tmp_dir);
        }

        let recovered_wal_frames = u64::try_from(replay.frames.len()).unwrap_or(u64::MAX);
        let physical_wal_frames = replay.physical_frames;
        let valid_wal_bytes = replay.valid_bytes;
        let mut mem: BTreeMap<String, Option<Record>> = BTreeMap::new();
        let mut mem_bytes = 0usize;
        let mut fold = fold;
        for f in replay.frames {
            for (h, bytes) in &f.novel {
                // Resolve through both tiers before re-folding, exactly as the write path does.
                // The crash window between the flip and the WAL truncate replays frames whose
                // pieces the just-committed part already holds; re-appending them would leak the
                // bytes into the file for nothing.
                let mut known = fold.lookup(*h).is_some();
                if !known {
                    for part in parts.iter().rev() {
                        if let Some(location) = part.find_piece(h)? {
                            if !fold.is_punched(location.block_id) {
                                known = true;
                                break;
                            }
                        }
                    }
                }
                if !known {
                    let put = fold.put_hashed(bytes, *h)?;
                    debug_assert_eq!(put.hash, *h);
                }
            }
            mem_bytes += approx_bytes(&f.record);
            if f.tomb {
                mem.insert(f.record.id, None);
            } else {
                mem.insert(f.record.id.clone(), Some(f.record));
            }
        }
        let wal =
            Wal::open_recovered(&wal_path, read_limits, physical_wal_frames, valid_wal_bytes)?;

        let mut metrics = crate::observability::StoreMetrics {
            recovered_wal_frames,
            debris_removed,
            ..crate::observability::StoreMetrics::default()
        };
        let recovery_duration = recovery_started.elapsed();
        metrics.open_wal_replay.observe_success(recovery_duration);
        let mut events = crate::observability::EventJournal::default();
        let recovery_result: Result<()> = Ok(());
        events.observe(
            crate::observability::LifecycleOperation::OpenWalReplay,
            recovery_duration,
            &recovery_result,
        );
        Ok(Store {
            path: path.to_path_buf(),
            container,
            fold,
            parts,
            manifest,
            mem,
            mem_bytes,
            wal,
            cfg,
            write_limits,
            read_limits,
            retained_commit_count,
            pcache,
            metrics,
            events,
            requires_reopen: false,
        })
    }

    /// Settle and release this writer. For a single-file store this is what leaves exactly one
    /// file at rest: the memtable flushes if it holds anything, and the emptied WAL sidecar is
    /// removed. A store dropped without closing keeps its sidecar — present-at-open means crash,
    /// and the next open replays it — so close is a tidy, never a requirement.
    pub fn close(mut self) -> Result<()> {
        self.ensure_writer_usable()?;
        if !self.mem.is_empty() {
            self.flush()?;
        }
        // Close completes any delayed publication acknowledgement even when the selected
        // successor came from WAL-free maintenance such as merge or refold.
        self.container.lock().expect("container lock poisoned").acknowledge_current_state()?;
        if self.mem.is_empty() && self.wal.frame_count() != 0 {
            // A publication can succeed before WAL truncation reports a barrier failure. The
            // handle has already adopted the manifest and cleared the pending set in that case;
            // retrying close must finish settling the now-redundant WAL rather than return success
            // with a nonempty sidecar.
            self.wal.truncate()?;
        }
        if self.wal.frame_count() == 0 {
            let wal_path = file_wal_path(&self.path);
            match crate::vfs::unlink(&wal_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("remove settled write-ahead log {}", wal_path.display())
                    });
                }
            }
            if let Some(parent) = wal_path.parent() {
                // "Exactly one file" is close's promise; it is not durable if this fails.
                crate::vfs::sync_dir(parent).with_context(|| {
                    format!("sync {} after removing the write-ahead log", parent.display())
                })?;
            }
        }
        Ok(())
    }

    /// The policy governing future writes through this handle.
    pub fn write_limits(&self) -> WriteLimits {
        self.write_limits
    }

    /// Atomic persisted-frame admission governing this handle.
    pub fn read_limits(&self) -> ReadLimits {
        self.read_limits
    }

    /// The fold configuration this writer was opened with, so an embedder deriving a reader from
    /// a live writer inherits cache and block policy instead of silently reverting to defaults.
    pub fn fold_cfg(&self) -> FoldCfg {
        self.cfg
    }

    fn note_manifest_commit(&mut self) {
        self.retained_commit_count =
            self.retained_commit_count.saturating_add(1).min(MANIFEST_RETAIN);
    }

    /// A referenced part's logical member size.
    fn part_member_bytes(&self, member: &str) -> Result<u64> {
        self.container.lock().expect("container lock poisoned").member_len(member).ok_or_else(
            || anyhow::anyhow!("MANIFEST names part {member} but the container does not hold it"),
        )
    }

    /// Directory containing the store, where filesystem capacity is measured.
    fn fs_probe_path(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    /// Monotonic, process-lifetime operation metrics for this writer handle.
    pub fn metrics(&self) -> crate::observability::StoreMetrics {
        self.metrics
    }

    /// Read retained lifecycle outcomes after an independent consumer cursor.
    ///
    /// Reads are non-destructive. `gap` reports that the requested next sequence aged out of the
    /// bounded journal; `dropped_events` is cumulative for this handle.
    pub fn lifecycle_events_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> crate::observability::LifecycleEventBatch {
        self.events.read_after(after_sequence, limit)
    }

    fn observe_lifecycle<T>(
        &mut self,
        operation: crate::observability::LifecycleOperation,
        duration: std::time::Duration,
        result: &Result<T>,
    ) {
        use crate::observability::LifecycleOperation;
        match operation {
            LifecycleOperation::OpenWalReplay => {
                self.metrics.open_wal_replay.observe(duration, result)
            }
            LifecycleOperation::Sync => self.metrics.sync.observe(duration, result),
            LifecycleOperation::Flush => self.metrics.flush.observe(duration, result),
            LifecycleOperation::Merge => self.metrics.merge.observe(duration, result),
            LifecycleOperation::Backup => self.metrics.backup.observe(duration, result),
            LifecycleOperation::Verification => self.metrics.verification.observe(duration, result),
            LifecycleOperation::ContentPunch => {
                self.metrics.content_punch.observe(duration, result)
            }
            LifecycleOperation::Refold => self.metrics.refold.observe(duration, result),
            LifecycleOperation::Erase => self.metrics.erase.observe(duration, result),
        }
        self.events.observe(operation, duration, result);
    }

    /// Verify the retained manifest chain, every selected immutable-part section, and every fold frame.
    ///
    /// This covers current store authority only. A writer that wants the report to include its
    /// pending change set must synchronize and publish first. Failures are classified at this integrity boundary and
    /// recorded in [`Store::metrics`].
    pub fn verify(&mut self) -> Result<StoreVerification> {
        self.verify_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::verify`] with cooperative checkpoints between bounded verification units.
    pub fn verify_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<StoreVerification> {
        let started = std::time::Instant::now();
        let result = self.verify_inner_with_control(control);
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::Verification,
            started.elapsed(),
            &result,
        );
        if result.as_ref().err().is_some_and(|error| {
            crate::error::classify(error) == crate::error::ErrorClass::Corruption
        }) {
            self.metrics.verification_corruption_failures =
                self.metrics.verification_corruption_failures.saturating_add(1);
        }
        result
    }

    fn verify_inner_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<StoreVerification> {
        control.check("store verification")?;
        // The writer fold may contain pieces accepted into the pending change set. Verification
        // observes one selected store authority, never writer staging.
        let container =
            crate::container::Container::open_internal_with_limits(&self.path, self.read_limits)?;
        let chain = verification_integrity(
            "verify retained manifest chain",
            verify_chain_container(&container, self.read_limits, control),
        )?;
        let selected = verification_integrity(
            "open selected store authority for verification",
            open_read_container_handle(&container, self.cfg, &self.path, self.read_limits),
        )?;
        verification_integrity(
            "verify retained-authority piece dictionaries",
            verify_retained_piece_dictionaries(
                &container,
                &self.path,
                self.cfg,
                self.read_limits,
                control,
            ),
        )?;
        drop(container);
        verify_committed_store(&selected.parts, &selected.fold, chain, control)
    }

    /// Exact member-size and physical-row distribution for the current live immutable parts.
    pub fn part_distribution(&self) -> Result<crate::observability::PartDistribution> {
        self.part_distribution_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::part_distribution`] with cooperative checkpoints between part metadata reads.
    pub fn part_distribution_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<crate::observability::PartDistribution> {
        control.check("part distribution")?;
        let mut bytes = Vec::with_capacity(self.manifest.parts.len());
        let mut rows = Vec::with_capacity(self.manifest.parts.len());
        let mut total_bytes = 0u64;
        let mut total_rows = 0u64;
        for part in &self.manifest.parts {
            control.check("part distribution")?;
            let size = self.part_member_bytes(&part.member)?;
            let part_rows = u64::from(part.records);
            total_bytes = total_bytes
                .checked_add(size)
                .ok_or_else(|| anyhow::anyhow!("part distribution byte count overflow"))?;
            total_rows = total_rows
                .checked_add(part_rows)
                .ok_or_else(|| anyhow::anyhow!("part distribution row count overflow"))?;
            bytes.push(size);
            rows.push(part_rows);
        }
        bytes.sort_unstable();
        rows.sort_unstable();
        let percentile = |values: &[u64], percent: usize| -> u64 {
            let rank = values
                .len()
                .saturating_mul(percent)
                .saturating_add(99)
                .checked_div(100)
                .unwrap_or(0);
            values
                .get(rank.saturating_sub(1).min(values.len().saturating_sub(1)))
                .copied()
                .unwrap_or(0)
        };
        Ok(crate::observability::PartDistribution {
            parts: bytes.len(),
            total_bytes,
            min_bytes: bytes.first().copied().unwrap_or(0),
            p50_bytes: percentile(&bytes, 50),
            p95_bytes: percentile(&bytes, 95),
            max_bytes: bytes.last().copied().unwrap_or(0),
            total_rows,
            min_rows: rows.first().copied().unwrap_or(0),
            p50_rows: percentile(&rows, 50),
            p95_rows: percentile(&rows, 95),
            max_rows: rows.last().copied().unwrap_or(0),
        })
    }

    /// Inspect exact referenced, unreferenced, and block-reclaimable folded content for the
    /// current manifest revision.
    ///
    /// This walks visible record programs and verifies every exact owning-Part piece location.
    /// A flushed memtable is required so unpublished references cannot make dead content appear
    /// safe to reclaim.
    pub fn content_liveness(&self) -> Result<crate::observability::ContentLiveness> {
        self.content_liveness_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::content_liveness`] with cooperative record and block checkpoints.
    pub fn content_liveness_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<crate::observability::ContentLiveness> {
        control.check("content liveness")?;
        if !self.mem.is_empty() {
            bail!("content liveness requires a flushed memtable; call sync() and flush() first");
        }
        let live_pieces = live_fold_pieces_with_control(&self.parts, &self.fold, control)?;
        let live_block_ids: HashSet<u32> =
            live_pieces.keys().map(|location| location.block_id).collect();
        let mut report = crate::observability::ContentLiveness {
            live_pieces: u64::try_from(live_pieces.len())
                .map_err(|_| anyhow::anyhow!("live piece count exceeds u64"))?,
            ..crate::observability::ContentLiveness::default()
        };
        for location in live_pieces.keys() {
            report.live_logical_bytes = report
                .live_logical_bytes
                .checked_add(u64::from(location.raw))
                .ok_or_else(|| anyhow::anyhow!("live content byte count overflow"))?;
        }

        let inventory = self.fold.block_inventory_with_control(control)?;
        let punched = |block_id: u32| {
            self.manifest.punched.iter().any(|&(lo, hi)| (lo..=hi).contains(&block_id))
        };
        let mut observed_live_blocks = HashSet::new();
        for block in inventory {
            control.check("content liveness")?;
            if punched(block.block_id) {
                continue;
            }
            if live_block_ids.contains(&block.block_id) {
                report
                    .live_blocks
                    .checked_observe(block.raw_bytes, block.stored_bytes)
                    .ok_or_else(|| anyhow::anyhow!("live block space count overflow"))?;
                observed_live_blocks.insert(block.block_id);
            } else {
                report
                    .reclaimable_blocks
                    .checked_observe(block.raw_bytes, block.stored_bytes)
                    .ok_or_else(|| anyhow::anyhow!("reclaimable block space count overflow"))?;
            }
        }
        if observed_live_blocks != live_block_ids {
            let missing = live_block_ids.difference(&observed_live_blocks).next().copied().unwrap();
            bail!("live content references block {missing}, which is absent or declared punched");
        }
        report.stranded_dead_logical_bytes =
            report.live_blocks.raw_bytes.checked_sub(report.live_logical_bytes).ok_or_else(
                || anyhow::anyhow!("live piece bytes exceed their containing block bytes"),
            )?;
        report.dead_logical_bytes = report
            .reclaimable_blocks
            .raw_bytes
            .checked_add(report.stranded_dead_logical_bytes)
            .ok_or_else(|| anyhow::anyhow!("dead content byte count overflow"))?;
        Ok(report)
    }

    /// Resolve one piece of content to a location, consulting both dedup tiers before appending.
    ///
    /// ```text
    ///   Tier 0   the fold's in-memory window   — this flush's pieces, no I/O
    ///   Tier 1   every referenced part's dictionary — published content, filter then search
    ///   append   genuinely novel content
    /// ```
    ///
    /// Tier 1 is what makes dedup **unbounded** while Tier 0 stays bounded: the window is released at
    /// every flush (see [`Store::flush`]), so resident dedup memory tracks the flush interval rather
    /// than the store, and Tier 1 is what keeps that from costing any dedup at all.
    ///
    /// Parts are consulted newest-first: recently written content is the content most likely to repeat.
    ///
    /// **Why a Tier-1 hit needs no WAL bytes.** A part is only named by the manifest after its content
    /// was durable, and the committed fold tail only grows — so any location reachable through a part's
    /// dictionary is already referenced below the current fold tail. The bytes cannot be the ones
    /// a crash discards.
    fn fold_piece(&mut self, b: &[u8]) -> Result<crate::fold::Put> {
        let hash = PieceHash::of(b);
        let result = match (|| -> Result<crate::fold::Put> {
            if let Some(loc) = self.locate(&hash)? {
                // Seed the window so further references in this flush interval answer from memory.
                self.fold.note(hash, loc);
                Ok(crate::fold::Put { hash, loc, deduped: true })
            } else {
                self.fold.put_hashed(b, hash)
            }
        })() {
            Ok(result) => result,
            Err(error) => {
                // Another span or batch member may already have staged healthy Fold bytes. No WAL
                // frame owns them yet, so allowing the handle to continue could later dedup against
                // bytes that disappear on reopen. Treat every folding-phase failure as requiring
                // that reopen, even when this individual piece refused before mutation.
                self.requires_reopen = true;
                return Err(error).context(
                    "content folding failed after acceptance staging began; reopen required",
                );
            }
        };
        self.metrics.folded_content.observe(b.len(), result.deduped);
        Ok(result)
    }

    /// Fold the spans, log the record, and stage it. Durable only after [`Store::sync`].
    pub fn put(&mut self, id: &str, spans: &[Span], attrs: Vec<(String, AttrValue)>) -> Result<()> {
        self.ensure_writer_usable()?;
        let input = [ContentSpans::new(BODY_CONTENT, spans.to_vec())];
        input_record_admission_bytes(
            id,
            &input,
            &attrs,
            self.write_limits,
            self.read_limits,
            None,
        )?;
        let sequence = self.pending_sequence()?;
        self.wal.admit_additional_frames(1)?;
        let mut novel = Vec::new();
        let body = self.fold_spans(BODY_CONTENT, spans, &mut novel)?;
        let rec = Record::new(id, vec![body], attrs)?;
        self.stage_record(sequence, rec, novel)
    }

    /// Fold, log, and stage a general record with independently named content values.
    pub fn put_record(
        &mut self,
        id: &str,
        contents: &[ContentSpans<'_>],
        attrs: Vec<(String, AttrValue)>,
    ) -> Result<()> {
        self.ensure_writer_usable()?;
        // Validate and meter the whole map before `fold_spans` can append anything to the fold.
        input_record_admission_bytes(
            id,
            contents,
            &attrs,
            self.write_limits,
            self.read_limits,
            None,
        )?;
        let sequence = self.pending_sequence()?;
        self.wal.admit_additional_frames(1)?;
        let mut novel = Vec::new();
        let mut carved = Vec::with_capacity(contents.len());
        for content in contents {
            carved.push(self.fold_spans(content.name, &content.spans, &mut novel)?);
        }
        let rec = Record::new(id, carved, attrs)?;
        self.stage_record(sequence, rec, novel)
    }

    fn fold_spans(
        &mut self,
        name: &str,
        spans: &[Span<'_>],
        novel: &mut Vec<(PieceHash, Vec<u8>)>,
    ) -> Result<Content> {
        let mut ops = Vec::with_capacity(spans.len());
        let mut identity = blake3::Hasher::new();
        for s in spans {
            match s {
                Span::Lit(b) => {
                    identity.update(b);
                    ops.push(BodyOp::Lit(b.to_vec()));
                }
                Span::Piece(b) => {
                    identity.update(b);
                    if b.is_empty() {
                        ops.push(BodyOp::Lit(Vec::new()));
                        continue;
                    }
                    let put = self.fold_piece(b)?;
                    if !put.deduped {
                        // New content: the WAL must carry the bytes because writer open ignores
                        // anything the fold wrote past the tail referenced by current authority.
                        novel.push((put.hash, b.to_vec()));
                    }
                    ops.push(BodyOp::Piece { hash: put.hash, len: b.len() as u32 });
                }
            }
        }
        Ok(Content::identified(name, ops, ContentHash(identity.finalize().into())))
    }

    fn pending_sequence(&self) -> Result<u64> {
        self.manifest
            .next_seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("MANIFEST sequence space is exhausted"))
    }

    fn stage_record(
        &mut self,
        sequence: u64,
        rec: Record,
        novel: Vec<(PieceHash, Vec<u8>)>,
    ) -> Result<()> {
        if let Err(error) = self.wal.append(sequence, &rec, &novel) {
            self.requires_reopen = true;
            return Err(error)
                .context("WAL acceptance failed after fold staging; writer poisoned until reopen");
        }
        self.mem_bytes += approx_bytes(&rec);
        self.mem.insert(rec.id.clone(), Some(rec));
        Ok(())
    }

    /// [`Store::put`], with the engine's default carve deciding the spans. The convenience most
    /// ingest wants; see [`crate::carve`] for the opinion and its escape hatches.
    pub fn put_body(
        &mut self,
        id: &str,
        body: &[u8],
        attrs: Vec<(String, AttrValue)>,
    ) -> Result<()> {
        self.put_body_with(id, body, attrs, &crate::carve::Carve::default())
    }

    /// [`Store::put`], carved by an explicit strategy — the per-call escape hatch.
    pub fn put_body_with(
        &mut self,
        id: &str,
        body: &[u8],
        attrs: Vec<(String, AttrValue)>,
        carve: &crate::carve::Carve,
    ) -> Result<()> {
        self.put(id, &carve.carve(body), attrs)
    }

    /// Apply a [`Batch`]: every member, or — across a crash — none.
    ///
    /// Fold work happens first, so each member's novel bytes are known; then every member frame
    /// plus the completion marker goes to the log in one append. Replay applies the members only when
    /// the marker committed them, so a crash anywhere inside this call replays nothing of the batch.
    /// (Content the fold gathered for an unreplayed batch is beyond the committed tail and is
    /// truncated at open, exactly like content from an unsynced put.)
    ///
    /// Durability is unchanged: the batch is ACKed by [`Store::sync`], like everything else.
    /// Within the batch, later members win over earlier ones on the same id, exactly as two puts
    /// would.
    pub fn apply(&mut self, batch: Batch) -> Result<()> {
        self.ensure_writer_usable()?;
        if batch.items.is_empty() {
            return Ok(());
        }
        if batch.items.len() > self.write_limits.max_batch_records {
            return Err(WriteAdmissionError::TooManyBatchRecords {
                actual: batch.items.len(),
                allowed: self.write_limits.max_batch_records,
            }
            .into());
        }
        // Refuse and meter the complete batch before folding any member. Otherwise an invalid later item could
        // leave novel bytes and dedup-window state behind even though no atomic batch was logged.
        let mut batch_bytes =
            WAL_FRAME_OVERHEAD.saturating_add(varint_bytes(batch.items.len() as u64));
        for (index, item) in batch.items.iter().enumerate() {
            let item_bytes = match item {
                BatchItem::Put { id, contents, attrs } => owned_record_admission_bytes(
                    id,
                    contents,
                    attrs,
                    self.write_limits,
                    self.read_limits,
                    Some(index),
                )?,
                BatchItem::Delete { id } => {
                    delete_admission_bytes(id, self.write_limits, self.read_limits, Some(index))?
                }
            };
            add_size(&mut batch_bytes, item_bytes);
        }
        if batch_bytes > self.write_limits.max_batch_bytes {
            return Err(WriteAdmissionError::BatchTooLarge {
                actual: batch_bytes,
                allowed: self.write_limits.max_batch_bytes,
            }
            .into());
        }
        let sequence = self.pending_sequence()?;
        let batch_frames = u64::try_from(batch.items.len())
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("WAL batch frame count overflow"))?;
        self.wal.admit_additional_frames(batch_frames)?;
        let mut framed: Vec<crate::store::wal::FramedRecord> =
            Vec::with_capacity(batch.items.len());
        for item in &batch.items {
            match item {
                BatchItem::Put { id, contents, attrs } => {
                    let mut novel = Vec::new();
                    let mut carved = Vec::with_capacity(contents.len());
                    for content in contents {
                        let mut ops = Vec::with_capacity(content.spans.len());
                        let mut identity = blake3::Hasher::new();
                        for s in &content.spans {
                            match s {
                                OwnedSpan::Lit(b) => {
                                    identity.update(b);
                                    ops.push(BodyOp::Lit(b.clone()));
                                }
                                OwnedSpan::Piece(b) => {
                                    identity.update(b);
                                    if b.is_empty() {
                                        ops.push(BodyOp::Lit(Vec::new()));
                                        continue;
                                    }
                                    let put = self.fold_piece(b)?;
                                    if !put.deduped {
                                        novel.push((put.hash, b.clone()));
                                    }
                                    ops.push(BodyOp::Piece { hash: put.hash, len: b.len() as u32 });
                                }
                            }
                        }
                        carved.push(Content::identified(
                            &content.name,
                            ops,
                            ContentHash(identity.finalize().into()),
                        ));
                    }
                    framed.push((Record::new(id, carved, attrs.clone())?, novel, false));
                }
                BatchItem::Delete { id } => {
                    framed.push((
                        Record { id: id.clone(), contents: Vec::new(), attrs: Vec::new() },
                        Vec::new(),
                        true,
                    ));
                }
            }
        }
        if let Err(error) = self.wal.append_batch(sequence, &framed) {
            self.requires_reopen = true;
            return Err(error).context(
                "atomic WAL acceptance failed after fold staging; writer poisoned until reopen",
            );
        }
        for (rec, _, tomb) in framed {
            if tomb {
                self.mem_bytes += rec.id.len() + 32;
                self.mem.insert(rec.id, None);
            } else {
                self.mem_bytes += approx_bytes(&rec);
                self.mem.insert(rec.id.clone(), Some(rec));
            }
        }
        Ok(())
    }

    /// Delete `id`. Durable only after [`Store::sync`], exactly like a put.
    ///
    /// Recorded as a TOMBSTONE rather than by removing anything: older parts are immutable and still
    /// hold the record, so a deletion has to be a newer version that says "absent". Space is not
    /// reclaimed here — the content stays in the fold, which is append-only. Reclaiming it is a
    /// separate, deliberate operation, because the fold is shared and the same bytes may be referenced
    /// by records that are still live.
    pub fn delete(&mut self, id: &str) -> Result<()> {
        self.ensure_writer_usable()?;
        delete_admission_bytes(id, self.write_limits, self.read_limits, None)?;
        let sequence = self.pending_sequence()?;
        self.wal.admit_additional_frames(1)?;
        self.wal.append_tomb(sequence, id)?;
        self.mem_bytes += id.len() + 32;
        self.mem.insert(id.to_string(), None);
        Ok(())
    }

    /// The ACK point: everything put so far survives a crash.
    pub fn sync(&mut self) -> Result<()> {
        self.sync_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::sync`] with an interruption checkpoint before entering the durability boundary.
    ///
    /// Once the durability boundary begins, cancellation is no longer observed. That boundary may
    /// first acknowledge a previously selected container authority before WAL fsync; returning
    /// cancellation after either dependency became durable would misreport the outcome.
    pub fn sync_with_control(&mut self, control: &crate::control::OperationControl) -> Result<()> {
        let started = std::time::Instant::now();
        let result = (|| {
            self.ensure_writer_usable()?;
            control.check("store sync")?;
            // A newer WAL frame can depend on the manifest revision this handle currently sees.
            // If an earlier publication selected that revision but its final barrier failed, WAL
            // fsync alone cannot acknowledge the dependency: a crash could select the predecessor
            // and leave replay input spanning two publication sequences. Establish the selected
            // container authority before acknowledging any dependent accepted mutation.
            self.container.lock().expect("container lock poisoned").acknowledge_current_state()?;
            self.wal.sync()
        })();
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::Sync,
            started.elapsed(),
            &result,
        );
        result
    }

    /// Synchronize and publish every accepted operation, then install a verified, self-contained
    /// backup store.
    ///
    /// The destination must not exist. Holding `&mut self` prevents this process from changing the
    /// manifest while the backup walks the members it names — that part holds everywhere. Excluding a
    /// second writer *process* is the writer lock's job; native Unix and Windows enforce it. On
    /// `wasm32-wasip1` it gates nothing, so a concurrent writer is admitted and the artifact may be
    /// a racing cut. See [the store shape](https://github.com/turndb/turndb/blob/main/FORMAT.md#store-shape).
    pub fn backup(&mut self, out: &Path) -> Result<crate::backup::BackupStats> {
        self.backup_with_control(out, &crate::control::OperationControl::default())
    }

    /// [`Store::backup`] with cooperative cancellation before atomic artifact installation.
    ///
    /// Sync/flush may publish an equivalent representation of earlier accepted writes in
    /// the source store. Cancellation never installs the backup destination.
    pub fn backup_with_control(
        &mut self,
        out: &Path,
        control: &crate::control::OperationControl,
    ) -> Result<crate::backup::BackupStats> {
        let started = std::time::Instant::now();
        let result = self.backup_inner_with_control(out, control);
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::Backup,
            started.elapsed(),
            &result,
        );
        result
    }

    fn backup_inner_with_control(
        &mut self,
        out: &Path,
        control: &crate::control::OperationControl,
    ) -> Result<crate::backup::BackupStats> {
        self.ensure_writer_usable()?;
        control.check("backup")?;
        if !crate::backup::ATOMIC_RESTORE {
            return Err(crate::backup::BackupError::Unsupported(
                "this target has no atomic no-replace artifact-installation primitive".to_string(),
            )
            .into());
        }
        crate::backup::ensure_destination_available(out)?;
        let staging = artifact_staging_path(out, "backing-up");
        crate::backup::ensure_source_is_not_staging(&self.path, &staging)?;
        self.sync_with_control(control)?;
        self.flush_with_control(control)?;
        control.check("backup")?;
        backup_container_copy(
            &self.container,
            &self.manifest,
            self.cfg,
            self.read_limits,
            out,
            &staging,
            control,
        )
    }

    /// Publish the memtable as a part and commit it.
    ///
    /// Data before pointers, and the manifest last: the fold is durable before a part names any of
    /// it, and the part is durable before the manifest names the part.
    pub fn flush(&mut self) -> Result<Option<PartRef>> {
        self.flush_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::flush`] with cooperative checks before manifest publication.
    ///
    /// Fold sync may make accepted content bytes durable before a later cancellation, but the live
    /// manifest revision and pending change set remain unchanged. An unpublished part is removed. Once publication
    /// begins, cancellation is no longer observed and the ordinary crash protocol owns the outcome.
    pub fn flush_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<Option<PartRef>> {
        let started = std::time::Instant::now();
        let result = self.flush_inner_with_control(control);
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::Flush,
            started.elapsed(),
            &result,
        );
        result
    }

    fn flush_inner_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<Option<PartRef>> {
        self.ensure_writer_usable()?;
        control.check("memtable flush")?;
        if self.mem.is_empty() {
            // Publication may have selected the successor before its final barrier reported an
            // error, leaving only redundant replay input. A retry of the publication primitive is
            // also the settlement retry; otherwise backup and actor preludes could claim a
            // settled source while preserving a WAL that replays already-published mutations.
            if self.wal.frame_count() != 0 {
                self.container
                    .lock()
                    .expect("container lock poisoned")
                    .acknowledge_current_state()?;
                self.wal.truncate()?;
            }
            return Ok(None);
        }
        let prepared = (|| -> Result<(Part, Manifest)> {
            let tail = self.fold.sync()?;
            let seq = self
                .manifest
                .next_seq
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("MANIFEST sequence space is exhausted"))?;
            let file = format!("part-{seq:08}.part");
            let mut recs: Vec<Record> = Vec::with_capacity(self.mem.len());
            let mut tombs: Vec<bool> = Vec::with_capacity(self.mem.len());
            for (id, v) in &self.mem {
                control.check("memtable flush planning")?;
                match v {
                    Some(r) => {
                        recs.push(r.clone());
                        tombs.push(false);
                    }
                    // A tombstone still needs a row, so it gets an empty one carrying only its id.
                    None => {
                        recs.push(Record {
                            id: id.clone(),
                            contents: Vec::new(),
                            attrs: Vec::new(),
                        });
                        tombs.push(true);
                    }
                }
            }

            // Resolve every referenced piece through both dedup tiers before building the part.
            let mut locs: HashMap<PieceHash, Loc> = HashMap::new();
            for record in &recs {
                control.check("memtable flush planning")?;
                for content in &record.contents {
                    for op in &content.ops {
                        control.check("memtable flush planning")?;
                        let BodyOp::Piece { hash, .. } = op else { continue };
                        if locs.contains_key(hash) {
                            continue;
                        }
                        let loc = self.locate(hash)?.ok_or_else(|| {
                            anyhow::anyhow!(
                                "pending piece {hash} is in neither the fold window nor any referenced part"
                            )
                        })?;
                        locs.insert(*hash, loc);
                    }
                }
            }
            let member =
                self.container.lock().expect("container lock poisoned").begin_member(&file)?;
            let (meta, member) = part::build_full_into(
                member,
                &recs,
                &tombs,
                seq,
                seq,
                self.cfg.level,
                |hash| locs.get(hash).copied(),
                &HashMap::new(),
                self.read_limits,
            )?;
            control.check("memtable flush publication")?;
            let digest = {
                let mut container = self.container.lock().expect("container lock poisoned");
                PieceHash(container.finish_member(member)?).to_hex()
            };
            let reader = self
                .container
                .lock()
                .expect("container lock poisoned")
                .extent(&file)
                .expect("the member was staged a moment ago");
            let opened = Part::open_reader_with_limits(
                Box::new(reader),
                self.pcache.clone(),
                self.read_limits,
            )?;

            let mut manifest = self.manifest.clone();
            manifest.parts.push(PartRef {
                member: file,
                seq_lo: seq,
                seq_hi: seq,
                records: meta.n_records,
                b3: digest,
            });
            manifest.fold_seg = tail.seg;
            manifest.fold_off = tail.off;
            manifest.next_seq = seq;
            let mut container = self.container.lock().expect("container lock poisoned");
            manifest.commit_into_container(&mut container)?;
            sweep_unreachable_container(&mut container, &manifest, self.read_limits)?;
            Ok((opened, manifest))
        })();
        let (opened, m) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(self.discard_staged_and_require_reopen("flush preparation", error));
            }
        };
        let mut c = self.container.lock().expect("container lock poisoned");
        let publication_error = match c.commit() {
            Ok(_) => None,
            Err(error) if c.failed_publication_selected() == Some(true) => Some(error),
            Err(error) => return Err(error),
        }; // <- the linearization point: one flip names everything above
        drop(c);

        self.parts.push(Arc::new(opened));
        self.manifest = m;
        self.note_manifest_commit();
        self.mem.clear();
        self.mem_bytes = 0;
        // Release Tier 0 only here, after the part is committed and open. Closing it any earlier
        // would drop the window while the part being built still needs it, and the part cannot answer
        // a Tier-1 lookup until it is committed and in `self.parts`. Everything the window covered is
        // now reachable through that part's dictionary, so nothing is lost but the memory.
        self.fold.release_dedup_window();
        if let Some(error) = publication_error {
            // The successor is selected but its durability barrier was not acknowledged. Keep the
            // redundant WAL for retry/reopen, but never leave this live handle behind its own
            // selected manifest revision.
            return Err(error).context(
                "container selected the flushed manifest revision, but its final synchronization failed",
            );
        }
        // Only now: the records are in a committed part, so the log that carried them is redundant.
        self.wal.truncate()?;
        Ok(self.manifest.parts.last().cloned())
    }

    /// Merge a contiguous run of parts referenced by the current manifest revision into one, then
    /// publish the resulting manifest revision atomically.
    ///
    /// Contiguity is the correctness gate: parts resolve versions by sequence, so merging a
    /// non-adjacent set would drop whatever an excluded part said about a shared id. The range is
    /// therefore expressed as a slice of that ordered part list, which cannot express a gap.
    pub fn merge_range(
        &mut self,
        lo: usize,
        len: usize,
    ) -> Result<Option<crate::part::merge::MergeStats>> {
        self.merge_range_with_control(lo, len, &crate::control::OperationControl::default())
    }

    /// [`Store::merge_range`] with cooperative checkpoints. The final cancellable checkpoint is
    /// immediately before manifest publication; publication and the in-memory replacement are
    /// then uninterruptible.
    pub fn merge_range_with_control(
        &mut self,
        lo: usize,
        len: usize,
        control: &crate::control::OperationControl,
    ) -> Result<Option<crate::part::merge::MergeStats>> {
        let started = std::time::Instant::now();
        let result = self.merge_range_inner_with_control(lo, len, control);
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::Merge,
            started.elapsed(),
            &result,
        );
        result
    }

    fn merge_range_inner_with_control(
        &mut self,
        lo: usize,
        len: usize,
        control: &crate::control::OperationControl,
    ) -> Result<Option<crate::part::merge::MergeStats>> {
        self.ensure_writer_usable()?;
        control.check("part compaction")?;
        let Some(end) = lo.checked_add(len) else { return Ok(None) };
        if len < 2 || end > self.parts.len() {
            return Ok(None);
        }
        let inputs: Vec<Arc<Part>> = self.parts[lo..end].to_vec();
        // Named by the sequence RANGE it spans. The output's range strictly contains every input's
        // (the inputs are disjoint and there are at least two), so the name cannot collide with a part
        // this merge is about to replace — which the post-commit sweep would otherwise unlink.
        let seq_lo = self.manifest.parts[lo].seq_lo;
        let seq_hi = self.manifest.parts[end - 1].seq_hi;
        let file = format!("part-{seq_lo:08}-{seq_hi:08}.part");
        debug_assert!(
            !self.manifest.parts.iter().any(|p| p.member == file),
            "merge output {file} collides with a referenced part"
        );
        // A tombstone may only be discarded when this merge covers the ENTIRE live list — otherwise a
        // part outside the run could still hold an older version of the deleted id, and dropping the
        // tombstone would resurrect it.
        let total = lo == 0 && len == self.parts.len();
        // Publish: the merged part is durable before the manifest names it; the pre-flip barrier
        // makes the manifest swap the single
        // linearization point. A crash before it leaves the merged output unreachable: an orphan
        // file, or uncommitted noise past the tail. The INPUTS are not deleted here: retained
        // manifests still name them, so a reader inside the retention window keeps a complete
        // snapshot. They fall to the sweep when the window prunes past their last naming manifest.
        // Every fallible preparation step and the final cancellation checkpoint happen before
        // commit is attempted. Once commit starts, its ordinary crash protocol—not cancellation—
        // decides the outcome.
        let tmp = file_tmp_dir(&self.path);
        crate::vfs::mkdir_all(&tmp)?;
        let member = self.container.lock().expect("container lock poisoned").begin_member(&file)?;
        let location_is_usable = |location: Loc, hash: PieceHash| -> Result<bool> {
            self.fold.verify_location_shape(location).with_context(|| {
                format!("merge input dictionary mapped {hash} to an invalid fold location")
            })?;
            if self.fold.is_punched(location.block_id) {
                return Ok(false);
            }
            self.fold.read_verified(location, hash).with_context(|| {
                format!("merge input dictionary mapped {hash} to invalid fold bytes")
            })?;
            Ok(true)
        };
        let built = crate::part::merge::merge_into_with_control_for_operation(
            member,
            &tmp.join("m"),
            &inputs,
            self.cfg.level,
            total,
            control,
            "part compaction",
            self.read_limits,
            Some(&location_is_usable),
        );
        let (meta, stats, member) = match built {
            Ok(v) => v,
            Err(error) => {
                self.container.lock().expect("container lock poisoned").abandon_open_member();
                let _ = crate::vfs::remove_tree(&tmp);
                return Err(error);
            }
        };
        if let Err(error) = control.check("part compaction") {
            self.container.lock().expect("container lock poisoned").abandon_open_member();
            let _ = crate::vfs::remove_tree(&tmp);
            return Err(error.into());
        }
        let digest = {
            let mut c = self.container.lock().expect("container lock poisoned");
            PieceHash(c.finish_member(member)?).to_hex()
        };
        let _ = crate::vfs::remove_tree(&tmp);
        let prepared = (|| -> Result<(Part, Manifest)> {
            let reader = self
                .container
                .lock()
                .expect("container lock poisoned")
                .extent(&file)
                .expect("the member was staged a moment ago");
            let opened = Part::open_reader_with_limits(
                Box::new(reader),
                self.pcache.clone(),
                self.read_limits,
            )?;
            let mut m = self.manifest.clone();
            m.parts.splice(
                lo..end,
                [PartRef {
                    member: file.clone(),
                    seq_lo: meta.seq_lo,
                    seq_hi: meta.seq_hi,
                    records: meta.n_records,
                    b3: digest,
                }],
            );
            let t = self.fold.sync()?;
            m.fold_seg = t.seg;
            m.fold_off = t.off;
            let mut c = self.container.lock().expect("container lock poisoned");
            m.commit_into_container(&mut c)?;
            sweep_unreachable_container(&mut c, &m, self.read_limits)?;
            Ok((opened, m))
        })();
        let (opened, m) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                // The merged member has already been registered in the staged directory. No
                // unrelated later operation may publish it (or a partly staged manifest). Fold
                // sync may also have advanced in-memory locations, so successful cleanup still
                // requires reopen before this writer can be trusted again.
                return Err(self.discard_staged_and_require_reopen("merge preparation", error));
            }
        };
        let mut c = self.container.lock().expect("container lock poisoned");
        let publication_error = match c.commit() {
            Ok(_) => None,
            Err(error) if c.failed_publication_selected() == Some(true) => Some(error),
            Err(error) => return Err(error),
        }; // <- the linearization point
        drop(c);
        self.manifest = m;
        self.note_manifest_commit();
        self.parts.splice(lo..end, [Arc::new(opened)]);
        if let Some(error) = publication_error {
            return Err(error).context(
                "container selected the merged manifest revision, but its final synchronization failed",
            );
        }
        Ok(Some(stats))
    }

    /// Size-tiered compaction: when parts pile up, fold the oldest run together.
    ///
    /// Merging the OLDEST parts keeps the run contiguous by construction and matches the access
    /// pattern — old parts are cold and stop being rewritten. Bounding part count is not only about
    /// read amplification: a Tier-1 dedup lookup is O(parts), so this is what keeps global dedup
    /// affordable.
    ///
    /// This is the MANUAL dial; [`Store::auto_compact`] is the engine's measured default policy.
    pub fn maybe_compact(
        &mut self,
        trigger: usize,
        run: usize,
    ) -> Result<Option<crate::part::merge::MergeStats>> {
        self.maybe_compact_with_control(trigger, run, &crate::control::OperationControl::default())
    }

    /// [`Store::maybe_compact`] with cooperative checkpoints. The final cancellable checkpoint is
    /// immediately before manifest publication in [`Store::merge_range_with_control`].
    pub fn maybe_compact_with_control(
        &mut self,
        trigger: usize,
        run: usize,
        control: &crate::control::OperationControl,
    ) -> Result<Option<crate::part::merge::MergeStats>> {
        control.check("part compaction")?;
        if self.parts.len() < trigger {
            return Ok(None);
        }
        self.merge_range_with_control(0, run.min(self.parts.len()), control)
    }

    /// The engine's compaction opinion: a TOTAL merge whenever the live list reaches
    /// [`Store::AUTO_COMPACT_K`] parts. Call it after flushes; it is cheap to call and refuses
    /// below the threshold.
    ///
    /// The classic LSM tradeoff — write amplification against read amplification — collapses
    /// here, because a merge rewrites references and columns and never content. Measured on 20k
    /// real records (examples/compact_bench): the whole policy space lands within 0.008–0.011 ms
    /// per point lookup, so read amp does not discriminate; merge WALL is the only real cost, and
    /// total-at-8 paid 0.7s across the run where tiered(8,4) paid 1.7s for MORE final parts and
    /// no tombstone removal. Total merges are also the only ones allowed to drop tombstones,
    /// so deleted record IDs stop shadowing older versions instead of persisting forever.
    ///
    /// The honest caveat, documented as the dial it is: a total merge costs O(live records) of
    /// wall time, so at some store size a young/old split becomes worth it. That crossover is far
    /// beyond current scale; when it arrives, `maybe_compact` is the young tier's tool and this
    /// policy becomes the old tier's slow beat.
    pub fn auto_compact(&mut self) -> Result<Option<crate::part::merge::MergeStats>> {
        self.auto_compact_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::auto_compact`] with cooperative checkpoints. Its final cancellable checkpoint is
    /// the manifest-publication boundary documented by [`Store::merge_range_with_control`].
    pub fn auto_compact_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<Option<crate::part::merge::MergeStats>> {
        self.ensure_writer_usable()?;
        control.check("part compaction")?;
        if self.parts.len() < Self::AUTO_COMPACT_K {
            return Ok(None);
        }
        self.merge_range_with_control(0, self.parts.len(), control)
    }

    /// Select a contiguous compaction run whose exact physical input fits every supplied budget.
    ///
    /// The widest eligible run wins; ties prefer the oldest run. Contiguity preserves sequence
    /// visibility, and only a plan covering the complete live list may drop tombstones.
    pub fn plan_compaction(&self, budget: CompactionBudget) -> Result<Option<CompactionPlan>> {
        budget.validate()?;
        if self.manifest.parts.len() < 2 {
            return Ok(None);
        }
        let costs: Vec<(u64, u64)> = self
            .manifest
            .parts
            .iter()
            .map(|part| {
                let bytes = self.part_member_bytes(&part.member)?;
                Ok((u64::from(part.records), bytes))
            })
            .collect::<Result<_>>()?;

        let mut best: Option<CompactionPlan> = None;
        for start in 0..costs.len() - 1 {
            let mut rows = 0u64;
            let mut bytes = 0u64;
            for (offset, &(part_rows, part_bytes)) in
                costs[start..].iter().take(budget.max_input_parts).enumerate()
            {
                rows = rows.saturating_add(part_rows);
                bytes = bytes.saturating_add(part_bytes);
                if rows > budget.max_input_rows || bytes > budget.max_input_bytes {
                    break;
                }
                let parts = offset + 1;
                if parts < 2 {
                    continue;
                }
                let plan = CompactionPlan {
                    start_part: start,
                    input_parts: parts,
                    input_rows: rows,
                    input_bytes: bytes,
                    drops_tombstones: start == 0 && parts == costs.len(),
                };
                if best.as_ref().is_none_or(|current| plan.input_parts > current.input_parts) {
                    best = Some(plan);
                }
            }
        }
        if best.is_some() {
            return Ok(best);
        }

        let (start_part, input_rows, input_bytes) = costs
            .windows(2)
            .enumerate()
            .map(|(start, pair)| {
                (start, pair[0].0.saturating_add(pair[1].0), pair[0].1.saturating_add(pair[1].1))
            })
            .min_by_key(|&(start, rows, bytes)| (bytes, rows, start))
            .expect("at least two parts were checked above");
        Err(CompactionError::BudgetTooSmall { start_part, input_rows, input_bytes, budget }.into())
    }

    /// Estimate temporary output space for the current bounded-part-merge plan.
    ///
    /// Input member lengths, section counts/raw bytes, retained-input bytes, and filesystem
    /// availability are exact at this cut. `estimated_stage_bytes` is intentionally not a hard
    /// bound: recompression and merged index encoding can change output size. Callers may apply
    /// their own safety factor; TurnDB exposes the basis instead of hiding policy in a boolean.
    pub fn estimate_compaction_space(
        &self,
        budget: CompactionBudget,
    ) -> Result<Option<CompactionSpaceEstimate>> {
        self.estimate_compaction_space_with_control(
            budget,
            &crate::control::OperationControl::default(),
        )
    }

    /// [`Store::estimate_compaction_space`] with cooperative planning checkpoints.
    pub fn estimate_compaction_space_with_control(
        &self,
        budget: CompactionBudget,
        control: &crate::control::OperationControl,
    ) -> Result<Option<CompactionSpaceEstimate>> {
        control.check("compaction space preflight")?;
        let Some(plan) = self.plan_compaction(budget)? else {
            return Ok(None);
        };
        let end = plan.start_part + plan.input_parts;
        let mut input_sections = 0usize;
        let mut input_raw_section_bytes = 0u64;
        for part in &self.parts[plan.start_part..end] {
            control.check("compaction space preflight")?;
            for (_, _, raw, _) in part.sections() {
                control.check("compaction space preflight")?;
                input_sections = input_sections
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("compaction section count overflow"))?;
                input_raw_section_bytes = input_raw_section_bytes
                    .checked_add(u64::from(raw))
                    .ok_or_else(|| anyhow::anyhow!("compaction raw section byte count overflow"))?;
            }
        }
        // Raw section bytes are the strongest cheap basis before running the merge. Explicit row
        // and section framing allowance plus one MiB covers ordinary metadata variation, but is not
        // represented as an admission guarantee.
        let row_allowance = plan
            .input_rows
            .checked_mul(64)
            .ok_or_else(|| anyhow::anyhow!("compaction row framing estimate overflow"))?;
        let section_allowance = u64::try_from(input_sections)
            .map_err(|_| anyhow::anyhow!("compaction section count exceeds u64"))?
            .checked_mul(256)
            .ok_or_else(|| anyhow::anyhow!("compaction section framing estimate overflow"))?;
        let estimated_stage_bytes = input_raw_section_bytes
            .checked_add(row_allowance)
            .and_then(|bytes| bytes.checked_add(section_allowance))
            .and_then(|bytes| bytes.checked_add(1 << 20))
            .ok_or_else(|| anyhow::anyhow!("compaction stage estimate overflow"))?;
        Ok(Some(CompactionSpaceEstimate {
            plan,
            input_sections,
            input_raw_section_bytes,
            estimated_stage_bytes,
            estimate_is_hard_bound: false,
            retained_input_bytes_after_commit: plan.input_bytes,
            filesystem_available_bytes: crate::sys::filesystem_available_bytes(
                self.fs_probe_path(),
            )
            .with_context(|| {
                format!("measure available filesystem bytes at {}", self.fs_probe_path().display())
            })?,
        }))
    }

    pub fn compact_bounded(
        &mut self,
        budget: CompactionBudget,
    ) -> Result<Option<BoundedCompaction>> {
        self.compact_bounded_with_control(budget, &crate::control::OperationControl::default())
    }

    /// Plan and publish one budget-bounded contiguous compaction run.
    pub fn compact_bounded_with_control(
        &mut self,
        budget: CompactionBudget,
        control: &crate::control::OperationControl,
    ) -> Result<Option<BoundedCompaction>> {
        self.ensure_writer_usable()?;
        control.check("bounded compaction")?;
        let Some(plan) = self.plan_compaction(budget)? else {
            return Ok(None);
        };
        let merge = self
            .merge_range_with_control(plan.start_part, plan.input_parts, control)?
            .expect("a compaction plan always contains at least two parts");
        let output = &self.manifest.parts[plan.start_part];
        let output_bytes = self.part_member_bytes(&output.member)?;
        Ok(Some(BoundedCompaction { plan, output_bytes, merge }))
    }

    /// Newest-wins across the committed parts, then the memtable, which is newer than all of them.
    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        if let Some(v) = self.mem.get(id) {
            return Ok(v.clone());
        }
        verification_integrity("read committed record", read::get(&self.parts, id))
    }

    /// Batch projection used by structured scans. Immutable rows share part-level decoders while
    /// memtable records retain their in-memory projection path.
    pub(crate) fn project_candidates(
        &self,
        candidates: &[crate::scan::ScanCandidate],
        attrs: &HashSet<&str>,
        contents: &HashSet<&str>,
    ) -> Result<Vec<Record>> {
        let mut committed = Vec::new();
        let mut committed_outputs = Vec::new();
        let mut out: Vec<Option<Record>> = vec![None; candidates.len()];
        for (output, candidate) in candidates.iter().enumerate() {
            match candidate {
                crate::scan::ScanCandidate::Committed(row) => {
                    committed.push(row);
                    committed_outputs.push(output);
                }
                crate::scan::ScanCandidate::Memtable(id) => {
                    let record = self.mem.get(id).and_then(Option::as_ref).ok_or_else(|| {
                        anyhow::anyhow!("resolved memtable row {id:?} is no longer live")
                    })?;
                    out[output] = Some(Record {
                        id: record.id.clone(),
                        contents: record
                            .contents
                            .iter()
                            .filter(|content| contents.contains(content.name.as_str()))
                            .cloned()
                            .collect(),
                        attrs: record
                            .attrs
                            .iter()
                            .filter(|(name, _)| attrs.contains(name.as_str()))
                            .cloned()
                            .collect(),
                    });
                }
            }
        }
        for (output, record) in committed_outputs.into_iter().zip(read::project_rows(
            &self.parts,
            &committed,
            attrs,
            contents,
        )?) {
            out[output] = Some(record);
        }
        out.into_iter()
            .map(|record| record.ok_or_else(|| anyhow::anyhow!("projected row was not produced")))
            .collect()
    }

    /// Reconstruct selected content without re-locating an already resolved scan row.
    pub(crate) fn reconstruct_candidate_content(
        &self,
        candidate: &crate::scan::ScanCandidate,
        content: &Content,
    ) -> Result<Vec<u8>> {
        match candidate {
            crate::scan::ScanCandidate::Committed(row) => {
                read::reconstruct_projected_content(&self.parts, &self.fold, row, content)
            }
            crate::scan::ScanCandidate::Memtable(id) => {
                self.mem.get(id).and_then(Option::as_ref).ok_or_else(|| {
                    anyhow::anyhow!("resolved memtable row {id:?} is no longer live")
                })?;
                self.rebuild_projected_content(content)
            }
        }
    }

    /// Byte-exact content for `id`.
    pub fn reconstruct(&self, id: &str) -> Result<Option<Vec<u8>>> {
        self.reconstruct_content(id, BODY_CONTENT)
    }

    /// Byte-exact named content for `id`, without reconstructing any sibling content value.
    pub fn reconstruct_content(&self, id: &str, name: &str) -> Result<Option<Vec<u8>>> {
        // The memtable is newer than every part, so it is consulted first — and it is the ONLY thing
        // this adds over the current manifest revision.
        if let Some(v) = self.mem.get(id) {
            return match v {
                Some(r) => self.rebuild_content(r, name),
                None => Ok(None), // staged deletion
            };
        }
        verification_integrity(
            "reconstruct committed content",
            read::reconstruct_content(&self.parts, &self.fold, id, name),
        )
    }

    /// Where content lives, through BOTH dedup tiers.
    ///
    /// The single answer to "where is this piece" for every caller that needs one — the write path,
    /// the flush path, and the staged-record read path. They disagreed before, and each disagreement
    /// was the same bug wearing a different hat: a piece deduped against a committed part is not in
    /// the in-memory window, and after a crash nothing puts it back there, because the WAL carries no
    /// bytes for content that was already durable.
    fn locate(&self, h: &PieceHash) -> Result<Option<Loc>> {
        locate_verified_piece(&self.parts, &self.fold, h)
    }

    fn rebuild_content(&self, r: &Record, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(content) = r.content(name) else {
            return Ok(None);
        };
        Ok(Some(self.rebuild_projected_content(content)?))
    }

    fn rebuild_projected_content(&self, content: &Content) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for op in &content.ops {
            match op {
                BodyOp::Lit(b) => out.extend_from_slice(b),
                BodyOp::Piece { hash, .. } => {
                    let loc = self
                        .locate(hash)?
                        .ok_or_else(|| anyhow::anyhow!("piece {hash} not resolvable"))?;
                    self.fold.read_verified_into(loc, *hash, &mut out)?;
                }
            }
        }
        if let Some(expected) = content.identity {
            let got = ContentHash::of(&out);
            if got != expected {
                bail!(
                    "content {:?} reconstructed as {got} but its identity is {expected}",
                    content.name
                );
            }
        }
        Ok(out)
    }

    pub fn memtable_len(&self) -> usize {
        self.mem.len()
    }
    pub fn memtable_bytes(&self) -> usize {
        self.mem_bytes
    }
    /// Hand the fold and parts to a lens. Consumes the store because the query layer takes ownership
    /// of both; the store authority it was opened at is already baked into `parts`.
    pub fn into_parts(self) -> (Arc<Fold>, Vec<Arc<Part>>) {
        (Arc::new(self.fold), self.parts)
    }

    pub fn part_count(&self) -> usize {
        self.parts.len()
    }

    /// Parts referenced by the current manifest revision, oldest to newest — the writer-side twin
    /// of [`ReadStore::parts`].
    pub fn parts(&self) -> &[Arc<Part>] {
        &self.parts
    }

    /// [`ReadStore::scan_ids`], plus the uncommitted memtable — so a writer paging its own store
    /// sees records it has not flushed yet, which is what makes a live backfill possible.
    ///
    /// The memtable is a `BTreeMap`, so its slice of the range overlays resolved committed rows in
    /// id order. Staged deletions remove an id here exactly as a tombstone would.
    pub fn scan_ids(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<String>> {
        Ok(self
            .scan_candidates(from, to, limit, reverse, usize::MAX, true)?
            .candidates
            .into_iter()
            .map(crate::scan::ScanCandidate::into_id)
            .collect())
    }

    /// Resolve a bounded id range once and retain each live row's storage origin for projection.
    pub(crate) fn scan_candidates(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        reverse: bool,
        max_resolution_entries: usize,
        allow_oversized_group: bool,
    ) -> Result<crate::scan::CandidateBatch> {
        check_range(from, to)?;
        if limit == 0 {
            return Ok(crate::scan::CandidateBatch::default());
        }
        let range = (
            from.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Included),
            to.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
        );
        let overlay: Box<dyn Iterator<Item = (&str, bool)> + '_> = if reverse {
            Box::new(
                self.mem
                    .range::<str, _>(range)
                    .rev()
                    .map(|(id, value)| (id.as_str(), value.is_some())),
            )
        } else {
            Box::new(
                self.mem.range::<str, _>(range).map(|(id, value)| (id.as_str(), value.is_some())),
            )
        };
        let resolved = read::scan_rows(
            &self.parts,
            overlay,
            read::RowScan {
                from,
                to,
                limit,
                reverse,
                max_resolution_entries,
                allow_oversized_group,
            },
        )?;
        Ok(crate::scan::CandidateBatch {
            candidates: resolved
                .rows
                .into_iter()
                .map(|row| match row.origin {
                    read::RowOrigin::Part { .. } => crate::scan::ScanCandidate::Committed(row),
                    read::RowOrigin::Memtable => crate::scan::ScanCandidate::Memtable(row.id),
                })
                .collect(),
            resolution: crate::scan::ScanResolutionStats {
                physical_rows: resolved.physical_rows,
                superseded_rows: resolved.superseded_rows,
                tombstones: resolved.tombstones,
                memtable_entries: resolved.memtable_entries,
                budget_exhausted: resolved.budget_exhausted,
            },
            resolved_through: resolved.resolved_through,
            has_more: resolved.has_more,
        })
    }

    /// Every live id: committed parts plus the uncommitted memtable.
    ///
    /// Includes staged records, unlike [`ReadStore::ids`], because a writer can see its own writes.
    pub fn ids(&self) -> Result<Vec<String>> {
        let mut all = read::ids(&self.parts)?;
        // Overlay the memtable: a staged put adds an id, a staged delete removes one.
        all.retain(|id| !matches!(self.mem.get(id), Some(None)));
        for (id, v) in &self.mem {
            if v.is_some() && !all.contains(id) {
                all.push(id.clone());
            }
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// Bounded structured paging over the writer's complete read-your-writes view.
    pub fn scan(&self, request: &crate::scan::ScanRequest) -> Result<crate::scan::ScanPage> {
        crate::scan::scan_store(self, request)
    }

    pub(crate) fn candidate_may_match(
        &self,
        candidate: &crate::scan::ScanCandidate,
        predicates: &[crate::scan::Predicate],
    ) -> Result<bool> {
        match candidate {
            crate::scan::ScanCandidate::Memtable(_) => Ok(true),
            crate::scan::ScanCandidate::Committed(row) => match row.origin {
                read::RowOrigin::Part { part, .. } => read::part_may_match(
                    self.parts
                        .get(part)
                        .ok_or_else(|| anyhow::anyhow!("candidate part is outside the store"))?,
                    predicates,
                ),
                read::RowOrigin::Memtable => Ok(true),
            },
        }
    }

    /// Explain a structured scan against the current read-your-writes view without resolving rows
    /// or evaluating predicates.
    pub fn explain_scan(
        &self,
        request: &crate::scan::ScanRequest,
    ) -> Result<crate::scan::ScanExplanation> {
        crate::scan::explain_store(self, request)
    }

    pub(crate) fn scan_physical_scope(
        &self,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<crate::scan::ScanPhysicalScope> {
        let mut scope = read::scan_physical_scope(&self.parts, from, to)?;
        let range = (
            from.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Included),
            to.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
        );
        scope.memtable_entries_in_bounds = self.mem.range::<str, _>(range).count();
        Ok(scope)
    }

    /// ERASE records: publish tombstones and rewrite until this store no longer references the
    /// records and its old members have been removed.
    ///
    /// This is the compliance path, and it composes three operations that each already existed:
    /// deletes shadow the ids; a TOTAL merge drops the tombstones once nothing remains for them
    /// to shadow; and the re-fold rewrites the fold without the dropped content and rebuilds
    /// every part — so both the bytes AND the columnar metadata (ids, piece lengths, attribute
    /// values) of the erased records are gone when this returns. The re-fold also purges the
    /// retained manifest history, which the erasure story requires: a read view that could still serve
    /// the erased record is not erasure.
    ///
    /// What this does NOT promise, stated because overclaiming here is a liability: media-byte
    /// non-recoverability on arbitrary or copy-on-write filesystems, through WASI, or for copies
    /// already made (replicas and backups). The measurable claims are query absence, logical
    /// file-length reclamation, and the lifecycle event for this operation.
    ///
    /// Ids that do not exist are counted, not errored: a DSAR naming already-gone data is a
    /// normal outcome, and the record should say so rather than fail. When every requested id is
    /// already absent, the operation returns statistics without a transition.
    pub fn erase_ids(&mut self, ids: &[String]) -> Result<ErasureStats> {
        self.erase_ids_with_control(ids, &crate::control::OperationControl::default())
    }

    /// [`Store::erase_ids`] with cancellation during its read-only planning phase.
    ///
    /// Once the atomic tombstone batch is applied, interruption is deliberately deferred until the
    /// total merge and re-fold finish. Returning `cancelled` after logical deletion but before
    /// reclamation would make a retry mistake the ids for previously absent records and falsely
    /// report completion. Erasure therefore either stops before mutation or drives its full store
    /// protocol to completion.
    pub fn erase_ids_with_control(
        &mut self,
        ids: &[String],
        control: &crate::control::OperationControl,
    ) -> Result<ErasureStats> {
        let started = std::time::Instant::now();
        let result = self.erase_ids_inner_with_control(ids, control);
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::Erase,
            started.elapsed(),
            &result,
        );
        result
    }

    fn erase_ids_inner_with_control(
        &mut self,
        ids: &[String],
        control: &crate::control::OperationControl,
    ) -> Result<ErasureStats> {
        self.ensure_writer_usable()?;
        let mut tombstoned = 0usize;
        let mut absent = 0usize;
        let mut delete = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            control.check("record erasure")?;
            if seen.insert(id) && self.get(id)?.is_some() {
                delete.push(id);
                tombstoned += 1;
            } else {
                absent += 1;
            }
        }
        if tombstoned == 0 {
            return Ok(ErasureStats {
                requested: ids.len(),
                tombstoned,
                absent,
                remaining: self.ids()?.len(),
                refold: None,
            });
        }
        control.check("record erasure")?;
        let mut batch = Batch::new();
        for id in delete {
            batch.delete(id);
        }
        self.apply(batch)?;
        self.sync()?;
        self.flush()?;
        if self.parts.len() > 1 {
            // TOTAL, so the tombstones can drop — a partial merge would carry them forward.
            self.merge_range(0, self.parts.len())?;
        }
        let refold = self.refold()?;
        Ok(ErasureStats {
            requested: ids.len(),
            tombstoned,
            absent,
            remaining: self.ids()?.len(),
            refold: Some(refold),
        })
    }

    /// Resolve every live program through its owning Part dictionary. One identity may have more
    /// than one physical location, and every owning location must remain live until the Parts that
    /// name it stop deciding records.
    fn live_fold_pieces_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<BTreeMap<Loc, PieceHash>> {
        live_fold_pieces_with_control(&self.parts, &self.fold, control)
    }

    /// Reclaim erased space in place: content-punch every fold block no record resolved by current
    /// authority can reach.
    ///
    /// The cheap half of erasure when a full rewrite is not requested. A re-fold reclaims the same
    /// bytes by rewriting the world — correct, thorough, and O(store); this walks the live
    /// records' piece references, finds blocks nothing reaches, records them in the manifest, and
    /// deallocates their extents. Offsets do not move, so no current part is rebuilt and no record
    /// resolved by current authority loses readable content. An older read view, including one
    /// pinned to a retained manifest revision, may lose readability when this operation punches
    /// bytes that current authority no longer needs.
    ///
    /// **Order matters and is the whole safety argument**: the manifest names the punched blocks
    /// BEFORE the bytes go, so a crash between the two leaves blocks marked punched that are
    /// still readable (harmless — the next call re-punches them), never punched blocks that
    /// nothing accounts for (an ops fire drill: zeros that look exactly like corruption).
    ///
    /// Requires a flushed memtable, for the same reason a re-fold does: staged records reference
    /// content this would otherwise consider unreachable.
    pub fn punch_unreferenced(&mut self) -> Result<ContentPunchStats> {
        self.punch_unreferenced_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::punch_unreferenced`] with cooperative, resumable block checkpoints.
    ///
    /// Cancellation after the manifest declaration may leave some dead blocks declared but not yet
    /// deallocated. That is a safe, durable partial state: reads already treat the blocks as erased,
    /// and a later call retries every still-present declared block.
    pub fn punch_unreferenced_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<ContentPunchStats> {
        let started = std::time::Instant::now();
        let result = self.punch_unreferenced_inner_with_control(control);
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::ContentPunch,
            started.elapsed(),
            &result,
        );
        result
    }

    fn punch_unreferenced_inner_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<ContentPunchStats> {
        self.ensure_writer_usable()?;
        control.check("content punching")?;
        if !self.mem.is_empty() {
            bail!("punching requires a flushed memtable; call sync() and flush() first");
        }
        // Every block a present record can still reach through its owning Part dictionary.
        let live_blocks: HashSet<u32> = self
            .live_fold_pieces_with_control(control)?
            .keys()
            .map(|location| location.block_id)
            .collect();
        // ... against every block the fold holds.
        let mut dead: Vec<u32> =
            self.fold.block_ids().into_iter().filter(|b| !live_blocks.contains(b)).collect();
        dead.sort_unstable();
        let already: HashSet<u32> =
            self.manifest.punched.iter().flat_map(|&(lo, hi)| lo..=hi).collect();
        if dead.is_empty() {
            return Ok(ContentPunchStats::default());
        }

        // Record first, punch second. Already-declared blocks stay in `dead`: a crash or
        // cancellation can land after this authority is durable but before every hole is punched,
        // and retrying those blocks is how the operation actually resumes.
        if dead.iter().any(|block| !already.contains(block)) {
            let prepared = (|| -> Result<Manifest> {
                control.check("content punching")?;
                let mut manifest = self.manifest.clone();
                let mut all: Vec<u32> = already.into_iter().chain(dead.iter().copied()).collect();
                all.sort_unstable();
                manifest.punched = to_ranges(&all);
                let tail = self.fold.sync()?;
                manifest.fold_seg = tail.seg;
                manifest.fold_off = tail.off;
                let mut container = self.container.lock().expect("container lock poisoned");
                manifest.commit_into_container(&mut container)?;
                // The declaration is a manifest-revision publication like any other: it prunes
                // the oldest retained revision, so members only that revision pinned must join
                // the free list in the same container state. Every open validates the exact
                // member namespace, and a part no manifest names would otherwise refuse the store.
                sweep_unreachable_container(&mut container, &manifest, self.read_limits)?;
                Ok(manifest)
            })();
            let m = match prepared {
                Ok(manifest) => manifest,
                Err(error) => {
                    return Err(
                        self.discard_staged_and_require_reopen("content-punch preparation", error)
                    );
                }
            };
            let mut c = self.container.lock().expect("container lock poisoned");
            let publication_error = match c.commit() {
                Ok(_) => None,
                Err(error) if c.failed_publication_selected() == Some(true) => Some(error),
                Err(error) => return Err(error),
            }; // the declaration is selected BEFORE any byte is destroyed
            drop(c);
            self.manifest = m;
            self.note_manifest_commit();
            self.fold.declare_punched(&self.manifest.punched);
            if let Some(error) = publication_error {
                return Err(error).context(
                    "container selected the content-punch declaration, but its final synchronization failed",
                );
            }
        }
        // Declare before destroying, the same order the manifest write follows and for the same
        // reason: at no point may a block's bytes be gone while this fold still calls it content.
        self.fold.declare_punched(&self.manifest.punched);

        // A selected declaration whose final publication barrier returned an error is current to
        // this process but not yet safe as crash authority. Obtain a successful barrier before
        // destroying bytes; otherwise a crash could revive the predecessor without the punched
        // declaration while leaving the holes behind.
        self.container.lock().expect("container lock poisoned").acknowledge_current_state()?;

        let punched = self.fold.punch_blocks_with_control(&dead, control)?;
        Ok(ContentPunchStats { blocks_punched: punched.len(), blocks_examined: dead.len() })
    }

    /// Rewrite the fold, keeping only content that live records still reference.
    ///
    /// The only operation that rewrites reachable fold content and rebuilds parts around its new
    /// locations. Content punch can deallocate payloads already declared unreachable; refold moves
    /// the content that remains, which is why this is a separate call rather than a merge flag.
    ///
    /// Requires a flushed memtable — staged records reference the old fold, and rebuilding parts under
    /// them would leave their pieces unresolvable. Returns no-op statistics without a transition
    /// when the current authority references no parts.
    pub fn refold(&mut self) -> Result<refold::RefoldStats> {
        self.refold_with_control(&crate::control::OperationControl::default())
    }

    /// Estimate duplicate-generation space for a future refold without decoding records/content.
    ///
    /// Source and retention bytes are exact. The stage estimate assumes the current logical fold
    /// plus uncompressed part sections and explicit framing allowance; it is conservative planning
    /// evidence, not a hard admission bound.
    pub fn estimate_refold_space(&self) -> Result<Option<RefoldSpaceEstimate>> {
        self.estimate_refold_space_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::estimate_refold_space`] with cooperative source and inventory checkpoints.
    pub fn estimate_refold_space_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<Option<RefoldSpaceEstimate>> {
        control.check("refold space preflight")?;
        if !self.mem.is_empty() {
            bail!("refold space estimation requires a flushed memtable; call sync() and flush() first");
        }
        if self.parts.is_empty() {
            return Ok(None);
        }
        let source_fold_logical_bytes = self.fold.disk_bytes();
        let mut source_part_bytes = 0u64;
        let mut source_part_sections = 0usize;
        let mut source_part_raw_section_bytes = 0u64;
        for (part, part_ref) in self.parts.iter().zip(&self.manifest.parts) {
            control.check("refold space preflight")?;
            source_part_bytes = source_part_bytes
                .checked_add(self.part_member_bytes(&part_ref.member)?)
                .ok_or_else(|| anyhow::anyhow!("refold source part byte count overflow"))?;
            for (_, _, raw, _) in part.sections() {
                control.check("refold space preflight")?;
                source_part_sections = source_part_sections
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("refold section count overflow"))?;
                source_part_raw_section_bytes = source_part_raw_section_bytes
                    .checked_add(u64::from(raw))
                    .ok_or_else(|| anyhow::anyhow!("refold raw section byte count overflow"))?;
            }
        }
        let rows = self.manifest.parts.iter().try_fold(0u64, |rows, part| {
            rows.checked_add(u64::from(part.records))
                .ok_or_else(|| anyhow::anyhow!("refold row count overflow"))
        })?;
        let row_allowance = rows
            .checked_mul(64)
            .ok_or_else(|| anyhow::anyhow!("refold row framing estimate overflow"))?;
        let section_allowance = u64::try_from(source_part_sections)
            .map_err(|_| anyhow::anyhow!("refold section count exceeds u64"))?
            .checked_mul(256)
            .ok_or_else(|| anyhow::anyhow!("refold section framing estimate overflow"))?;
        let estimated_stage_bytes = source_fold_logical_bytes
            .checked_add(source_part_raw_section_bytes)
            .and_then(|bytes| bytes.checked_add(row_allowance))
            .and_then(|bytes| bytes.checked_add(section_allowance))
            .and_then(|bytes| bytes.checked_add(1 << 20))
            .ok_or_else(|| anyhow::anyhow!("refold stage estimate overflow"))?;
        let usage = self.space_usage_with_control(control)?;
        Ok(Some(RefoldSpaceEstimate {
            source_fold_logical_bytes,
            source_part_bytes,
            source_part_sections,
            source_part_raw_section_bytes,
            retained_only_bytes_before: usage.retained_only.logical_bytes,
            estimated_stage_bytes,
            estimate_is_hard_bound: false,
            filesystem_available_bytes: usage.filesystem_available_bytes,
        }))
    }

    /// [`Store::refold`] with cooperative checkpoints before the generation swap.
    ///
    /// Cancellation removes the unpublished generation and rebuilt parts. Once publication is
    /// attempted, cancellation is no longer observed: the crash-safe commit protocol and mandatory
    /// handle/retention cleanup must run to a definite result.
    pub fn refold_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<refold::RefoldStats> {
        let started = std::time::Instant::now();
        let result = self.refold_inner_with_control(control);
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::Refold,
            started.elapsed(),
            &result,
        );
        result
    }

    fn refold_inner_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<refold::RefoldStats> {
        self.ensure_writer_usable()?;
        control.check("content refold")?;
        if !self.mem.is_empty() {
            bail!("refold requires a flushed memtable; call sync() and flush() first");
        }
        if self.parts.is_empty() {
            return Ok(refold::RefoldStats::default());
        }
        self.refold_in_file(self.container.clone(), control)
    }

    /// The refold's single-file form. The build stages a whole new generation and its rebuilt
    /// parts as uncommitted members; publication is ONE flip carrying the swap, the retained-log
    /// purge, and the sweep's frees. A crash cannot land between the swap and purge because they
    /// are one commit. Time travel does not cross a refold.
    fn refold_in_file(
        &mut self,
        container: std::sync::Arc<std::sync::Mutex<crate::container::Container>>,
        control: &crate::control::OperationControl,
    ) -> Result<refold::RefoldStats> {
        let seqs: Vec<(u64, u64)> =
            self.manifest.parts.iter().map(|p| (p.seq_lo, p.seq_hi)).collect();
        let built_generation = refold::refold_into_container_with_control_and_limits(
            container.clone(),
            &self.parts,
            &seqs,
            &self.fold,
            self.manifest.fold_gen,
            self.cfg,
            control,
            self.read_limits,
        );
        let (new_gen, built, mut stats, nf) = match built_generation {
            Ok(built) => built,
            Err(error) => {
                return match container.lock().expect("container lock poisoned").discard_staged() {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        self.requires_reopen = true;
                        Err(cleanup.context(format!(
                            "refold build failed ({error:#}); discarding its isolated staged generation also failed and writer requires reopen"
                        )))
                    }
                };
            }
        };

        let mut m = self.manifest.clone();
        m.parts = built
            .iter()
            .map(|(file, lo, hi, n, b3)| PartRef {
                member: file.clone(),
                seq_lo: *lo,
                seq_hi: *hi,
                records: *n,
                b3: b3.clone(),
            })
            .collect();
        m.fold_gen = new_gen;
        // Block ids are PER GENERATION; the new fold has no holes to declare.
        m.punched.clear();
        let t = nf.tail();
        m.fold_seg = t.seg;
        m.fold_off = t.off;
        if let Err(error) = control.check("content refold publication") {
            return match container.lock().expect("container lock poisoned").discard_staged() {
                Ok(()) => Err(error.into()),
                Err(cleanup) => {
                    self.requires_reopen = true;
                    Err(cleanup.context(format!(
                        "content refold was cancelled ({error}); discarding its isolated staged generation also failed and writer requires reopen"
                    )))
                }
            };
        }
        let staged = (|| -> Result<()> {
            let mut c = container.lock().expect("container lock poisoned");
            m.commit_into_container(&mut c)?;
            // The purge, staged into the SAME commit: every retained manifest except this
            // commit's own goes, because a retained name would keep the superseded generation —
            // deleted content included — readable for MANIFEST_RETAIN more commits.
            for commit in container_retained_commits(&c) {
                if commit != m.commit {
                    c.remove(&format!("MANIFEST.{commit:08}"))?;
                }
            }
            // With no retained pins left, the sweep frees the old generation and every
            // superseded part in the same state that abandons them.
            sweep_unreachable_container(&mut c, &m, self.read_limits)?;
            Ok(())
        })();
        if let Err(error) = staged {
            return match container.lock().expect("container lock poisoned").discard_staged() {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    self.requires_reopen = true;
                    Err(cleanup.context(format!(
                        "refold preparation failed ({error:#}); discarding its isolated staged generation also failed and writer requires reopen"
                    )))
                }
            };
        }
        // Open every newly staged part while the predecessor is still authoritative. After the
        // container-state flip, Store adoption must be a nonfallible pointer/state swap; otherwise
        // a transient extent read can leave this live writer holding the successor manifest, a
        // partial part list, and the predecessor Fold.
        let part_cache_budget = self.pcache.budget();
        let new_cache = Arc::new(SectionCache::new(part_cache_budget));
        let prepared_parts = (|| -> Result<Vec<Arc<Part>>> {
            let mut parts = Vec::new();
            parts.try_reserve_exact(m.parts.len()).context("reserve refolded part handles")?;
            for part in &m.parts {
                let reader = container
                    .lock()
                    .expect("container lock poisoned")
                    .extent(&part.member)
                    .ok_or_else(|| {
                        anyhow::anyhow!("refold staged {} but the container lost it", part.member)
                    })?;
                parts.push(Arc::new(Part::open_reader_with_limits(
                    Box::new(reader),
                    new_cache.clone(),
                    self.read_limits,
                )?));
            }
            Ok(parts)
        })();
        let prepared_parts = match prepared_parts {
            Ok(parts) => parts,
            Err(error) => {
                return match container.lock().expect("container lock poisoned").discard_staged() {
                    Ok(()) => Err(error).context("open staged refold parts before publication"),
                    Err(cleanup) => {
                        self.requires_reopen = true;
                        Err(cleanup.context(format!(
                            "opening staged refold parts failed ({error:#}); discarding them also failed and writer requires reopen"
                        )))
                    }
                };
            }
        };
        let publication_error = {
            let mut c = container.lock().expect("container lock poisoned");
            match c.commit() {
                Ok(_) => None,
                Err(error) if c.failed_publication_selected() == Some(true) => Some(error),
                Err(error) => return Err(error).context("publish the rebuilt fold generation"),
            }
        };

        self.manifest = m;
        self.note_manifest_commit();
        self.retained_commit_count = 1;
        self.pcache = new_cache;
        self.parts = prepared_parts;
        self.fold = nf;
        // Freed in the same flip that abandoned it: there is no stale generation to report.
        stats.stale_generation_left = false;
        if let Some(error) = publication_error {
            return Err(error).context(
                "container selected the refolded manifest revision, but its final synchronization failed",
            );
        }
        Ok(stats)
    }

    /// Return the space this file's history has already abandoned: deallocate the aligned
    /// interior of free extents older than the retention window, in place, offsets unmoved.
    ///
    /// The single-file complement to [`Store::punch_unreferenced`]: that one destroys dead
    /// CONTENT blocks under a manifest declaration; this one destroys extents the sweep already
    /// free-listed — superseded parts, purged manifests, abandoned fold generations — whose only
    /// remaining claim is the free list itself. The grace window (the manifest retention window,
    /// in commits) is what keeps a reader holding a recent superblock exact rather than erroring;
    /// a reader older than that reads zeros and fails checksums — detected, never silent.
    ///
    pub fn punch_free_space(&mut self) -> Result<crate::container::FreePunchStats> {
        self.ensure_writer_usable()?;
        let mut c = self.container.lock().expect("container lock poisoned");
        c.acknowledge_current_state()?;
        c.punch_free_extents(MANIFEST_RETAIN as u64)
    }

    /// Bytes pinned by every open part's section caches, against their shared budget.
    pub fn part_cache_bytes(&self) -> (usize, usize) {
        (self.pcache.bytes(), self.pcache.budget())
    }

    /// Pieces resident in the Tier-0 dedup window. Bounded by the flush interval, not by store size.
    pub fn dedup_window_len(&self) -> usize {
        self.fold.window_len()
    }
    /// Return the in-memory authority fields. `commit == 0` encodes the canonical origin and is not
    /// a manifest revision.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn fold(&self) -> &Fold {
        &self.fold
    }
    pub fn wal_bytes(&self) -> u64 {
        self.wal.bytes()
    }

    /// A constant-work snapshot of operational state. No records or content are decoded.
    pub fn health(&self) -> StoreHealth {
        let fold_cache = self.fold.cache_stats();
        let (part_cache_bytes, part_cache_budget) = self.part_cache_bytes();
        let punched_blocks =
            self.manifest.punched.iter().map(|&(lo, hi)| u64::from(hi) - u64::from(lo) + 1).sum();
        StoreHealth {
            commit: self.manifest.commit,
            fold_generation: self.manifest.fold_gen,
            parts: self.parts.len(),
            part_rows: self.manifest.parts.iter().map(|part| u64::from(part.records)).sum(),
            memtable_entries: self.mem.len(),
            memtable_bytes: self.mem_bytes,
            wal_bytes: self.wal.bytes(),
            wal_frames: self.wal.frame_count(),
            fold_disk_bytes: self.fold.disk_bytes(),
            fold_segments: self.fold.segment_count(),
            fold_cache_hits: fold_cache.hits,
            fold_cache_misses: fold_cache.misses,
            fold_cache_bytes: fold_cache.bytes,
            fold_cache_budget: fold_cache.budget,
            fold_block_target_bytes: self.cfg.block_target,
            fold_segment_max_bytes: self.cfg.seg_max,
            fold_compression_level: self.cfg.level,
            fold_compression_threads: self.cfg.compress_threads,
            part_cache_bytes,
            part_cache_budget,
            max_stored_frame_bytes: self.read_limits.max_stored_frame_bytes,
            max_decoded_frame_bytes: self.read_limits.max_decoded_frame_bytes,
            max_directory_entries: self.read_limits.max_directory_entries,
            max_wal_frames: self.read_limits.max_wal_frames,
            max_fold_blocks: self.read_limits.max_fold_blocks,
            dedup_window_entries: self.fold.window_len(),
            retained_commits: self.retained_commit_count,
            punched_blocks,
        }
    }

    /// Classify container bytes by manifest reachability.
    ///
    /// This is intentionally separate from [`Store::health`]: it performs filesystem traversal and
    /// parses the retained window, so callers opt into its cost. No records, column values, or
    /// content are decoded.
    pub fn space_usage(&self) -> Result<StoreSpaceUsage> {
        self.space_usage_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::space_usage`] with cooperative checks between manifests and filesystem entries.
    pub fn space_usage_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<StoreSpaceUsage> {
        let c = self.container.lock().expect("container lock poisoned");
        container_space_usage(&c, &self.path, &self.manifest, self.read_limits, control)
    }

    /// Discover attribute names/types and named-content columns without decoding stored values.
    pub fn schema(&self) -> Result<crate::schema::Schema> {
        let mut schema = crate::schema::Builder::default();
        schema.add_parts(&self.parts)?;
        for record in self.mem.values().flatten() {
            schema.add_record(record);
        }
        Ok(schema.finish())
    }
}

/// [`store_space_usage`]'s single-file form: members classified by the same reachability rules
/// — current manifest revision, retained manifest revisions, and their fold generations — with the WAL sidecar counted
/// live and the free list counted unclassified: bytes the file holds that no reachable name
/// claims, which is exactly what that bucket means. `total` is the additive total of classified
/// member payload, free extents, and the WAL sidecar. Container superblocks, directory encodings,
/// and alignment padding are structural overhead and are deliberately outside these buckets.
fn container_space_usage(
    c: &crate::container::Container,
    path: &Path,
    live_manifest: &Manifest,
    _read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<StoreSpaceUsage> {
    control.check("store space inventory")?;
    let mut live_members: std::collections::HashSet<String> =
        std::iter::once("MANIFEST".to_string()).collect();
    live_members.extend(live_manifest.parts.iter().map(|p| p.member.clone()));
    let live_prefix = crate::fold::fold_member_prefix(live_manifest.fold_gen);

    let mut retained_members: std::collections::HashSet<String> = Default::default();
    let mut retained_prefixes: std::collections::HashSet<String> = Default::default();
    for commit in container_retained_commits(c) {
        control.check("store space inventory")?;
        let name = format!("MANIFEST.{commit:08}");
        let bytes = c
            .read_file_bounded(&name, MAX_MANIFEST_BYTES)
            .with_context(|| format!("account retained manifest {commit}"))?;
        let m = Manifest::parse(&bytes)
            .with_context(|| format!("account retained manifest {commit}"))?;
        retained_members.insert(name);
        retained_members.extend(m.parts.into_iter().map(|p| p.member));
        retained_prefixes.insert(crate::fold::fold_member_prefix(m.fold_gen));
    }

    let mut usage = StoreSpaceUsage {
        filesystem_available_bytes: crate::sys::filesystem_available_bytes(
            path.parent().unwrap_or_else(|| Path::new(".")),
        )
        .with_context(|| format!("measure available filesystem bytes at {}", path.display()))?,
        ..StoreSpaceUsage::default()
    };
    let add = |amount: &mut SpaceAmount, bytes: u64| {
        amount.members += 1;
        amount.logical_bytes += bytes;
    };
    for name in c.names().map(String::from).collect::<Vec<String>>() {
        control.check("store space inventory")?;
        let len = c.member_len(&name).unwrap_or(0);
        let gen = fold_generation_of_member(&name);
        let is_live = live_members.contains(&name)
            || gen.map(|g| crate::fold::fold_member_prefix(g) == live_prefix).unwrap_or(false);
        let is_retained = retained_members.contains(&name)
            || gen
                .map(|g| retained_prefixes.contains(&crate::fold::fold_member_prefix(g)))
                .unwrap_or(false);
        if is_live {
            add(&mut usage.live, len);
        } else if is_retained {
            add(&mut usage.retained_only, len);
        } else {
            add(&mut usage.unclassified, len);
        }
    }
    let wal = file_wal_path(path);
    if let Ok(meta) = std::fs::metadata(&wal) {
        add(&mut usage.live, meta.len());
    }
    // The free list: bytes present under no reachable name. Zero members — extents are not members.
    usage.unclassified.logical_bytes += c.free_bytes();
    // Totals stay bucket-additive — the accounting identity every consumer of this struct
    // checks. Structural overhead (superblocks, the directory, alignment padding) is uncounted;
    // the file's own length is one stat away for anyone asking the other question.
    usage.total.members =
        usage.live.members + usage.retained_only.members + usage.unclassified.members;
    usage.total.logical_bytes = usage.live.logical_bytes
        + usage.retained_only.logical_bytes
        + usage.unclassified.logical_bytes;
    Ok(usage)
}

/// Exact physical-input bounds for one incremental compaction step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionBudget {
    /// Maximum number of adjacent immutable parts admitted to one work unit.
    pub max_input_parts: usize,
    /// Maximum physical rows across the admitted input parts.
    pub max_input_rows: u64,
    /// Maximum sum of the admitted input part member lengths.
    pub max_input_bytes: u64,
}

impl CompactionBudget {
    /// Reject structurally unusable limits before any maintenance work begins.
    pub fn validate(self) -> std::result::Result<(), CompactionError> {
        if self.max_input_parts < 2 {
            return Err(CompactionError::InvalidBudget(
                "max_input_parts must be at least 2".into(),
            ));
        }
        if self.max_input_rows == 0 {
            return Err(CompactionError::InvalidBudget(
                "max_input_rows must be greater than zero".into(),
            ));
        }
        if self.max_input_bytes == 0 {
            return Err(CompactionError::InvalidBudget(
                "max_input_bytes must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// An exact contiguous input selection from the part list referenced by current authority.
pub struct CompactionPlan {
    /// Zero-based index of the oldest selected part.
    pub start_part: usize,
    /// Number of adjacent parts selected.
    pub input_parts: usize,
    /// Physical rows across the selected part members.
    pub input_rows: u64,
    /// Sum of the selected part members' exact logical lengths.
    pub input_bytes: u64,
    /// Whether this run covers the complete referenced part list and may drop tombstones.
    pub drops_tombstones: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Space facts and an explicitly advisory stage estimate for one compaction plan.
pub struct CompactionSpaceEstimate {
    pub plan: CompactionPlan,
    pub input_sections: usize,
    pub input_raw_section_bytes: u64,
    pub estimated_stage_bytes: u64,
    pub estimate_is_hard_bound: bool,
    /// Selected inputs remain pinned by the immediately preceding retained manifest after commit.
    pub retained_input_bytes_after_commit: u64,
    pub filesystem_available_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
/// Evidence returned after one bounded compaction work unit is published.
pub struct BoundedCompaction {
    /// The exact input plan that was executed.
    pub plan: CompactionPlan,
    /// Exact on-disk length of the newly published output part.
    pub output_bytes: u64,
    /// Logical merge counters.
    pub merge: crate::part::merge::MergeStats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A budget that cannot describe or admit a compaction work unit.
pub enum CompactionError {
    /// One or more limits are structurally unusable.
    InvalidBudget(String),
    /// At least two parts exist, but no adjacent pair fits all supplied limits.
    BudgetTooSmall {
        /// Start of a concrete smallest-byte adjacent pair.
        start_part: usize,
        /// Physical rows required by that pair.
        input_rows: u64,
        /// Exact file bytes required by that pair.
        input_bytes: u64,
        /// Limits that rejected the pair.
        budget: CompactionBudget,
    },
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactionError::InvalidBudget(reason) => write!(f, "invalid compaction budget: {reason}"),
            CompactionError::BudgetTooSmall {
                start_part,
                input_rows,
                input_bytes,
                budget,
            } => write!(
                f,
                "no adjacent part pair fits the compaction budget; the smallest-byte pair starts at part {start_part} and needs {input_rows} rows / {input_bytes} bytes, limits are {} rows / {} bytes",
                budget.max_input_rows,
                budget.max_input_bytes
            ),
        }
    }
}

impl std::error::Error for CompactionError {}

/// What [`Store::punch_unreferenced`] did.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContentPunchStats {
    pub blocks_examined: usize,
    /// Fewer than examined when blocks sit in the active segment, which is never punched.
    pub blocks_punched: usize,
}

/// Collapse a sorted id list into inclusive ranges.
fn to_ranges(ids: &[u32]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &id in ids {
        match out.last_mut() {
            Some(last) if last.1 + 1 == id => last.1 = id,
            Some(last) if last.1 >= id => {}
            _ => out.push((id, id)),
        }
    }
    out
}

/// What an erasure did.
#[derive(Clone, Copy, Debug)]
pub struct ErasureStats {
    pub requested: usize,
    pub tombstoned: usize,
    /// Named but already gone. A normal outcome, recorded rather than errored.
    pub absent: usize,
    /// Live records remaining after this operation completed.
    pub remaining: usize,
    /// `None` when nothing existed to erase and the store was left untouched.
    pub refold: Option<refold::RefoldStats>,
}

/// A reader over one store authority. No lock, no writer, no daemon. Clones retain the same
/// immutable parts and fold handles, making ownership by concurrent query streams cheap.
#[derive(Clone)]
pub struct ReadStore {
    fold: Arc<Fold>,
    parts: Vec<Arc<Part>>,
    manifest: Manifest,
    read_limits: ReadLimits,
}

/// A read-only store is the authority-pinned read core, with nothing layered on top — so every method here
/// is a direct delegation, and there is no second implementation to keep in step.
impl ReadStore {
    /// Atomic persisted-frame admission governing this immutable handle.
    pub fn read_limits(&self) -> ReadLimits {
        self.read_limits
    }

    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        verification_integrity("read committed record", read::get(&self.parts, id))
    }

    pub(crate) fn project_candidates(
        &self,
        candidates: &[crate::scan::ScanCandidate],
        attrs: &HashSet<&str>,
        contents: &HashSet<&str>,
    ) -> Result<Vec<Record>> {
        let committed: Result<Vec<_>> = candidates
            .iter()
            .map(|candidate| match candidate {
                crate::scan::ScanCandidate::Committed(row) => Ok(row),
                crate::scan::ScanCandidate::Memtable(id) => {
                    bail!("immutable snapshot received memtable row {id:?}")
                }
            })
            .collect();
        read::project_rows(&self.parts, &committed?, attrs, contents)
    }

    pub(crate) fn reconstruct_candidate_content(
        &self,
        candidate: &crate::scan::ScanCandidate,
        content: &Content,
    ) -> Result<Vec<u8>> {
        match candidate {
            crate::scan::ScanCandidate::Committed(row) => {
                read::reconstruct_projected_content(&self.parts, &self.fold, row, content)
            }
            crate::scan::ScanCandidate::Memtable(id) => {
                bail!("immutable snapshot received memtable row {id:?}")
            }
        }
    }

    pub fn reconstruct(&self, id: &str) -> Result<Option<Vec<u8>>> {
        verification_integrity(
            "reconstruct committed content",
            read::reconstruct(&self.parts, &self.fold, id),
        )
    }

    /// Byte-exact named content, if both the record and value are present.
    pub fn reconstruct_content(&self, id: &str, name: &str) -> Result<Option<Vec<u8>>> {
        verification_integrity(
            "reconstruct committed content",
            read::reconstruct_content(&self.parts, &self.fold, id, name),
        )
    }

    /// Distinct committed ids, sorted — the union across parts, newest-wins.
    pub fn ids(&self) -> Result<Vec<String>> {
        read::ids(&self.parts)
    }

    /// Bounded structured paging over this immutable read view.
    pub fn scan(&self, request: &crate::scan::ScanRequest) -> Result<crate::scan::ScanPage> {
        crate::scan::scan_read_store(self, request)
    }

    pub(crate) fn candidate_may_match(
        &self,
        candidate: &crate::scan::ScanCandidate,
        predicates: &[crate::scan::Predicate],
    ) -> Result<bool> {
        let crate::scan::ScanCandidate::Committed(row) = candidate else {
            return Ok(true);
        };
        match row.origin {
            read::RowOrigin::Part { part, .. } => read::part_may_match(
                self.parts
                    .get(part)
                    .ok_or_else(|| anyhow::anyhow!("candidate part is outside the snapshot"))?,
                predicates,
            ),
            read::RowOrigin::Memtable => Ok(true),
        }
    }

    /// Explain a structured scan against this immutable snapshot without resolving rows or
    /// evaluating predicates.
    pub fn explain_scan(
        &self,
        request: &crate::scan::ScanRequest,
    ) -> Result<crate::scan::ScanExplanation> {
        crate::scan::explain_read_store(self, request)
    }

    pub(crate) fn scan_physical_scope(
        &self,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<crate::scan::ScanPhysicalScope> {
        read::scan_physical_scope(&self.parts, from, to)
    }

    /// Live ids in `[from, to)`, at most `limit`, id-ordered or reversed — the paged read.
    ///
    /// Only ids inside the range are decoded, so the cost tracks the page rather than the store.
    /// Ids sort lexicographically, so ids designed with the query in mind (a `member/timestamp/…`
    /// prefix) give member-then-time paging with no secondary index.
    pub fn scan_ids(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        reverse: bool,
    ) -> Result<Vec<String>> {
        Ok(self
            .scan_candidates(from, to, limit, reverse, usize::MAX, true)?
            .candidates
            .into_iter()
            .map(crate::scan::ScanCandidate::into_id)
            .collect())
    }

    pub(crate) fn scan_candidates(
        &self,
        from: Option<&str>,
        to: Option<&str>,
        limit: usize,
        reverse: bool,
        max_resolution_entries: usize,
        allow_oversized_group: bool,
    ) -> Result<crate::scan::CandidateBatch> {
        check_range(from, to)?;
        let resolved = read::scan_rows(
            &self.parts,
            std::iter::empty::<(&str, bool)>(),
            read::RowScan {
                from,
                to,
                limit,
                reverse,
                max_resolution_entries,
                allow_oversized_group,
            },
        )?;
        Ok(crate::scan::CandidateBatch {
            candidates: resolved
                .rows
                .into_iter()
                .map(crate::scan::ScanCandidate::Committed)
                .collect(),
            resolution: crate::scan::ScanResolutionStats {
                physical_rows: resolved.physical_rows,
                superseded_rows: resolved.superseded_rows,
                tombstones: resolved.tombstones,
                memtable_entries: resolved.memtable_entries,
                budget_exhausted: resolved.budget_exhausted,
            },
            resolved_through: resolved.resolved_through,
            has_more: resolved.has_more,
        })
    }

    /// Hand the fold and parts to a lens. Consumes the store because the query layer takes ownership
    /// of both; the store authority it was opened at is already baked into `parts`.
    pub fn into_parts(self) -> (Arc<Fold>, Vec<Arc<Part>>) {
        (self.fold, self.parts)
    }

    pub fn part_count(&self) -> usize {
        self.parts.len()
    }
    /// Parts referenced by this read view's store authority, oldest to newest.
    pub fn parts(&self) -> &[Arc<Part>] {
        &self.parts
    }
    /// The fold, for tools that scrub or measure it.
    pub fn fold(&self) -> &Fold {
        &self.fold
    }
    /// Return the in-memory authority fields. `commit == 0` encodes the canonical origin and is not
    /// a manifest revision.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Discover this immutable snapshot's physical field universe without decoding stored values.
    pub fn schema(&self) -> Result<crate::schema::Schema> {
        let mut schema = crate::schema::Builder::default();
        schema.add_parts(&self.parts)?;
        Ok(schema.finish())
    }
}

fn approx_bytes(r: &Record) -> usize {
    r.id.len()
        + r.contents
            .iter()
            .map(|content| {
                content.name.len()
                    + content
                        .ops
                        .iter()
                        .map(|o| match o {
                            BodyOp::Lit(b) => b.len() + 8,
                            BodyOp::Piece { .. } => 40,
                        })
                        .sum::<usize>()
            })
            .sum::<usize>()
        + r.attrs.iter().map(|(k, _)| k.len() + 24).sum::<usize>()
}

// Positioned reads are the one thing with no fallback: every read in the engine is "n bytes at
// offset o", and emulating that with seek-then-read is not safe across threads. Unix and WASI both
// provide it. What WASI does NOT provide — advisory locking and hole punching — is degraded
// explicitly in `crate::sys` rather than refused here.

#[cfg(test)]
mod tests {

    #[test]
    fn manifest_promotion_validates_piece_locations_through_each_rows_owning_part() {
        use crate::fold::block::{self, Loc, CODEC_STORED};
        use crate::fold::segment::SegHeader;
        use crate::types::{BodyOp, Content, ContentHash, PieceHash, Record, BODY_CONTENT};
        use std::collections::HashMap;
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!(
            "turndb-promotion-owning-part-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let fold_dir = root.join("fold");
        std::fs::create_dir_all(&fold_dir).unwrap();
        let bytes = b"same identity at two physical locations";
        let hash = PieceHash::of(bytes);
        let mut segment = SegHeader { seg: 0, flags: 0, dict_id: [0; 32] }.encode().to_vec();
        let mut frame = Vec::new();
        block::encode(&mut frame, 0, CODEC_STORED, bytes, bytes).unwrap();
        segment.extend_from_slice(&frame);
        block::encode(&mut frame, 1, CODEC_STORED, bytes, bytes).unwrap();
        segment.extend_from_slice(&frame);
        std::fs::write(fold_dir.join("seg-00000000.fold"), segment).unwrap();
        let cfg = crate::fold::FoldCfg::default();
        let fold = crate::fold::Fold::open_read_with_limits(
            &fold_dir,
            cfg,
            &[(0, 0)],
            crate::read_limits::ReadLimits::default(),
        )
        .unwrap();
        let loc0 = Loc { block_id: 0, in_off: 0, raw: bytes.len() as u32 };
        let loc1 = Loc { block_id: 1, in_off: 0, raw: bytes.len() as u32 };
        let record = |id: &str| {
            Record::new(
                id,
                vec![Content::identified(
                    BODY_CONTENT,
                    vec![BodyOp::Piece { hash, len: bytes.len() as u32 }],
                    ContentHash(hash.0),
                )],
                Vec::new(),
            )
            .unwrap()
        };
        let p1_path = root.join("p1.part");
        let p2_path = root.join("p2.part");
        crate::part::build_full(
            &p1_path,
            &[record("owned-by-punched")],
            &[],
            1,
            1,
            3,
            |_| Some(loc0),
            &HashMap::new(),
        )
        .unwrap();
        crate::part::build_full(
            &p2_path,
            &[record("unrelated-readable")],
            &[],
            2,
            2,
            3,
            |_| Some(loc1),
            &HashMap::new(),
        )
        .unwrap();
        let reader = super::ReadStore {
            fold: Arc::new(fold),
            parts: vec![
                Arc::new(crate::part::Part::open(&p1_path).unwrap()),
                Arc::new(crate::part::Part::open(&p2_path).unwrap()),
            ],
            manifest: super::Manifest::default(),
            read_limits: crate::read_limits::ReadLimits::default(),
        };
        let error = super::validate_candidate_records(
            &reader,
            &crate::control::OperationControl::default(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("ERASED"), "{error:#}");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn content_reachability_preserves_every_owning_location_for_one_piece_identity() {
        use crate::fold::block::{self, Loc, CODEC_STORED};
        use crate::fold::segment::SegHeader;
        use crate::types::{BodyOp, Content, ContentHash, PieceHash, Record, BODY_CONTENT};
        use std::collections::HashMap;
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!(
            "turndb-reachability-owning-part-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let fold_dir = root.join("fold");
        std::fs::create_dir_all(&fold_dir).unwrap();
        let bytes = b"one identity stored at two locations";
        let hash = PieceHash::of(bytes);
        let mut segment = SegHeader { seg: 0, flags: 0, dict_id: [0; 32] }.encode().to_vec();
        let mut frame = Vec::new();
        block::encode(&mut frame, 0, CODEC_STORED, bytes, bytes).unwrap();
        segment.extend_from_slice(&frame);
        block::encode(&mut frame, 1, CODEC_STORED, bytes, bytes).unwrap();
        segment.extend_from_slice(&frame);
        std::fs::write(fold_dir.join("seg-00000000.fold"), segment).unwrap();
        let fold = crate::fold::Fold::open_read_with_limits(
            &fold_dir,
            crate::fold::FoldCfg::default(),
            &[],
            crate::read_limits::ReadLimits::default(),
        )
        .unwrap();
        let loc0 = Loc { block_id: 0, in_off: 0, raw: bytes.len() as u32 };
        let loc1 = Loc { block_id: 1, in_off: 0, raw: bytes.len() as u32 };
        let record = |id: &str| {
            Record::new(
                id,
                vec![Content::identified(
                    BODY_CONTENT,
                    vec![BodyOp::Piece { hash, len: bytes.len() as u32 }],
                    ContentHash(hash.0),
                )],
                Vec::new(),
            )
            .unwrap()
        };
        let p1_path = root.join("p1.part");
        let p2_path = root.join("p2.part");
        crate::part::build_full(
            &p1_path,
            &[record("row-at-block-zero")],
            &[],
            1,
            1,
            3,
            |_| Some(loc0),
            &HashMap::new(),
        )
        .unwrap();
        crate::part::build_full(
            &p2_path,
            &[record("unrelated-row-at-block-one")],
            &[],
            2,
            2,
            3,
            |_| Some(loc1),
            &HashMap::new(),
        )
        .unwrap();
        let parts = vec![
            Arc::new(crate::part::Part::open(&p1_path).unwrap()),
            Arc::new(crate::part::Part::open(&p2_path).unwrap()),
        ];
        let live = super::live_fold_pieces_with_control(
            &parts,
            &fold,
            &crate::control::OperationControl::default(),
        )
        .unwrap();
        assert_eq!(live.len(), 2, "identity-keyed reachability would collapse one location");
        assert_eq!(live.get(&loc0), Some(&hash));
        assert_eq!(live.get(&loc1), Some(&hash));
        assert_eq!(
            live.keys().map(|loc| loc.block_id).collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([0, 1]),
            "content punch must retain both blocks named by their owning live rows"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn punched_crc_exemption_names_only_segments_that_contain_declared_blocks() {
        let root = std::env::temp_dir().join(format!(
            "turndb-punched-segments-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("store.turndb");
        let cfg = crate::fold::FoldCfg {
            block_target: 4 * 1024,
            seg_max: 16 * 1024,
            ..Default::default()
        };
        let noise = |seed: u64, len: usize| {
            let mut out = Vec::with_capacity(len);
            let mut hash = blake3::hash(&seed.to_le_bytes());
            while out.len() < len {
                out.extend_from_slice(hash.as_bytes());
                hash = blake3::hash(hash.as_bytes());
            }
            out.truncate(len);
            out
        };

        let mut store = super::Store::open_file(&path, cfg).unwrap();
        store.put("dead", &[super::Span::Piece(&noise(1, 6 * 1024))], vec![]).unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
        for index in 0..8u64 {
            store
                .put(
                    &format!("live:{index}"),
                    &[super::Span::Piece(&noise(10 + index, 6 * 1024))],
                    vec![],
                )
                .unwrap();
        }
        store.sync().unwrap();
        store.flush().unwrap();
        store.delete("dead").unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
        store.merge_range(0, store.part_count()).unwrap().unwrap();
        assert!(store.punch_unreferenced().unwrap().blocks_punched > 0);

        let container = crate::container::Container::open_internal(&path).unwrap();
        let segments = container
            .names()
            .filter(|name| name.ends_with(".fold"))
            .map(String::from)
            .collect::<std::collections::HashSet<_>>();
        let ignored = super::verified_punched_fold_members(
            &container,
            cfg,
            crate::read_limits::ReadLimits::default(),
            &crate::control::OperationControl::default(),
        )
        .unwrap();
        assert!(!ignored.is_empty(), "at least one segment contains a punched block");
        assert!(
            ignored.is_subset(&segments) && ignored.len() < segments.len(),
            "unaffected segments must retain their outer CRC: ignored={ignored:?}, segments={segments:?}"
        );
        drop(container);
        store.close().unwrap();
        std::fs::remove_dir_all(root).ok();
    }

    fn persisted_manifest() -> super::Manifest {
        super::Manifest { commit: 1, fold_off: 48, ..Default::default() }
    }

    fn manifest_bytes_with_part(member: &str) -> Vec<u8> {
        super::Manifest {
            parts: vec![super::PartRef {
                member: member.into(),
                seq_lo: 1,
                seq_hi: 1,
                records: 1,
                b3: "00".repeat(32),
            }],
            next_seq: 1,
            ..persisted_manifest()
        }
        .encode()
        .unwrap()
    }

    #[test]
    fn manifest_part_names_cannot_escape_the_store_root() {
        for hostile in
            ["../secret.part", "/absolute/secret.part", "nested/secret.part", "..\\secret.part"]
        {
            let error =
                super::Manifest::parse(&manifest_bytes_with_part(hostile)).unwrap_err().to_string();
            assert!(error.contains("canonical name"), "{hostile:?}: {error}");
        }
        assert!(
            super::Manifest::parse(&manifest_bytes_with_part("part-00000001.part")).is_ok(),
            "an ordinary current manifest is accepted"
        );
    }

    #[test]
    fn manifest_semantics_reject_ambiguous_or_malformed_authority() {
        let part = super::PartRef {
            member: "part-00000001.part".into(),
            seq_lo: 2,
            seq_hi: 1,
            records: 1,
            b3: "00".repeat(32),
        };
        let inverted = super::Manifest { parts: vec![part.clone()], ..persisted_manifest() };
        assert!(super::Manifest::parse(&inverted.encode().unwrap()).is_err());

        let duplicate = super::Manifest {
            parts: vec![
                super::PartRef { seq_lo: 1, seq_hi: 1, ..part.clone() },
                super::PartRef { seq_lo: 2, seq_hi: 2, ..part.clone() },
            ],
            ..persisted_manifest()
        };
        assert!(super::Manifest::parse(&duplicate.encode().unwrap()).is_err());

        let bad_digest = super::Manifest {
            parts: vec![super::PartRef {
                seq_lo: 1,
                seq_hi: 1,
                b3: "not-a-blake3-digest".into(),
                ..part
            }],
            ..persisted_manifest()
        };
        assert!(super::Manifest::parse(&bad_digest.encode().unwrap()).is_err());

        let overlapping = super::Manifest {
            parts: vec![
                super::PartRef {
                    member: "part-00000001-00000003.part".into(),
                    seq_lo: 1,
                    seq_hi: 3,
                    records: 1,
                    b3: "00".repeat(32),
                },
                super::PartRef {
                    member: "part-00000003-00000004.part".into(),
                    seq_lo: 3,
                    seq_hi: 4,
                    records: 1,
                    b3: "11".repeat(32),
                },
            ],
            ..persisted_manifest()
        };
        assert!(super::Manifest::parse(&overlapping.encode().unwrap()).is_err());

        let stale_cursor = super::Manifest {
            parts: vec![super::PartRef {
                member: "part-00000001-00000007.part".into(),
                seq_lo: 1,
                seq_hi: 7,
                records: 1,
                b3: "22".repeat(32),
            }],
            next_seq: 6,
            ..persisted_manifest()
        };
        assert!(super::Manifest::parse(&stale_cursor.encode().unwrap()).is_err());
        let current_cursor = super::Manifest { next_seq: 7, ..stale_cursor };
        assert!(super::Manifest::parse(&current_cursor.encode().unwrap()).is_ok());
        let ahead_cursor = super::Manifest { next_seq: 8, ..current_cursor };
        assert!(super::Manifest::parse(&ahead_cursor.encode().unwrap()).is_err());

        let zero_sequence = super::Manifest {
            parts: vec![super::PartRef {
                member: "part-00000000.part".into(),
                seq_lo: 0,
                seq_hi: 0,
                records: 0,
                b3: "33".repeat(32),
            }],
            next_seq: 0,
            ..persisted_manifest()
        };
        assert!(super::Manifest::parse(&zero_sequence.encode().unwrap()).is_err());

        let gap = super::Manifest {
            parts: vec![
                super::PartRef {
                    member: "part-00000001.part".into(),
                    seq_lo: 1,
                    seq_hi: 1,
                    records: 1,
                    b3: "44".repeat(32),
                },
                super::PartRef {
                    member: "part-00000003.part".into(),
                    seq_lo: 3,
                    seq_hi: 3,
                    records: 1,
                    b3: "55".repeat(32),
                },
            ],
            next_seq: 3,
            ..persisted_manifest()
        };
        assert!(super::Manifest::parse(&gap.encode().unwrap()).is_err());

        let punched = super::Manifest { punched: vec![(5, 9), (9, 12)], ..Default::default() };
        assert!(super::Manifest::parse(&punched.encode().unwrap()).is_err());
    }

    #[test]
    fn manifest_origin_and_fold_tail_have_one_canonical_shape() {
        assert!(super::Manifest::parse(&super::Manifest::default().encode().unwrap()).is_err());
        let origin = persisted_manifest();
        assert!(super::Manifest::parse(&origin.encode().unwrap()).is_ok());

        let false_origin = super::Manifest { prev: Some("00".repeat(32)), ..origin.clone() };
        assert!(super::Manifest::parse(&false_origin.encode().unwrap()).is_err());
        let missing_predecessor = super::Manifest { commit: 2, ..origin.clone() };
        assert!(super::Manifest::parse(&missing_predecessor.encode().unwrap()).is_err());
        let unsupported_cursor = super::Manifest { next_seq: 1, ..origin.clone() };
        assert!(super::Manifest::parse(&unsupported_cursor.encode().unwrap()).is_err());
        let impossible_tail = super::Manifest { fold_seg: 1, fold_off: 0, ..origin };
        assert!(super::Manifest::parse(&impossible_tail.encode().unwrap()).is_err());
    }

    #[test]
    fn container_fold_sidecars_obey_explicit_read_limits_without_advisory_fallback() {
        let root = std::env::temp_dir().join(format!(
            "turndb-sidecar-limits-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("store.turndb");
        let cfg =
            crate::fold::FoldCfg { block_target: 1, seg_max: 16 * 1024, ..Default::default() };
        let mut store = super::Store::open_file(&path, cfg).unwrap();
        for index in 0..1_200u32 {
            let bytes = index.to_le_bytes();
            store
                .put(&format!("id:{index:04}"), &[super::Span::Piece(&bytes)], Vec::new())
                .unwrap();
        }
        store.sync().unwrap();
        store.flush().unwrap();
        store.close().unwrap();

        let container = crate::container::Container::open(&path).unwrap();
        let manifest = super::Manifest::parse(
            &container.read_file_bounded("MANIFEST", super::MAX_MANIFEST_BYTES).unwrap(),
        )
        .unwrap();
        let sidecar_len = container
            .names()
            .filter(|name| name.ends_with(".dir"))
            .filter_map(|name| container.member_len(name))
            .max()
            .expect("the small-segment fixture must produce a directory sidecar");
        assert!(sidecar_len > 1);

        let strict = crate::read_limits::ReadLimits {
            max_stored_frame_bytes: sidecar_len - 1,
            ..Default::default()
        };
        let error = super::open_fold_from_container(&container, &manifest, cfg, &path, strict)
            .err()
            .expect("an over-budget sidecar must refuse rather than fall back");
        assert_eq!(crate::error::classify(&error), crate::error::ErrorClass::ResourceExhausted);
        assert!(matches!(
            error.downcast_ref::<crate::read_limits::ReadAdmissionError>(),
            Some(crate::read_limits::ReadAdmissionError::StoredFrameTooLarge { frame, .. })
                if frame.contains("fold directory sidecar")
        ));

        let nearest = crate::read_limits::ReadLimits {
            max_stored_frame_bytes: sidecar_len,
            ..Default::default()
        };
        super::open_fold_from_container(&container, &manifest, cfg, &path, nearest)
            .expect("the inclusive sidecar ceiling must admit the exact current fold");
        std::fs::remove_dir_all(root).ok();
    }

    /// The bug this exists to prevent: corruption that still PARSES. A shortened `fold_off` here
    /// would have become a false authority boundary and hidden durable fold bytes with no error.
    #[test]
    fn a_flipped_byte_that_still_parses_is_refused() {
        let d = std::env::temp_dir().join(format!("turndb-mancrc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let m = super::Manifest { fold_off: 4096, next_seq: 9, ..Default::default() };
        let mut b = m.encode().unwrap();
        let at = b.windows(4).position(|w| w == b"4096").expect("fold_off literal in the JSON");
        b[at] = b'1'; // now claims fold_off 1096 — valid JSON, wrong bytes
        let err = super::Manifest::parse(&b).unwrap_err().to_string();
        assert!(err.contains("checksum"), "must refuse via the checksum, got: {err}");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Damage to the trailer must never demote a manifest to unchecked JSON.
    #[test]
    fn a_damaged_trailer_is_refused() {
        let d = std::env::temp_dir().join(format!("turndb-mantrail-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut b = super::Manifest::default().encode().unwrap();
        let at = b.len() - 14; // the 'c' of the final "crc32=XXXXXXXX" line
        b[at] = b'x';
        assert!(super::Manifest::parse(&b).is_err(), "the required trailer must be exact");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn unchecked_or_pre_epoch_manifest_bytes_are_refused() {
        let old = br#"{"parts":[],"fold_seg":0,"fold_off":0,"next_seq":0}"#;
        assert!(super::Manifest::parse(old).is_err(), "bare JSON is not a current manifest");

        let current = super::Manifest { draft_epoch: 0, ..Default::default() };
        let error = super::Manifest::parse(&current.encode().unwrap()).unwrap_err().to_string();
        assert!(error.contains("draft epoch"), "{error}");

        let mut value = serde_json::to_value(super::Manifest::default()).unwrap();
        value.as_object_mut().unwrap().insert("discarded_field".into(), true.into());
        let mut unknown = serde_json::to_vec(&value).unwrap();
        let crc = crc32fast::hash(&unknown);
        unknown.extend_from_slice(format!("\ncrc32={crc:08x}").as_bytes());
        let error = format!("{:#}", super::Manifest::parse(&unknown).unwrap_err());
        assert!(
            error.contains("unknown field"),
            "a checksummed unknown field must refuse: {error}"
        );
    }
}
