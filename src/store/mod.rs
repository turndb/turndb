//! The store: WAL, memtable, flush, manifest, recovery — the layer that turns a fold and some parts
//! into a database.
//!
//! # Substrate
//!
//! A store is a **directory**, and reading one requires nothing but the files. There is no daemon in
//! this design; a server is a *role* a process takes when it holds the writer lock, not a thing the
//! format depends on. (That lock is enforced by the OS on Unix and **not enforced on
//! `wasm32-wasip1`** — there the single-writer invariant is the embedder's to maintain, since
//! there is no advisory lock to hold. See `src/sys.rs` and FORMAT.md.)
//!
//! [`open_read_container`] takes no lock, replays nothing, and is safe to run
//! concurrently with a writer — parts are immutable and the fold is append-only, so a reader pinned to
//! a manifest sees a consistent store with no coordination at all.
//!
//! # The commit point
//!
//! The manifest is the only one. It names the live parts, the fold tail, and the log cursor;
//! everything else — the block directory, the dedup index, part contents — is derived. It is written
//! tmp + fsync + rename + fsync-dir, so a crash either sees the old manifest or the new one.
//!
//! # Ordering, and why recovery is simple
//!
//! ```text
//! put    -> fold.put (no fsync)  +  WAL append
//! sync   -> WAL fsync                        <- the ACK point
//! flush  -> fold.sync -> write part -> commit manifest -> truncate WAL
//! ```
//! Recovery does not try to work out how far the fold got. It **truncates the fold to the tail the
//! manifest committed** and replays the log, which carries the bytes of every piece that was new.
//! Anything the fold wrote past that tail is discarded and regenerated, so there is no window in
//! which a part could reference content that never landed.

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
use std::io::Read;
use std::path::{Component, Path, PathBuf};
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
/// Maximum committed-manifest bytes accepted from disk or a pack under the default format reader.
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

/// Runtime writer configuration. None of these values are persisted format commitments.
#[derive(Clone, Copy, Debug)]
pub struct StoreOptions {
    pub fold: FoldCfg,
    pub write_limits: WriteLimits,
    /// Admission applied before atomic frame allocation and persistent collection growth.
    pub read_limits: ReadLimits,
    /// One decompressed-section cache budget shared by every immutable part in this handle.
    pub part_cache_bytes: usize,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            fold: FoldCfg::default(),
            write_limits: WriteLimits::default(),
            read_limits: ReadLimits::default(),
            part_cache_bytes: crate::part::cache::BUDGET_DEFAULT,
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
        add_size(&mut size, 1 + 32); // present whole-content identity
        add_size(&mut size, varint_bytes(content.spans.len() as u64));
        let mut novel = 0u64;
        for span in &content.spans {
            match span {
                Span::Lit(bytes) => {
                    add_size(&mut size, 1u64.saturating_add(bytes_field_size(bytes.len())))
                }
                Span::Piece(bytes) => {
                    read_limits.admit("new fold block", bytes.len() as u64, bytes.len() as u64)?;
                    add_size(
                        &mut size,
                        1u64.saturating_add(32).saturating_add(varint_bytes(bytes.len() as u64)),
                    );
                    add_size(&mut novel, 32u64.saturating_add(bytes_field_size(bytes.len())));
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
        .filter(|span| matches!(span, Span::Piece(_)))
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
                    read_limits.admit("new fold block", bytes.len() as u64, bytes.len() as u64)?;
                    u32::try_from(bytes.len())
                        .context("one folded piece exceeds the format's u32 length")?;
                    piece_count = piece_count.saturating_add(1);
                    add_size(
                        &mut size,
                        1u64.saturating_add(32).saturating_add(varint_bytes(bytes.len() as u64)),
                    );
                    add_size(&mut novel, 32u64.saturating_add(bytes_field_size(bytes.len())));
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
pub struct PartRef {
    pub file: String,
    pub seq_lo: u64,
    pub seq_hi: u64,
    pub records: u32,
    /// BLAKE3 of the part file's bytes, hex — the manifest PINNING the part. Content is pinned
    /// transitively from here: this digest covers `pdict.hash`, which carries per-piece BLAKE3,
    /// so a fold that drifted from what a part expects is detectable without any segment-level
    /// digest. Absent in manifests written before the chain existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b3: Option<String>,
}

/// How many committed manifests are RETAINED beside the live one, as `MANIFEST.<commit>`.
///
/// Retention is what turns the commit point into a log: every file a retained manifest names
/// survives the sweep, so a reader holding any manifest in the window sees its whole snapshot on
/// disk, and a corrupt `MANIFEST` is recoverable by explicit promotion instead of surgery. The
/// window is a count of COMMITS, not time — each flush, merge, or re-fold advances it by one.
pub const MANIFEST_RETAIN: usize = 4;

/// The committed state of the store. Small, atomic, and the only source of truth about what is live.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub parts: Vec<PartRef>,
    /// Which fold generation is live. A re-fold writes a new one and names it here, so the swap IS the
    /// manifest commit. Absent in stores written before re-folding existed, which serde reads as 0 —
    /// the original `fold/` directory, needing no migration.
    #[serde(default)]
    pub fold_gen: u32,
    pub fold_seg: u32,
    pub fold_off: u32,
    pub next_seq: u64,
    /// Monotonic commit counter — the retained log's namespace. `next_seq` cannot serve here: it
    /// only advances at flush, and merges and re-folds commit without flushing. Absent in stores
    /// written before the log existed, which serde reads as 0.
    #[serde(default)]
    pub commit: u64,
    /// Block ids whose bytes were PUNCHED out of the fold, as inclusive `[lo, hi]` ranges (erasure
    /// tends to hit runs of blocks, and ranges keep the manifest small). Authoritative, and that
    /// is the point: a punched block reads back as zeros, which is indistinguishable from
    /// corruption unless something says otherwise. This says otherwise.
    ///
    /// Ranges are ascending and disjoint. Absent in manifests written before punching existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub punched: Vec<(u32, u32)>,
    /// BLAKE3 of the PREVIOUS manifest's exact bytes, hex — the commit log as a hash chain, at
    /// zero marginal cost. Absent on a store's first commit and in manifests written before the
    /// chain existed.
    ///
    /// This is an INTEGRITY check, not a security claim: it catches a manifest that was replaced,
    /// reordered, or restored out of band, which section checksums cannot see because each one is
    /// individually valid. Pruned manifests take their bytes with them, so the chain is verifiable
    /// across the retained window and says nothing about what is no longer there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<String>,
}

fn safe_part_file_name(name: &str) -> bool {
    if name.is_empty() || name.contains('\\') {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn valid_blake3_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Read the one small authoritative file through a hard allocation boundary. Metadata is checked
/// first for the common/sparse-file case and `take(max + 1)` closes a concurrent-growth race.
pub(crate) fn read_manifest_file(path: &Path) -> Result<Vec<u8>> {
    let file = crate::vfs::open_read(path)?;
    let announced = file.metadata()?.len();
    if announced > MAX_MANIFEST_BYTES {
        bail!(
            "MANIFEST at {} is {announced} bytes, exceeding the supported {MAX_MANIFEST_BYTES}-byte limit",
            path.display()
        );
    }
    let capacity =
        usize::try_from(announced).context("MANIFEST length does not fit this platform")?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).context("reserve MANIFEST buffer")?;
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        bail!(
            "MANIFEST at {} grew past the supported {MAX_MANIFEST_BYTES}-byte limit while reading",
            path.display()
        );
    }
    Ok(bytes)
}

impl Manifest {
    /// A MISSING manifest is a new store. An UNREADABLE one is an error.
    ///
    /// These were conflated, and the orphan sweep made the conflation destructive: a transient EACCES
    /// or EIO yielded an empty manifest, and the sweep then unlinked every part it did not name. One
    /// unreadable byte turned a live store into an empty directory.
    #[cfg(test)]
    fn load(dir: &Path) -> Result<Manifest> {
        Self::load_with_limits(dir, ReadLimits::default())
    }

    fn load_with_limits(dir: &Path, read_limits: ReadLimits) -> Result<Manifest> {
        let path = dir.join("MANIFEST");
        match read_manifest_file(&path) {
            Ok(b) => Manifest::parse(&b),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                // A missing manifest is a new store — UNLESS a commit log exists, in which case
                // this store has committed before and `MANIFEST` was lost. Opening it as new
                // would be the destructive conflation all over again, one deletion further
                // upstream: an empty manifest followed by the sweep.
                let retained = list_retained_with_limits(dir, read_limits)?;
                if retained.is_empty() {
                    Ok(Manifest::default())
                } else {
                    bail!(
                        "MANIFEST is missing but {} retained commits exist at {} — a damaged \
                         store, not a new one; recover_manifest() can promote the newest intact copy",
                        retained.len(),
                        dir.display()
                    )
                }
            }
            Err(e) => Err(e.context(format!(
                "cannot read {} — refusing to treat an unreadable manifest as an empty store",
                path.display()
            ))),
        }
    }

    /// Parse manifest bytes, verifying the checksum trailer when one is present.
    ///
    /// The manifest is the one file whose corruption used to be able to DESTROY data with no error
    /// anywhere: it is parsed JSON, so a flipped bit that still parses — a shortened `fold_off`, a
    /// wrong generation — was believed, and recovery then truncated durable fold bytes to match it.
    /// Every other structure in the store refuses corruption; this closes the last gap.
    ///
    /// A manifest written before the trailer existed is bare compact JSON and is accepted as-is:
    /// the trailer is recognised by SHAPE (a final line `crc32=XXXXXXXX`), which compact JSON cannot
    /// end with. Corruption cannot demote a checksummed manifest to a legacy one either way it
    /// lands: mangling the trailer leaves trailing bytes that JSON parsing refuses, and mangling
    /// the payload fails the checksum.
    fn parse(bytes: &[u8]) -> Result<Manifest> {
        let payload = match checksum_trailer(bytes) {
            Some((payload, want)) => {
                let got = crc32fast::hash(payload);
                if got != want {
                    bail!(
                        "MANIFEST fails its checksum (crc32 {got:08x}, recorded {want:08x}) — \
                         refusing to open from a corrupt commit point"
                    );
                }
                payload
            }
            None => bytes,
        };
        if payload.len() as u64 > MAX_MANIFEST_BYTES {
            bail!(
                "MANIFEST is {} bytes, exceeding the supported {MAX_MANIFEST_BYTES}-byte limit",
                payload.len()
            );
        }
        let manifest: Manifest = serde_json::from_slice(payload).context("corrupt MANIFEST")?;
        manifest.validate()
    }

    /// Validate semantic fields before any one of them becomes a filesystem path or allocation
    /// input. JSON syntax and a checksum prove faithful bytes, not safe meaning.
    fn validate(self) -> Result<Manifest> {
        let mut files = HashSet::with_capacity(self.parts.len());
        for part in &self.parts {
            if !safe_part_file_name(&part.file) {
                bail!("MANIFEST part file {:?} is not one store-local path component", part.file);
            }
            if !files.insert(part.file.as_str()) {
                bail!("MANIFEST names part file {:?} more than once", part.file);
            }
            if part.seq_lo > part.seq_hi {
                bail!(
                    "MANIFEST part {:?} has inverted sequence range {}..{}",
                    part.file,
                    part.seq_lo,
                    part.seq_hi
                );
            }
            if part.b3.as_deref().is_some_and(|digest| !valid_blake3_hex(digest)) {
                bail!("MANIFEST part {:?} carries an invalid BLAKE3 digest", part.file);
            }
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

    /// tmp + fsync + rename + fsync-dir: a crash sees either the old manifest or the new one.
    ///
    /// Bumps the commit counter, and writes the retained copy `MANIFEST.<commit>` BEFORE the
    /// rename: if the live manifest is later corrupted, the copy of the very state it carried is
    /// what recovery promotes. A crash between the copy and the rename leaves a retained manifest
    /// describing a commit that never took effect. That residue is NOT the old state plus a
    /// counter bump — the caller mutates the manifest before committing, so the copy names files
    /// (a new part, a moved fold tail) the live manifest does not. Data-before-pointers makes
    /// those files durable, but the commit's authority is the rename that never happened:
    /// whatever the crashed commit acknowledged is still in the WAL, and writer open durably
    /// removes the residue before replaying it, precisely so the counters this manifest restarts
    /// cannot re-create a file name the residue still claims a digest for.
    ///
    /// One directory fsync at the end covers both dirents. Pruning runs last and is best-effort —
    /// a retained manifest that outlives its window is swept space, never a correctness problem,
    /// because it is OLDER than live: everything it pins, the live chain pins or has aged out.
    #[cfg(test)]
    fn commit(&mut self, dir: &Path) -> Result<()> {
        self.commit_with_limits(dir, ReadLimits::default())
    }

    fn commit_with_limits(&mut self, dir: &Path, read_limits: ReadLimits) -> Result<()> {
        // Admission precedes the first mutation. A commit creates one retained name and one
        // temporary name before the rename, so reserve both against the current directory count.
        let retained_before = list_retained_with_limits(dir, read_limits)?;
        let directory_entries = count_directory_entries(dir, read_limits, "store directory")?;
        read_limits.admit_directory_entries(
            "store directory during manifest commit",
            directory_entries.saturating_add(2),
        )?;
        self.commit += 1;
        // Chain onto whatever is being replaced. Hashed from disk rather than from memory,
        // because the chain's claim is about the BYTES a verifier can read back.
        self.prev = read_manifest_file(&dir.join("MANIFEST"))
            .ok()
            .map(|b| blake3::hash(&b).to_hex().to_string());
        let bytes = self.encode()?;
        {
            let p = retained_path(dir, self.commit);
            let f = crate::vfs::create(&p)?;
            crate::vfs::write_all_at(&f, &p, &bytes, 0)?;
            crate::vfs::sync_file(&f, &p)?;
        }
        // The retained copy's NAME is made durable before the live name is touched. On Windows
        // the replace-rename below can leave neither the old nor the new `MANIFEST` after a crash
        // (tests/dst.rs, `rename-neither`), and the copy published here is then the manifest a
        // recovery promotes. One directory fsync per commit on POSIX, in a layout that is
        // convert-only.
        crate::vfs::sync_dir(dir)?;
        let tmp = dir.join("MANIFEST.tmp");
        let f = crate::vfs::create(&tmp)?;
        crate::vfs::write_all_at(&f, &tmp, &bytes, 0)?;
        crate::vfs::sync_file(&f, &tmp)?;
        drop(f);
        crate::vfs::rename(&tmp, &dir.join("MANIFEST"))?;
        crate::vfs::sync_dir(dir)?;
        for c in retained_before {
            if c + (MANIFEST_RETAIN as u64) <= self.commit {
                let _ = crate::vfs::unlink(&retained_path(dir, c));
            }
        }
        Ok(())
    }

    /// [`Manifest::commit_with_limits`]'s single-file form: stage this commit's members. The
    /// caller's superblock flip — not this — is the linearization point, so everything a flush
    /// staged (fold extents, the part, these manifests, the sweep's frees) publishes as ONE
    /// atomic state. The directory protocol's five steps — retained copy, tmp, fsync, rename,
    /// dir fsync — collapse into two staged members, and the prune that was best-effort-after
    /// becomes part of the same commit it belongs to.
    fn commit_into_container(&mut self, c: &mut crate::container::Container) -> Result<()> {
        self.commit += 1;
        // Chain onto whatever is being replaced — from the member's bytes, because the chain's
        // claim is about what a verifier can read back.
        self.prev = c
            .read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)
            .ok()
            .map(|b| blake3::hash(&b).to_hex().to_string());
        let bytes = self.encode()?;
        c.put_bytes(&format!("MANIFEST.{:08}", self.commit), &bytes)?;
        c.put_bytes("MANIFEST", &bytes)?;
        for commit in container_retained_commits(c) {
            if commit + (MANIFEST_RETAIN as u64) <= self.commit {
                let _ = c.remove(&format!("MANIFEST.{commit:08}"));
            }
        }
        Ok(())
    }

    fn fold_tail(&self) -> Option<FoldTail> {
        if self.next_seq == 0 && self.parts.is_empty() && self.fold_off == 0 {
            None
        } else {
            Some(FoldTail { seg: self.fold_seg, off: self.fold_off })
        }
    }
}

/// The `(payload, recorded crc32)` of a checksummed manifest, or `None` for one written before the
/// trailer existed. Recognition is by exact shape — a final line `crc32=` plus eight hex digits —
/// so a legacy manifest (compact JSON, which cannot end that way) is never misread as checksummed,
/// and a checksummed one whose trailer is damaged falls through to JSON parsing, which refuses the
/// trailing bytes rather than silently accepting the payload unverified.
fn checksum_trailer(bytes: &[u8]) -> Option<(&[u8], u32)> {
    let pos = bytes.iter().rposition(|&b| b == b'\n')?;
    let tail = &bytes[pos + 1..];
    if tail.len() != 14 || !tail.starts_with(b"crc32=") {
        return None;
    }
    let hex = std::str::from_utf8(&tail[6..]).ok()?;
    let want = u32::from_str_radix(hex, 16).ok()?;
    Some((&bytes[..pos], want))
}

fn retained_path(dir: &Path, commit: u64) -> PathBuf {
    dir.join(format!("MANIFEST.{commit:08}"))
}

fn list_retained_with_limits(dir: &Path, read_limits: ReadLimits) -> Result<Vec<u64>> {
    let read_limits = read_limits.validate()?;
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir).with_context(|| {
        format!("read store directory {} for retained manifests", dir.display())
    })?;
    let mut visited = 0u64;
    for e in rd {
        visited = visited.saturating_add(1);
        read_limits.admit_directory_entries("store directory", visited)?;
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_prefix("MANIFEST.") {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(n) = rest.parse::<u64>() {
                    out.push(n);
                }
            }
        }
    }
    out.sort_unstable();
    Ok(out)
}

fn count_directory_entries(
    dir: &Path,
    read_limits: ReadLimits,
    label: &'static str,
) -> Result<u64> {
    let mut visited = 0u64;
    for entry in std::fs::read_dir(dir)? {
        visited = visited.saturating_add(1);
        read_limits.admit_directory_entries(label, visited)?;
        entry?;
    }
    Ok(visited)
}

/// Open a READER over a pack — the store in one file, served through bounded extents.
///
/// Everything [`ReadStore`] can do over a directory it does here identically: same manifest, same
/// parts, same fold, same version resolution. There is no writer role to take — a pack is
/// immutable by definition — and no retry loop to need, because nothing can sweep files out from
/// under an open handle on an immutable artifact.
pub fn open_read_pack(path: &Path, cfg: FoldCfg) -> Result<ReadStore> {
    open_read_pack_with_limits(path, cfg, ReadLimits::default())
}

/// Open a packed reader with explicit frame and persistent object-count admission.
pub fn open_read_pack_with_limits(
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    let read_limits = read_limits.validate()?;
    let pack = crate::pack::Pack::open(path)?;
    let manifest = Manifest::parse(&pack.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?;

    let fold_rel = if manifest.fold_gen == 0 {
        "fold".to_string()
    } else {
        format!("fold-{:04}", manifest.fold_gen)
    };
    let mut segs = Vec::new();
    let mut dict_files = Vec::new();
    for name in pack.names().map(String::from).collect::<Vec<_>>() {
        let Some(rest) = name.strip_prefix(&format!("{fold_rel}/")) else { continue };
        if let Some(n) = crate::fold::segment::parse_seg_name(rest) {
            let extent = pack.file(&name).expect("name came from this pack's TOC");
            let segment_len = crate::readat::ReadAt::len(&extent)?;
            segs.push(crate::fold::SegmentInput {
                seg: n,
                reader: Arc::new(extent) as Arc<dyn crate::readat::ReadAt>,
                // Advisory: an overlarge sidecar is ignored and the segment is scanned instead.
                sidecar: pack
                    .read_file_bounded(
                        &format!("{fold_rel}/seg-{n:08}.dir"),
                        crate::fold::segment::max_dir_sidecar_bytes(segment_len)
                            .min(read_limits.max_stored_frame_bytes),
                    )
                    .ok(),
            });
        } else if rest.starts_with("zdict-") && rest.ends_with(".zd") {
            dict_files.push(pack.read_file_bounded(&name, crate::fold::MAX_DICTIONARY_BYTES)?);
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

    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    for p in &manifest.parts {
        let ext = pack.file(&p.file).ok_or_else(|| {
            anyhow::anyhow!("pack manifest names {} but the pack does not hold it", p.file)
        })?;
        parts.push(Arc::new(Part::open_reader_with_limits(
            Box::new(ext),
            pcache.clone(),
            read_limits,
        )?));
    }
    Ok(ReadStore { fold: Arc::new(fold), parts, manifest, read_limits })
}

/// Open a READER over a container — the store in one **mutable** file.
///
/// Identical in every observable way to opening the directory it was checkpointed from: same
/// manifest, same parts, same fold, same version resolution. The difference from a pack is only
/// that the file this reads can still grow — a container names its state in a superblock rather
/// than a footer at EOF, so appending does not invalidate the state a reader already resolved.
pub fn open_read_container(path: &Path, cfg: FoldCfg) -> Result<ReadStore> {
    open_read_container_with_limits(path, cfg, ReadLimits::default())
}

/// Open a container reader with explicit frame and persistent object-count admission.
pub fn open_read_container_with_limits(
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    if !path.exists() && crate::container::reclaim_names(path).anchor.exists() {
        bail!(
            "{} is absent but its reclaim anchor {} exists: a reclaim's replace was interrupted; \
             a writer open recovers the store from the anchor (readers never mutate)",
            path.display(),
            crate::container::reclaim_names(path).anchor.display()
        );
    }
    let read_limits = read_limits.validate()?;
    let container = crate::container::Container::open(path)?;
    // Absent means a store nothing has flushed yet — the directory reader's rule, with the same
    // tripwire: retained commits beside a missing manifest are damage, not emptiness.
    let manifest = if container.contains("MANIFEST") {
        Manifest::parse(&container.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?
    } else if container_retained_commits(&container).is_empty() {
        Manifest::default()
    } else {
        bail!(
            "MANIFEST is missing but retained commits exist in {} — a damaged store, not a new \
             one",
            path.display()
        );
    };

    let fold_rel = if manifest.fold_gen == 0 {
        "fold".to_string()
    } else {
        format!("fold-{:04}", manifest.fold_gen)
    };
    let mut segs = Vec::new();
    let mut dict_files = Vec::new();
    for name in container.names().map(String::from).collect::<Vec<_>>() {
        let Some(rest) = name.strip_prefix(&format!("{fold_rel}/")) else { continue };
        if let Some(n) = crate::fold::segment::parse_seg_name(rest) {
            let extent = container.extent(&name).expect("name came from this directory");
            let segment_len = crate::readat::ReadAt::len(&extent)?;
            segs.push(crate::fold::SegmentInput {
                seg: n,
                reader: Arc::new(extent) as Arc<dyn crate::readat::ReadAt>,
                sidecar: container
                    .read_file_bounded(
                        &format!("{fold_rel}/seg-{n:08}.dir"),
                        crate::fold::segment::max_dir_sidecar_bytes(segment_len)
                            .min(read_limits.max_stored_frame_bytes),
                    )
                    .ok(),
            });
        } else if rest.starts_with("zdict-") && rest.ends_with(".zd") {
            dict_files.push(container.read_file_bounded(&name, crate::fold::MAX_DICTIONARY_BYTES)?);
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

    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    for p in &manifest.parts {
        let ext = container.extent(&p.file).ok_or_else(|| {
            anyhow::anyhow!(
                "container manifest names {} but the container does not hold it",
                p.file
            )
        })?;
        parts.push(Arc::new(Part::open_reader_with_limits(
            Box::new(ext),
            pcache.clone(),
            read_limits,
        )?));
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
    let container = crate::container::ContainerReader::open(source, label)?;
    if container.len() as u64 > read_limits.max_directory_entries {
        bail!(
            "container {label} holds {} directory entries, over the configured limit {}",
            container.len(),
            read_limits.max_directory_entries
        );
    }
    let manifest = if container.contains("MANIFEST") {
        Manifest::parse(&container.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?
    } else if !container.names().any(|name| {
        name.strip_prefix("MANIFEST.")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
    }) {
        Manifest::default()
    } else {
        bail!(
            "MANIFEST is missing but retained commits exist in {label} — a damaged store, not a new one"
        )
    };
    let fold_rel = if manifest.fold_gen == 0 {
        "fold".to_string()
    } else {
        format!("fold-{:04}", manifest.fold_gen)
    };
    let mut segs = Vec::new();
    let mut dict_files = Vec::new();
    for name in container.names().map(String::from).collect::<Vec<_>>() {
        let Some(rest) = name.strip_prefix(&format!("{fold_rel}/")) else { continue };
        if let Some(number) = crate::fold::segment::parse_seg_name(rest) {
            let extent = container.extent(&name).expect("name came from this directory");
            let segment_len = crate::readat::ReadAt::len(&extent)?;
            segs.push(crate::fold::SegmentInput {
                seg: number,
                reader: Arc::new(extent) as Arc<dyn crate::readat::ReadAt>,
                sidecar: container
                    .read_file_bounded(
                        &format!("{fold_rel}/seg-{number:08}.dir"),
                        crate::fold::segment::max_dir_sidecar_bytes(segment_len)
                            .min(read_limits.max_stored_frame_bytes),
                    )
                    .ok(),
            });
        } else if rest.starts_with("zdict-") && rest.ends_with(".zd") {
            dict_files.push(container.read_file_bounded(&name, crate::fold::MAX_DICTIONARY_BYTES)?);
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
        let extent = container.extent(&part.file).ok_or_else(|| {
            anyhow::anyhow!(
                "container manifest names {} but the container does not hold it",
                part.file
            )
        })?;
        parts.push(Arc::new(Part::open_reader_with_limits(
            Box::new(extent),
            pcache.clone(),
            read_limits,
        )?));
    }
    Ok(ReadStore { fold: Arc::new(fold), parts, manifest, read_limits })
}

/// Open a retained snapshot of a single-file store: the store exactly as commit `commit` left it.
///
/// The retained manifest names the state; `punched` comes from the LIVE manifest, exactly as the
/// directory form insists, because erasure is declared by the live commit and a retained copy
/// predates every punch that followed it. Fold readers are bounded to the snapshot's exact tail,
/// so nothing this handle serves can wander into bytes its commit never named.
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
    let c = crate::container::Container::open(path)?;
    let bytes = c
        .read_file_bounded(&format!("MANIFEST.{commit:08}"), MAX_MANIFEST_BYTES)
        .with_context(|| format!("retained commit {commit} is not held by {}", path.display()))?;
    let mut manifest = Manifest::parse(&bytes)?;
    let live = Manifest::parse(&c.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)
        .with_context(|| {
            format!(
                "retained snapshot at commit {commit} needs the live manifest in {} to tell \
                 erased blocks from damaged ones, and it could not be read",
                path.display()
            )
        })?;
    if live.fold_gen != manifest.fold_gen {
        bail!(
            "retained commit {commit} is from fold generation {} but the live store is at {} — \
             a re-fold purges the retained log, so this snapshot has no erasure authority",
            manifest.fold_gen,
            live.fold_gen
        );
    }
    manifest.punched = live.punched;

    let prefix = crate::fold::fold_member_prefix(manifest.fold_gen);
    let tail = manifest.fold_tail();
    let mut segs = Vec::new();
    let mut dict_files = Vec::new();
    for name in c.names().map(String::from).collect::<Vec<_>>() {
        let Some(rest) = name.strip_prefix(&format!("{prefix}/")) else { continue };
        if let Some(n) = crate::fold::segment::parse_seg_name(rest) {
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
                    c.read_file_bounded(
                        &format!("{prefix}/seg-{n:08}.dir"),
                        crate::fold::segment::max_dir_sidecar_bytes(len)
                            .min(read_limits.max_stored_frame_bytes),
                    )
                    .ok()
                } else {
                    None
                },
            });
        } else if rest.starts_with("zdict-") && rest.ends_with(".zd") {
            dict_files.push(c.read_file_bounded(&name, crate::fold::MAX_DICTIONARY_BYTES)?);
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
    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    for p in &manifest.parts {
        let ext = c.extent(&p.file).ok_or_else(|| {
            anyhow::anyhow!(
                "retained commit {commit} names {} but the container does not hold it",
                p.file
            )
        })?;
        parts.push(Arc::new(Part::open_reader_with_limits(
            Box::new(ext),
            pcache.clone(),
            read_limits,
        )?));
    }
    Ok(ReadStore { fold: Arc::new(fold), parts, manifest, read_limits })
}

/// Which single-file form a path holds.
///
/// Told apart by magic rather than by extension, because the extension is the user's to choose and
/// the magic is the format's. Both forms answer reads identically; they differ in whether the file
/// can still grow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SingleFileKind {
    /// Sealed: footer at EOF, immutable, writer-less by definition.
    Pack,
    /// Growable: alternating superblocks at the head, appended beyond the committed tail.
    Container,
}

/// Classify a single-file store by its magic.
///
/// The two forms carry their magic in different places, which is the addressing difference that
/// separates them: a container is superblock-addressed, so its magic is the first eight bytes; a
/// pack is footer-addressed, so its magic begins the footer at EOF. Checking only one position
/// finds only one form.
///
/// A path that is not a regular file, or that carries neither magic, is reported as `None` rather
/// than guessed at — callers that also accept directories dispatch on that.
pub fn single_file_kind(path: &Path) -> Option<SingleFileKind> {
    if !path.is_file() {
        return None;
    }
    let f = crate::vfs::open_read(path).ok()?;
    let len = f.metadata().ok()?.len();

    let mut magic = [0u8; 8];
    if len >= crate::container::REGION_START
        && crate::sys::read_exact_at(&f, &mut magic, 0).is_ok()
        && &magic == crate::container::MAGIC
    {
        return Some(SingleFileKind::Container);
    }
    if len >= crate::pack::FOOTER_LEN
        && crate::sys::read_exact_at(&f, &mut magic, len - crate::pack::FOOTER_LEN).is_ok()
        && &magic == crate::pack::MAGIC
    {
        return Some(SingleFileKind::Pack);
    }
    None
}

/// Open a READER over whichever single-file form the path holds.
///
/// The one entry a consumer needs when it has a `.turndb` and does not care which form produced
/// it. A directory is refused here — the directory layout is retired, and `convert` is its door.
pub fn open_read_file(path: &Path, cfg: FoldCfg) -> Result<ReadStore> {
    open_read_file_with_limits(path, cfg, ReadLimits::default())
}

/// Open a single-file reader with explicit frame and persistent object-count admission.
pub fn open_read_file_with_limits(
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
) -> Result<ReadStore> {
    match single_file_kind(path) {
        Some(SingleFileKind::Container) => open_read_container_with_limits(path, cfg, read_limits),
        Some(SingleFileKind::Pack) => open_read_pack_with_limits(path, cfg, read_limits),
        None => bail!("{} is not a turndb pack or container (no recognised magic)", path.display()),
    }
}

/// Checkpoint a store directory into a container, creating it or growing one that exists.
///
/// This is the directory→file transition, and it is incremental by construction: parts and rolled
/// fold segments are immutable and uniquely named, so a member already present under the same name
/// and length is skipped rather than rewritten. Only `MANIFEST` and the live segment are restaged
/// every time.
///
/// The source must be quiescent — a non-empty WAL means writes exist that no manifest names yet.
/// [`convert_to_file`] is the only caller and settles the source first; the directory layout has
/// no other door left.
fn checkpoint_into_container(dir: &Path, out: &Path) -> Result<u64> {
    let wal = dir.join("WAL");
    if wal.metadata().map(|m| m.len()).unwrap_or(0) != 0 {
        bail!("checkpoint refuses a store with a non-empty WAL: settle it with sync and flush");
    }
    let manifest = Manifest::load_with_limits(dir, ReadLimits::default())?;
    let mut container = if out.exists() {
        crate::container::Container::open(out)?
    } else {
        crate::container::Container::create(out)?
    };

    let mut names: Vec<String> = vec!["MANIFEST".to_string()];
    for p in &manifest.parts {
        names.push(p.file.clone());
    }
    let fold_rel = if manifest.fold_gen == 0 {
        "fold".to_string()
    } else {
        format!("fold-{:04}", manifest.fold_gen)
    };
    let fold_path = refold::fold_dir(dir, manifest.fold_gen);
    let mut fold_names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&fold_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let keep = name.ends_with(".fold")
                || name.ends_with(".dir")
                || (name.starts_with("zdict-") && name.ends_with(".zd"));
            if keep {
                fold_names.push(format!("{fold_rel}/{name}"));
            }
        }
    }
    fold_names.sort();
    names.extend(fold_names);

    for name in &names {
        let src = dir.join(name);
        // A store that never applies a record never commits, and a directory store announces
        // itself as new precisely by having no MANIFEST — `Manifest::load_with_limits` is explicit
        // about that. A container has no equivalent affordance: its members ARE its state, so one
        // holding no MANIFEST names no store and every later command refuses it. Writing the
        // manifest the loader just handed us keeps the empty store openable without teaching the
        // container reader a second way to be empty, and without committing behind the back of a
        // Store this checkpoint may not own — `checkpoint()` runs with the writer still open.
        if name == "MANIFEST" && !src.exists() {
            container.put_bytes(name, &manifest.encode()?)?;
            continue;
        }
        // A member the working directory does not hold is one the session never copied out —
        // the container is already its only home and there is nothing to move. Deciding this on
        // presence rather than on a length comparison matters: `metadata` on a missing file yields
        // zero, which reads as "a different length" and sends an absent member to `ingest`, which
        // then fails on a file that was never supposed to exist.
        if !src.exists() {
            if container.contains(name) {
                continue;
            }
            bail!("MANIFEST names {name} but neither {} nor the container holds it", dir.display());
        }
        let src_len = std::fs::metadata(&src)?.len();
        // MANIFEST and the live segment change in place; an immutable member of the same length is
        // already the same bytes, because parts and rolled segments are never rewritten.
        let immutable = name != "MANIFEST" && !name.ends_with(".dir");
        if immutable {
            if let Some(existing) = container.extent(name) {
                if crate::readat::ReadAt::len(&existing)? == src_len {
                    continue;
                }
            }
        }
        container.ingest(name, &src)?;
    }
    container.commit()
}

fn load_retained(dir: &Path, commit: u64) -> Result<Manifest> {
    let p = retained_path(dir, commit);
    let b = read_manifest_file(&p).with_context(|| {
        format!("no retained manifest {} — the retention window has moved past it", p.display())
    })?;
    Manifest::parse(&b).with_context(|| format!("retained manifest {} is corrupt", p.display()))
}

/// Delete every file that no manifest — live or retained — names. THE deletion path: flush, merge,
/// re-fold, and writer open all converge here, so there is exactly one place that decides
/// reachability. A file a retained manifest names is a live snapshot's file and survives; it is
/// swept only when the window prunes past its last naming manifest.
///
/// A retained manifest that fails its checksum (a torn copy from a crash) pins nothing — it can
/// describe no snapshot anyone can open.
fn sweep_unreachable_with_limits(dir: &Path, read_limits: ReadLimits) -> Result<()> {
    let mut keep: Vec<Manifest> = vec![Manifest::load_with_limits(dir, read_limits)?];
    for c in list_retained_with_limits(dir, read_limits)? {
        if let Ok(m) = load_retained(dir, c) {
            keep.push(m);
        }
    }
    let live_parts: HashSet<&str> =
        keep.iter().flat_map(|m| m.parts.iter().map(|p| p.file.as_str())).collect();
    let live_gens: HashSet<u32> = keep.iter().map(|m| m.fold_gen).collect();
    let rd = std::fs::read_dir(dir)
        .with_context(|| format!("read store directory {} for orphan cleanup", dir.display()))?;
    let mut visited = 0u64;
    for e in rd {
        visited = visited.saturating_add(1);
        read_limits.admit_directory_entries("store directory", visited)?;
        let e = e?;
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with("part-") && n.ends_with(".part") && !live_parts.contains(n.as_str()) {
            let _ = crate::vfs::unlink(&e.path());
        }
        if e.path().is_dir() {
            if let Some(g) = refold::parse_fold_gen(&n) {
                if !live_gens.contains(&g) {
                    let _ = crate::vfs::remove_tree(&e.path());
                }
            }
        }
    }
    Ok(())
}

fn cleanup_refold_stage(dir: &Path, generation: u32, built: &[refold::RefoldedPart]) {
    let fold = refold::fold_dir(dir, generation);
    if fold.exists() {
        let _ = crate::vfs::remove_tree(&fold);
    }
    for (file, ..) in built {
        let path = dir.join(file);
        if path.exists() {
            let _ = crate::vfs::unlink(&path);
        }
    }
}

/// What [`verify_chain_file`] checked.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChainReport {
    /// Retained manifest files parsed and checked. Zero is an explicit empty/new-store result.
    pub retained_manifests: usize,
    /// prev-links verified across the retained window (newest retained == live MANIFEST included).
    pub links: usize,
    /// part digests verified against their files, across every retained manifest.
    pub part_digests: usize,
    /// parts whose manifest entry predates digests — reported, because "verified" must never
    /// silently include "had nothing to verify".
    pub undigested: usize,
}

/// Evidence returned by a complete committed-store verification pass.
#[derive(Clone, Copy, Debug, Default)]
pub struct StoreVerification {
    pub chain: ChainReport,
    pub fold: crate::fold::FoldScrub,
    pub parts: usize,
    pub part_sections: usize,
    /// Distinct live records in the committed snapshot, reconstructed below.
    pub records: usize,
    /// Named content values reconstructed byte-exactly.
    pub content_values: usize,
    /// Exact bytes reconstructed across all named content values.
    pub content_bytes: u64,
    /// Reconstructed values whose stored whole-value BLAKE3 identity was checked.
    pub content_identities: usize,
    /// Legacy values carrying no whole-value identity. Reconstruction still checks every piece.
    pub unidentified_content_values: usize,
}

/// The retained-chain walk for a directory session's own verify. The layout is retired as a
/// public surface; a converter-opened session still answers `verify` honestly while it lives.
fn verify_chain_dir(
    dir: &Path,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<ChainReport> {
    let mut report = ChainReport::default();
    let commits = list_retained_with_limits(dir, read_limits)?;
    report.retained_manifests = commits.len();
    let mut prev_bytes: Option<Vec<u8>> = None;
    for &c in &commits {
        control.check("manifest verification")?;
        let bytes = read_manifest_file(&retained_path(dir, c))?;
        let m =
            Manifest::parse(&bytes).with_context(|| format!("retained manifest {c} is corrupt"))?;
        if let (Some(want), Some(pb)) = (&m.prev, &prev_bytes) {
            let got = blake3::hash(pb).to_hex().to_string();
            if *want != got {
                bail!("manifest chain broken: commit {c} names prev {want} but commit {} hashes to {got}", c - 1);
            }
            report.links += 1;
        }
        for p in &m.parts {
            control.check("manifest verification")?;
            match &p.b3 {
                Some(want) => {
                    let got = hash_file_with_control(
                        &dir.join(&p.file),
                        control,
                        "manifest verification",
                    )
                    .with_context(|| format!("part {} named by commit {c}", p.file))?
                    .to_hex()
                    .to_string();
                    if *want != got {
                        bail!("part {} drifted from the digest commit {c} pinned", p.file);
                    }
                    report.part_digests += 1;
                }
                None => report.undigested += 1,
            }
        }
        prev_bytes = Some(bytes);
    }
    // The live MANIFEST must be byte-identical to its retained copy — same commit, same bytes.
    if let (Some(&newest), Some(pb)) = (commits.last(), &prev_bytes) {
        let live = read_manifest_file(&dir.join("MANIFEST"))?;
        if live != *pb {
            bail!("MANIFEST diverges from its retained copy at commit {newest}");
        }
        report.links += 1;
    }
    Ok(report)
}

/// Copy a single-file store's committed snapshot into a fresh, SEALED container at `out`.
///
/// What a pack carries, in the pack's spirit: `MANIFEST`, every part it names, and the live
/// generation's members — no retained log (snapshots of an immutable artifact are meaningless),
/// no writer state. Staged beside the destination, committed sealed, verified member by member,
/// then published with a rename that refuses to replace. A crash leaves staging litter and an
/// untouched destination.
fn seal_container_copy(
    container: &std::sync::Arc<std::sync::Mutex<crate::container::Container>>,
    manifest: &Manifest,
    out: &Path,
    control: &crate::control::OperationControl,
) -> Result<crate::pack::BackupStats> {
    let mut staging = out.as_os_str().to_os_string();
    staging.push(".sealing");
    let staging = PathBuf::from(staging);
    let _ = crate::vfs::unlink(&staging);
    let mut fresh = crate::container::Container::create(&staging)?;

    let c = container.lock().expect("container lock poisoned");
    let prefix = crate::fold::fold_member_prefix(manifest.fold_gen);
    let mut names: Vec<String> = vec!["MANIFEST".to_string()];
    names.extend(manifest.parts.iter().map(|p| p.file.clone()));
    let mut fold_members: Vec<String> =
        c.names().filter(|n| n.starts_with(&format!("{prefix}/"))).map(String::from).collect();
    fold_members.sort();
    names.extend(fold_members);

    let mut bytes = 0u64;
    for name in &names {
        control.check("backup")?;
        let reader = c.extent(name).ok_or_else(|| {
            anyhow::anyhow!("the committed snapshot names {name} but the container lost it")
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
    fresh.commit_sealed()?;
    fresh.verify()?;
    drop(fresh);
    crate::vfs::rename_noreplace(&staging, out)?;
    if let Some(parent) = out.parent() {
        // The published name is the result; a failed directory sync means it may not survive a
        // crash, and the operation reports that rather than success.
        crate::vfs::sync_dir(parent).with_context(|| {
            format!("sync {} after publishing {}", parent.display(), out.display())
        })?;
    }
    Ok(crate::pack::BackupStats { files: names.len(), bytes, commit: manifest.commit })
}

/// [`verify_chain`] over a single-file store's members: the same walk, the same claims, with
/// files replaced by members and no filesystem between the evidence and the check.
fn verify_chain_container(
    c: &crate::container::Container,
    control: &crate::control::OperationControl,
) -> Result<ChainReport> {
    let mut report = ChainReport::default();
    let commits = container_retained_commits(c);
    report.retained_manifests = commits.len();
    let mut prev_bytes: Option<Vec<u8>> = None;
    for &commit in &commits {
        control.check("manifest verification")?;
        let bytes = c.read_file_bounded(&format!("MANIFEST.{commit:08}"), MAX_MANIFEST_BYTES)?;
        let m = Manifest::parse(&bytes)
            .with_context(|| format!("retained manifest {commit} is corrupt"))?;
        if let (Some(want), Some(pb)) = (&m.prev, &prev_bytes) {
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
            match &p.b3 {
                Some(want) => {
                    let reader = c.extent(&p.file).ok_or_else(|| {
                        anyhow::anyhow!(
                            "part {} named by commit {commit} is not held by the container",
                            p.file
                        )
                    })?;
                    let got = hash_reader_with_control(&reader, control, "manifest verification")
                        .with_context(|| format!("part {} named by commit {commit}", p.file))?
                        .to_hex()
                        .to_string();
                    if *want != got {
                        bail!("part {} drifted from the digest commit {commit} pinned", p.file);
                    }
                    report.part_digests += 1;
                }
                None => report.undigested += 1,
            }
        }
        prev_bytes = Some(bytes);
    }
    if let (Some(&newest), Some(pb)) = (commits.last(), &prev_bytes) {
        let live = c.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?;
        if live != *pb {
            bail!("MANIFEST diverges from its retained copy at commit {newest}");
        }
        report.links += 1;
    }
    Ok(report)
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

/// Complete the one narrowly recognizable first-commit crash window.
///
/// This intentionally checks only manifest syntax because it runs before the ordinary writer open
/// performs the same fold/part validation it would perform had the rename landed. It is private so
/// operator recovery cannot bypass [`recover_manifest`]'s exclusive whole-candidate validation.
#[cfg(test)]
fn complete_first_commit(dir: &Path) -> Result<u64> {
    complete_first_commit_with_limits(dir, ReadLimits::default())
}

fn complete_first_commit_with_limits(dir: &Path, read_limits: ReadLimits) -> Result<u64> {
    if Manifest::load_with_limits(dir, read_limits).is_ok() {
        bail!("MANIFEST at {} is intact — refusing to roll back a healthy store", dir.display());
    }
    for c in list_retained_with_limits(dir, read_limits)?.into_iter().rev() {
        if load_retained(dir, c).is_err() {
            continue;
        }
        let bytes = read_manifest_file(&retained_path(dir, c))?;
        promote_manifest_with_limits(dir, c, &bytes, read_limits)?;
        return Ok(c);
    }
    bail!("MANIFEST at {} is damaged and no retained manifest is intact", dir.display());
}

/// Explicit bounds for checked operator recovery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryOptions {
    /// Maximum number of newer retained commits that may be abandoned. Zero repairs only to the
    /// newest retained commit and is the safe default.
    pub max_rollback_commits: u64,
}

/// Evidence returned after checked manifest recovery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub commit: u64,
    pub rollback_commits: u64,
    pub records: usize,
    pub content_values: usize,
    pub parts: usize,
    pub part_sections: usize,
    pub fold_segments: u32,
    pub fold_blocks: usize,
    pub fold_bytes: u64,
}

#[derive(Debug)]
pub enum RecoveryError {
    Healthy(PathBuf),
    RollbackLimit { needed: u64, allowed: u64 },
    NoUsableCandidate { examined: usize, reason: String },
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryError::Healthy(path) => write!(
                f,
                "MANIFEST at {} is intact; refusing recovery of a healthy store",
                path.display()
            ),
            RecoveryError::RollbackLimit { needed, allowed } => write!(
                f,
                "recovery needs to abandon {needed} newer retained commits but only {allowed} were authorized"
            ),
            RecoveryError::NoUsableCandidate { examined, reason } => write!(
                f,
                "none of {examined} retained manifests is a fully readable recovery candidate: {reason}"
            ),
        }
    }
}

impl std::error::Error for RecoveryError {}

/// Checked manifest recovery for a single-file store: writer role by `flock` on the file itself, a
/// healthy refusal by the same rule, candidates validated whole at their exact tails, and
/// promotion as ONE flip that restages `MANIFEST` verbatim and removes the abandoned newer
/// retained members in the same atomic state. The directory protocol's prune-before-promote
/// ordering — two durable steps a crash could land between — has nothing to order here.
pub fn recover_manifest_file(
    path: &Path,
    cfg: FoldCfg,
    options: RecoveryOptions,
) -> Result<RecoveryReport> {
    recover_manifest_file_with_limits_and_control(
        path,
        cfg,
        options,
        ReadLimits::default(),
        &crate::control::OperationControl::default(),
    )
}

/// [`recover_manifest_file`] with explicit admission and cooperative cancellation; the last
/// checkpoint is immediately before the publishing flip.
pub fn recover_manifest_file_with_limits_and_control(
    path: &Path,
    cfg: FoldCfg,
    options: RecoveryOptions,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<RecoveryReport> {
    let read_limits = read_limits.validate()?;
    control.check("manifest recovery")?;
    let mut container = crate::container::Container::open(path)?;
    if container.sealed() {
        bail!(
            "{} is sealed; sealed is final — recovery mutates, so recover a copy instead",
            path.display()
        );
    }
    container.lock_writer()?;
    control.check("manifest recovery")?;
    if container
        .read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)
        .ok()
        .and_then(|b| Manifest::parse(&b).ok())
        .is_some()
    {
        return Err(RecoveryError::Healthy(path.to_path_buf()).into());
    }
    let commits = container_retained_commits(&container);
    let newest = commits.last().copied().unwrap_or(0);
    let mut examined = 0usize;
    let mut last_reason = "no retained manifests exist".to_string();
    for c in commits.into_iter().rev() {
        control.check("manifest recovery validation")?;
        examined += 1;
        let bytes =
            match container.read_file_bounded(&format!("MANIFEST.{c:08}"), MAX_MANIFEST_BYTES) {
                Ok(bytes) => bytes,
                Err(error) => {
                    last_reason = error.to_string();
                    continue;
                }
            };
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
            manifest,
            read_limits,
            control,
        ) {
            Ok(mut report) => {
                let rollback_commits = newest.saturating_sub(c);
                if rollback_commits > options.max_rollback_commits {
                    return Err(RecoveryError::RollbackLimit {
                        needed: rollback_commits,
                        allowed: options.max_rollback_commits,
                    }
                    .into());
                }
                // No cancellation checkpoint after this point: promotion is one flip, and its
                // actual outcome must be reported.
                control.check("manifest recovery publication")?;
                container.put_bytes("MANIFEST", &bytes)?;
                for newer in container_retained_commits(&container) {
                    if newer > c {
                        let _ = container.remove(&format!("MANIFEST.{newer:08}"));
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
                            let _ = container.remove(&name);
                        } else if n == keep_seg && container.member_len(&name) != Some(keep_len) {
                            let ext = container.extent(&name).ok_or_else(|| {
                                anyhow::anyhow!("container lost member {name} mid-recovery")
                            })?;
                            container.put_stream(&name, keep_len, |at, into| {
                                crate::readat::ReadAt::read_exact_at(&ext, into, at)
                            })?;
                            // A sidecar describes the SEALED segment this member no longer is.
                            let _ = container.remove(&format!("{fold_rel}/seg-{n:08}.dir"));
                        }
                    } else if let Some(n) = rest
                        .strip_prefix("seg-")
                        .and_then(|r| r.strip_suffix(".dir"))
                        .and_then(|digits| digits.parse::<u32>().ok())
                    {
                        if n > keep_seg {
                            let _ = container.remove(&name);
                        }
                    }
                }
                container.commit()?;
                report.commit = c;
                report.rollback_commits = rollback_commits;
                return Ok(report);
            }
            Err(error) => {
                if matches!(
                    crate::error::classify(&error),
                    crate::error::ErrorClass::Cancelled
                        | crate::error::ErrorClass::ResourceExhausted
                ) {
                    return Err(error);
                }
                last_reason = error.to_string();
            }
        }
    }
    Err(RecoveryError::NoUsableCandidate { examined, reason: last_reason }.into())
}

/// The home-neutral half of candidate validation: every visible record's every content value
/// reconstructed piece by piece, lengths checked, whole-value identities checked where carried.
fn validate_candidate_records(
    reader: &ReadStore,
    control: &crate::control::OperationControl,
) -> Result<(usize, usize)> {
    let ids = reader.ids()?;
    let mut content_values = 0usize;
    for id in &ids {
        control.check("manifest recovery validation")?;
        let record = reader.get(id)?.expect("ids returns visible records");
        for content in record.contents {
            control.check("manifest recovery validation")?;
            let mut identity = blake3::Hasher::new();
            for op in content.ops {
                control.check("manifest recovery validation")?;
                match op {
                    BodyOp::Lit(bytes) => {
                        identity.update(&bytes);
                    }
                    BodyOp::Piece { hash, len } => {
                        let mut loc = None;
                        for part in reader.parts.iter().rev() {
                            if let Some(found) = part.lookup_piece(&hash)? {
                                loc = Some(found);
                                break;
                            }
                        }
                        let loc = loc.ok_or_else(|| {
                            anyhow::anyhow!(
                                "candidate record {id:?} references absent piece {hash}"
                            )
                        })?;
                        let bytes = reader.fold.read_verified(loc, hash)?;
                        if bytes.len() != len as usize {
                            bail!(
                                "candidate record {id:?} says piece {hash} is {len} bytes but it is {}",
                                bytes.len()
                            );
                        }
                        identity.update(&bytes);
                    }
                }
            }
            if let Some(want) = content.identity {
                let got = ContentHash(identity.finalize().into());
                if got != want {
                    bail!(
                        "candidate record {id:?} content {:?} has identity {got}, expected {want}",
                        content.name
                    );
                }
            }
            content_values += 1;
        }
    }
    Ok((ids.len(), content_values))
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
) -> Result<RecoveryReport> {
    control.check("manifest recovery validation")?;
    let prefix = crate::fold::fold_member_prefix(manifest.fold_gen);
    let tail = manifest.fold_tail();
    let mut segs = Vec::new();
    let mut dict_files = Vec::new();
    for name in c.names().map(String::from).collect::<Vec<_>>() {
        let Some(rest) = name.strip_prefix(&format!("{prefix}/")) else { continue };
        if let Some(n) = crate::fold::segment::parse_seg_name(rest) {
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
                    c.read_file_bounded(
                        &format!("{prefix}/seg-{n:08}.dir"),
                        crate::fold::segment::max_dir_sidecar_bytes(len)
                            .min(read_limits.max_stored_frame_bytes),
                    )
                    .ok()
                } else {
                    None
                },
            });
        } else if rest.starts_with("zdict-") && rest.ends_with(".zd") {
            dict_files.push(c.read_file_bounded(&name, crate::fold::MAX_DICTIONARY_BYTES)?);
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
        control.check("manifest recovery validation")?;
        let reader = c.extent(&part_ref.file).ok_or_else(|| {
            anyhow::anyhow!(
                "candidate commit {} names part {} but the container does not hold it",
                manifest.commit,
                part_ref.file
            )
        })?;
        if let Some(want) = &part_ref.b3 {
            let got = hash_reader_with_control(&reader, control, "manifest recovery validation")?
                .to_hex()
                .to_string();
            if &got != want {
                bail!(
                    "candidate commit {} names part {} with the wrong digest",
                    manifest.commit,
                    part_ref.file
                );
            }
        }
        let part =
            Arc::new(Part::open_reader_with_limits(Box::new(reader), pcache.clone(), read_limits)?);
        part_sections += part.verify_sections_with_control(control)?;
        parts.push(part);
    }
    let reader = ReadStore { fold: Arc::new(fold), parts, manifest, read_limits };
    let (records, content_values) = validate_candidate_records(&reader, control)?;
    Ok(RecoveryReport {
        records,
        content_values,
        parts: reader.parts.len(),
        part_sections,
        fold_segments: scrub.segments,
        fold_blocks: scrub.blocks,
        fold_bytes: scrub.bytes,
        ..RecoveryReport::default()
    })
}

/// [`hash_file_with_control`] for bytes that live behind a reader — a member of the live file.
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

fn hash_file_with_control(
    path: &Path,
    control: &crate::control::OperationControl,
    operation: &'static str,
) -> Result<blake3::Hash> {
    use std::io::Read;

    let mut file = crate::vfs::open_read(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        control.check(operation)?;
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finalize())
}

/// `<store>` is absent and `<store>.reclaimed` — a reclaim's anchor — is present: promote the
/// anchor to the store's name, or refuse. One contender at a time: the anchor's own writer lock
/// is the exclusion, so a second recoverer sees `WriterLocked`. The anchor is validated whole
/// (fold at its tail scrubbed, every part by digest and sections, every record reconstructed —
/// the same bar as manifest recovery) BEFORE anything is created; a corrupt or incomplete anchor
/// refuses and leaves everything as found. Promotion is a copy to a fresh candidate, fsynced,
/// writer-locked, and published at `<store>` by a write-through no-replace rename — a name taken
/// meanwhile refuses with the anchor intact — after which the anchor is unlinked (laggable). A
/// crash at any point re-enters here on the next writer open and converges.
fn recover_store_from_reclaim_anchor(
    path: &Path,
    cfg: FoldCfg,
    read_limits: ReadLimits,
) -> Result<()> {
    let names = crate::container::reclaim_names(path);
    let anchor = crate::container::Container::open(&names.anchor)
        .with_context(|| format!("open reclaim anchor {}", names.anchor.display()))?;
    anchor.lock_writer().with_context(|| "another recovery of this store is in progress")?;
    if anchor.sealed() {
        bail!("reclaim anchor {} is sealed; not a reclaim's output", names.anchor.display());
    }
    let manifest = Manifest::parse(&anchor.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?;
    let control = crate::control::OperationControl::default();
    validate_recovery_candidate_container(
        &anchor,
        &names.anchor,
        cfg,
        manifest,
        read_limits,
        &control,
    )
    .with_context(|| {
        format!(
            "reclaim anchor {} does not validate whole; the store is not recreated from it",
            names.anchor.display()
        )
    })?;
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let _ = crate::vfs::unlink(&names.candidate_tmp);
    let _ = crate::vfs::unlink(&names.candidate);
    crate::container::copy_container_bytes_pub(&names.anchor, &names.candidate_tmp)?;
    crate::vfs::sync_dir(&parent)?;
    crate::vfs::rename_noreplace(&names.candidate_tmp, &names.candidate)?;
    crate::vfs::sync_dir(&parent)?;
    let new_store = crate::vfs::open_rw(&names.candidate)?;
    if !crate::sys::lock_exclusive(&new_store)? {
        return Err(crate::fold::WriterLocked { path: names.candidate.clone() }.into());
    }
    crate::container::Container::open(&names.candidate)?.verify()?;
    crate::vfs::rename_noreplace(&names.candidate, path).with_context(|| {
        format!("{} was taken while the reclaim anchor was being promoted", path.display())
    })?;
    crate::vfs::sync_dir(&parent)?;
    drop(anchor);
    let _ = crate::vfs::unlink(&names.anchor);
    crate::vfs::sync_dir(&parent)
        .with_context(|| format!("sync {} after removing the reclaim anchor", parent.display()))?;
    drop(new_store);
    Ok(())
}

/// Promote retained `commit` to the live `MANIFEST` of a directory-layout store, but only after
/// validating the candidate whole — the same bar as [`recover_manifest_file`]'s candidates, on
/// files instead of container members. Used by open when the live manifest is absent beside a
/// commit log; the caller has already established that shape.
fn promote_newest_retained_if_whole(
    dir: &Path,
    cfg: FoldCfg,
    commit: u64,
    read_limits: ReadLimits,
) -> Result<()> {
    let bytes = read_manifest_file(&retained_path(dir, commit))?;
    let manifest = Manifest::parse(&bytes)?;
    if manifest.commit != commit {
        bail!(
            "retained copy {} carries commit {}, not the commit its name claims",
            retained_path(dir, commit).display(),
            manifest.commit
        );
    }
    let control = crate::control::OperationControl::default();
    validate_recovery_candidate_dir(dir, cfg, manifest, read_limits, &control)?;
    promote_manifest_with_limits(dir, commit, &bytes, read_limits)
}

/// [`validate_recovery_candidate_container`] for the directory layout: the candidate's fold,
/// bounded at its tail and scrubbed; every part it names, by digest where recorded and by
/// section checksums always; then every record it serves, reconstructed byte for byte.
fn validate_recovery_candidate_dir(
    dir: &Path,
    cfg: FoldCfg,
    manifest: Manifest,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<RecoveryReport> {
    control.check("manifest recovery validation")?;
    let fold_dir = refold::fold_dir(dir, manifest.fold_gen);
    let tail = manifest.fold_tail();
    let mut segs = Vec::new();
    let mut dict_files = Vec::new();
    let mut entries = 0u64;
    for entry in std::fs::read_dir(&fold_dir).with_context(|| {
        format!("candidate commit {} names fold {}", manifest.commit, fold_dir.display())
    })? {
        control.check("manifest recovery validation")?;
        entries = entries.saturating_add(1);
        read_limits.admit_directory_entries("candidate fold directory", entries)?;
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(n) = crate::fold::segment::parse_seg_name(&name) {
            let file = crate::vfs::open_read(&entry.path())?;
            let full_len = crate::readat::ReadAt::len(&file)?;
            let (reader, len, whole): (Arc<dyn crate::readat::ReadAt>, u64, bool) = match tail {
                Some(t) if n > t.seg => continue,
                Some(t) if n == t.seg => (
                    Arc::new(crate::readat::Slice::new(file, 0, u64::from(t.off))),
                    u64::from(t.off),
                    u64::from(t.off) == full_len,
                ),
                _ => (Arc::new(file), full_len, true),
            };
            segs.push(crate::fold::SegmentInput {
                seg: n,
                reader,
                sidecar: if whole {
                    read_bounded(
                        &fold_dir.join(format!("seg-{n:08}.dir")),
                        crate::fold::segment::max_dir_sidecar_bytes(len)
                            .min(read_limits.max_stored_frame_bytes),
                    )
                    .ok()
                } else {
                    None
                },
            });
        } else if name.starts_with("zdict-") && name.ends_with(".zd") {
            dict_files.push(read_bounded(&entry.path(), crate::fold::MAX_DICTIONARY_BYTES)?);
        }
    }
    let fold = Fold::open_read_from_with_limits(
        segs,
        dict_files,
        cfg,
        &fold_dir,
        &manifest.punched,
        read_limits,
    )?;
    let scrub = fold.scrub_with_control(control)?;
    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    let mut part_sections = 0usize;
    for part_ref in &manifest.parts {
        control.check("manifest recovery validation")?;
        let path = dir.join(&part_ref.file);
        if !path.is_file() {
            bail!(
                "candidate commit {} names part {} but the directory does not hold it",
                manifest.commit,
                part_ref.file
            );
        }
        if let Some(want) = &part_ref.b3 {
            let got = hash_file_with_control(&path, control, "manifest recovery validation")?
                .to_hex()
                .to_string();
            if &got != want {
                bail!(
                    "candidate commit {} names part {} with the wrong digest",
                    manifest.commit,
                    part_ref.file
                );
            }
        }
        let part = Arc::new(Part::open_in_with_limits(&path, pcache.clone(), read_limits)?);
        part_sections += part.verify_sections_with_control(control)?;
        parts.push(part);
    }
    let reader = ReadStore { fold: Arc::new(fold), parts, manifest, read_limits };
    let (records, content_values) = validate_candidate_records(&reader, control)?;
    Ok(RecoveryReport {
        records,
        content_values,
        parts: reader.parts.len(),
        part_sections,
        fold_segments: scrub.segments,
        fold_blocks: scrub.blocks,
        fold_bytes: scrub.bytes,
        ..RecoveryReport::default()
    })
}

/// Read a whole small file, refusing one larger than `max` rather than allocating for it.
fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = crate::vfs::open_read(path)?;
    let announced = file.metadata()?.len();
    if announced > max {
        bail!("{} is {announced} bytes, over the {max}-byte limit", path.display());
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(announced as usize).context("reserve read buffer")?;
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        bail!("{} grew past the {max}-byte limit while reading", path.display());
    }
    Ok(bytes)
}

fn promote_manifest_with_limits(
    dir: &Path,
    commit: u64,
    bytes: &[u8],
    read_limits: ReadLimits,
) -> Result<()> {
    // Recovery stages one temporary name before publication. If MANIFEST is absent, the rename
    // also leaves one additional persistent root entry. Reserve that worst case before changing
    // the supplied directory; the abandonment pruning below only removes entries, so the
    // reservation is not affected by it.
    let directory_entries = count_directory_entries(dir, read_limits, "store directory")?;
    read_limits.admit_directory_entries(
        "store directory during manifest recovery",
        directory_entries.saturating_add(1),
    )?;
    // Abandonment becomes durable BEFORE the new timeline publishes. The reverse order left a
    // window where the promoted MANIFEST and the abandoned newer retained manifests were both
    // durable: a crash there resurrected the abandoned timeline, a re-run of recovery would see
    // the chain diverge, treat the store as damaged, and — because the resurrected commits are
    // genuine descendants that can validate — promote the exact history the operator authorized
    // abandoning. Pruning first converges instead: a crash before the rename leaves the damaged
    // manifest and fewer candidates, and re-running recovery promotes the same target. The
    // unlinks are propagated, not best-effort — an abandonment that cannot be made durable must
    // not be reported as one.
    let mut abandoned = false;
    for retained in list_retained_with_limits(dir, read_limits)? {
        if retained > commit {
            crate::vfs::unlink(&retained_path(dir, retained))?;
            abandoned = true;
        }
    }
    if abandoned {
        crate::vfs::sync_dir(dir)?;
    }
    let tmp = dir.join("MANIFEST.tmp");
    let f = crate::vfs::create(&tmp)?;
    crate::vfs::write_all_at(&f, &tmp, bytes, 0)?;
    crate::vfs::sync_file(&f, &tmp)?;
    drop(f);
    crate::vfs::rename(&tmp, &dir.join("MANIFEST"))?;
    crate::vfs::sync_dir(dir)?;
    Ok(())
}

/// The retained snapshot commits a single-file store holds, oldest first — the commits
/// [`open_read_container_at`] can still serve.
pub fn retained_commits_file(path: &Path) -> Result<Vec<u64>> {
    let c = crate::container::Container::open(path)?;
    Ok(container_retained_commits(&c))
}

/// Verify a single-file store's retained manifest chain: prev-links across the retained
/// members, every part pin hashed against the extents the file actually holds, and the live
/// manifest checked byte-identical to its newest retained copy.
pub fn verify_chain_file(path: &Path) -> Result<ChainReport> {
    let c = crate::container::Container::open(path)?;
    verify_chain_container(&c, &crate::control::OperationControl::default())
}

/// Restore a single-file backup: verify every member of `src` against its recorded checksums,
/// then publish a byte-identical copy at `dst` with a rename that refuses to replace. The
/// backup IS a store — sealed by `backup`, final by flag — so restoring is verified copying,
/// and a crash leaves staging litter and an untouched destination.
pub fn restore_file(src: &Path, dst: &Path) -> Result<crate::pack::RestoreStats> {
    restore_file_with_control(src, dst, &crate::control::OperationControl::default())
}

/// [`restore_file`] with cooperative cancellation; the last checkpoint is immediately before the
/// publishing rename, and a cancelled restore removes its staging and never publishes.
pub fn restore_file_with_control(
    src: &Path,
    dst: &Path,
    control: &crate::control::OperationControl,
) -> Result<crate::pack::RestoreStats> {
    control.check("backup restore")?;
    crate::pack::ensure_destination_available(dst)?;
    let c = crate::container::Container::open(src)?;
    // A member failing its checksum is CORRUPTION, and the refusal must classify as such —
    // the same integrity wrapper every verification path speaks through.
    let members = verification_integrity("verify backup before restoring", c.verify())?;
    drop(c);
    control.check("backup restore")?;
    let mut staging = dst.as_os_str().to_os_string();
    staging.push(".restoring");
    let staging = PathBuf::from(staging);
    let _ = crate::vfs::unlink(&staging);
    // The copy goes through the vfs seam in bounded chunks and is fsynced before anything
    // depends on it: the publishing rename must never make bytes reachable that a crash could
    // still take back — and a copy the crash simulator cannot see is a copy the crash-safety
    // argument does not cover.
    {
        use std::io::Read;
        let mut from =
            crate::vfs::open_read(src).with_context(|| format!("open backup {}", src.display()))?;
        let to = crate::vfs::create(&staging)
            .with_context(|| format!("stage restore at {}", staging.display()))?;
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
    }
    // Restoring births a WRITABLE store: the backup stays sealed forever, and the copy is a new
    // file whose life is just starting. The unseal happens on the staging side of the rename, so
    // the destination is never observable sealed.
    {
        let mut fresh = crate::container::Container::open(&staging)?;
        fresh.clear_seal_for_restore()?;
    }
    if let Err(interrupted) = control.check("backup restore publication") {
        let _ = crate::vfs::unlink(&staging);
        return Err(interrupted.into());
    }
    crate::vfs::rename_noreplace(&staging, dst)?;
    if let Some(parent) = dst.parent() {
        crate::vfs::sync_dir(parent).with_context(|| {
            format!("sync {} after publishing {}", parent.display(), dst.display())
        })?;
    }
    // Member bytes, not file length: the same accounting the backup reported, so a consumer can
    // compare the two stats and see the identity they describe.
    let restored = crate::container::Container::open(dst)?;
    let bytes = restored.member_bytes();
    let commit =
        Manifest::parse(&restored.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?.commit;
    Ok(crate::pack::RestoreStats { files: members, bytes, commit })
}

/// What a conversion carried forward.
#[derive(Clone, Copy, Debug)]
pub struct ConvertStats {
    /// Members written into the fresh store.
    pub members: usize,
    /// Bytes those members hold.
    pub bytes: u64,
    /// The manifest commit the converted store opens at, unchanged from the source.
    pub commit: u64,
}

/// Convert an old layout — a store directory, or a sealed pack — into a single-file store.
///
/// This is the one door those layouts keep. A directory store is opened with the writer role
/// (which settles its WAL: acknowledged records ride along, exactly as a reopen would carry
/// them), flushed if it held staged records, and its committed snapshot copied member by member.
/// A pack is copied straight from its extents. Either way the output is a fresh, WRITABLE
/// single-file store carrying the source's manifest verbatim — same commit counter, same part
/// pins, same punched declaration — verified whole before this returns, refused rather than
/// replacing anything at `out`.
///
/// The retained commit log deliberately does not convert: it pins the source's history, and the
/// converted file starts its history at the commit it carries — the same posture a pack has
/// always taken.
pub fn convert_to_file(src: &Path, out: &Path) -> Result<ConvertStats> {
    if !src.exists() {
        // Typed absence: a missing source is NOT_FOUND, never a shrug about unrecognized layouts.
        return Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} does not exist", src.display()),
        )));
    }
    crate::pack::ensure_destination_available(out)?;
    // The converted store is BUILT in staging and published with a rename that refuses to
    // replace: a crash mid-conversion leaves staging litter and an absent destination, and
    // re-running the conversion is always the whole recovery story.
    let mut staging = out.as_os_str().to_os_string();
    staging.push(".converting");
    let staging = PathBuf::from(staging);
    let _ = crate::vfs::unlink(&staging);
    let stats = if src.is_dir() {
        // The writer role settles the WAL and replays acknowledged records into the memtable;
        // one flush makes them part of the committed snapshot the copy walks.
        let mut store = Store::open(src, FoldCfg::default())?;
        store.sync()?;
        store.flush()?;
        drop(store);
        let commit = checkpoint_into_container(src, &staging)?;
        let fresh = crate::container::Container::open(&staging)?;
        fresh.verify()?;
        ConvertStats { members: fresh.len(), bytes: fresh.member_bytes(), commit }
    } else {
        let kind = single_file_kind(src).ok_or_else(|| {
            anyhow::anyhow!(
                "{} is neither a store directory, a pack, nor a container",
                src.display()
            )
        })?;
        match kind {
            SingleFileKind::Container => bail!(
                "{} is already a single-file store; a writer open upgrades its revision in place",
                src.display()
            ),
            SingleFileKind::Pack => {
                let pack = crate::pack::Pack::open(src)?;
                let mut fresh = crate::container::Container::create(&staging)?;
                let mut members = 0usize;
                let mut bytes = 0u64;
                for name in pack.names().map(String::from).collect::<Vec<_>>() {
                    let reader = pack.file(&name).expect("name came from this pack");
                    let len = crate::readat::ReadAt::len(&reader)?;
                    fresh.put_stream(&name, len, |at, into| {
                        crate::readat::ReadAt::read_exact_at(&reader, into, at)
                    })?;
                    members += 1;
                    bytes += len;
                }
                fresh.commit()?;
                fresh.verify()?;
                let manifest =
                    Manifest::parse(&fresh.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?)?;
                ConvertStats { members, bytes, commit: manifest.commit }
            }
        }
    };
    crate::vfs::rename_noreplace(&staging, out)?;
    if let Some(parent) = out.parent() {
        // The published name is the result; a failed directory sync means it may not survive a
        // crash, and the operation reports that rather than success.
        crate::vfs::sync_dir(parent).with_context(|| {
            format!("sync {} after publishing {}", parent.display(), out.display())
        })?;
    }
    Ok(stats)
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
            digits.parse().ok()
        } else {
            None
        }
    }
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
        live.parts.iter().map(|p| p.file.clone()).collect();
    let mut keep_gens: std::collections::HashSet<u32> = std::iter::once(live.fold_gen).collect();
    for commit in container_retained_commits(c) {
        let name = format!("MANIFEST.{commit:08}");
        // A retained copy that fails to read or parse pins nothing — identical to the file rule.
        let Ok(bytes) = c.read_file_bounded(&name, MAX_MANIFEST_BYTES) else { continue };
        let Ok(m) = Manifest::parse(&bytes) else { continue };
        keep_parts.extend(m.parts.into_iter().map(|p| p.file));
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
            let _ = c.remove(&name);
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

/// Where a store lives: a directory of files, or one container file. Everything the engine does
/// is home-neutral above the storage seams; what branches here is only placement — where a part
/// lands, how a manifest publishes, what a sweep frees.
enum Home {
    Dir(PathBuf),
    File { path: PathBuf, container: std::sync::Arc<std::sync::Mutex<crate::container::Container>> },
}

impl Home {
    /// The directory a directory store lives in. A refusal for a single-file store — used by
    /// operations not yet taught the single-file protocol, so an unconverted path refuses loudly
    /// instead of inventing filesystem locations that do not exist.
    fn dir(&self) -> Result<&Path> {
        match self {
            Home::Dir(dir) => Ok(dir),
            Home::File { path, .. } => bail!(
                "{} is a single-file store; this operation has not yet been taught its protocol",
                path.display()
            ),
        }
    }
}

pub struct Store {
    home: Home,
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
}

/// Cheap operational state for an embedder's health and metrics endpoint.
///
/// This reports engine facts, not a telemetry format or consumer policy. Counters that require a
/// full visibility walk (for example exact live-record count) are deliberately absent from this
/// cheap snapshot rather than hidden behind a surprisingly expensive getter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreHealth {
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

/// File bytes in one reachability class.
///
/// `logical_bytes` is portable file length. `allocated_bytes` measures filesystem blocks and is
/// `None` where the platform cannot report sparse allocation honestly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceAmount {
    pub files: usize,
    pub logical_bytes: u64,
    pub allocated_bytes: Option<u64>,
}

impl Default for SpaceAmount {
    fn default() -> Self {
        SpaceAmount {
            files: 0,
            logical_bytes: 0,
            allocated_bytes: if cfg!(any(unix, windows)) { Some(0) } else { None },
        }
    }
}

/// Exact reachability-aware storage facts for preflight and operational reporting.
///
/// Categories are disjoint. `retained_only` is not garbage: bounded time-travel manifests still
/// require it. `unclassified` is deliberately not called reclaimable because an embedder may have
/// placed an unrelated file in the directory and TurnDB has no authority to delete it.
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

/// Exact progress of upgrading the current live immutable-part set to this build's format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FormatMigrationStatus {
    pub target_part_version: u8,
    pub live_parts: usize,
    pub current_parts: usize,
    pub legacy_parts: usize,
    pub legacy_rows: u64,
    pub legacy_bytes: u64,
    /// Unique legacy parts pinned only by retained manifests, not counted in `legacy_parts`.
    pub retained_legacy_parts: usize,
    pub retained_legacy_rows: u64,
    pub retained_legacy_bytes: u64,
}

/// Exact source facts and an advisory stage estimate for one resumable migration step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatMigrationPlan {
    pub part_index: usize,
    pub source_part_version: u8,
    pub seq_lo: u64,
    pub seq_hi: u64,
    pub input_rows: u64,
    pub input_bytes: u64,
    pub input_sections: usize,
    pub input_raw_section_bytes: u64,
    pub estimated_stage_bytes: u64,
    pub estimate_is_hard_bound: bool,
    pub retained_input_bytes_after_commit: u64,
    pub filesystem_available_bytes: Option<u64>,
}

/// Evidence returned after atomically publishing one migrated part.
#[derive(Clone, Copy, Debug)]
pub struct FormatMigrationStep {
    pub plan: FormatMigrationPlan,
    pub output_bytes: u64,
    pub remaining_legacy_parts: usize,
    pub rewrite: crate::part::merge::MergeStats,
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

    /// Open a writer over the retired directory layout — the converter's private door, and no
    /// one else's. Takes the writer lock (through the fold's lock file) and recovers, exactly as
    /// every directory session always did, because a conversion must settle the WAL it finds.
    pub(crate) fn open(dir: &Path, cfg: FoldCfg) -> Result<Store> {
        Self::open_with_options(dir, StoreOptions { fold: cfg, ..StoreOptions::default() })
    }

    /// Open a writer over the single-file store — THE way a store opens for writing.
    ///
    /// `path` names a container; an absent path becomes a new, empty store, exactly as an absent
    /// directory does. Beside it while open: `<path>-wal`, replayed here if a crash left it, and
    /// nothing else. Writer exclusion is `flock` on the file itself, kernel-released on death.
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
        let StoreOptions { fold: cfg, write_limits, read_limits, part_cache_bytes } = options;
        let write_limits = write_limits.validate()?;
        let read_limits = read_limits.validate()?;
        if part_cache_bytes < crate::part::cache::BUDGET_MIN {
            bail!("part_cache_bytes must be at least {}", crate::part::cache::BUDGET_MIN);
        }
        if path.is_dir() {
            bail!(
                "{} is a directory; a store is one file — the directory layout is retired, and \
                 `convert` is its one door",
                path.display()
            );
        }
        // A 0.1.x working session beside the store (CHANGELOG, 0.1.0/0.1.2) may hold acknowledged
        // writes only that release can settle: refuse, name it, never remove it.
        let legacy = debris::refusal_beside(path, read_limits)?;
        if !legacy.is_empty() {
            bail!(
                "{} has a 0.1.x working directory beside it ({}), which may hold acknowledged \
                 writes only that release can settle; open it with the release that wrote it, or \
                 move it aside deliberately — this release never removes it",
                path.display(),
                legacy.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            );
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
            // A reclaim's replace crashed in the state its protocol admits — the store's name
            // gone, the anchor intact. Recover from the anchor, or refuse; never create.
            recover_store_from_reclaim_anchor(path, cfg, read_limits)?;
        }
        let container = if !path.exists() {
            // The parent directories are created exactly as the directory store's open always
            // created its own — the friendliness embedders relied on, kept.
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    crate::vfs::mkdir_all(parent)?;
                }
            }
            crate::container::Container::create(path)?
        } else {
            match crate::container::Container::open(path) {
                Ok(c) => c,
                // A crash inside creation leaves a file no longer than the superblock region —
                // short slots, torn slots, or both dropped entirely — and such a file provably
                // names no member byte: nothing durable lives below REGION_START. The writer's
                // create-if-absent contract finishes the birth. One byte longer and the refusal
                // stands: a mature store with both slots smashed holds someone's members, and
                // re-birthing it would be data loss wearing recovery's clothes.
                Err(_) if std::fs::metadata(path)?.len() <= crate::container::REGION_START => {
                    crate::container::Container::recreate_interrupted(path)?
                }
                Err(e) => return Err(e),
            }
        };
        if container.sealed() {
            bail!("{} is sealed; sealed is final — a writer cannot open it", path.display());
        }
        container.lock_writer()?;
        // The store is present, so it is authority: every transient name beside it — reclaim
        // material, a Windows pending publish, a merge's scratch, an artifact staging — is dead by
        // the protocol and removed here, counted on success; a removal that fails is this open's
        // error with the path and the cause (#126), and nothing is counted.
        let debris_removed = debris::remove_beside_present_store(path, read_limits)?;

        // The manifest is a member. Missing means a new store — UNLESS retained commits exist,
        // which is the same tripwire the directory store fires: this store has committed before
        // and MANIFEST was lost, and opening it as new buries the loss.
        let manifest = if container.contains("MANIFEST") {
            let bytes = container.read_file_bounded("MANIFEST", MAX_MANIFEST_BYTES)?;
            verification_integrity("open committed manifest", Manifest::parse(&bytes))?
        } else {
            let retained = container_retained_commits(&container);
            if retained.is_empty() {
                Manifest::default()
            } else {
                return verification_integrity(
                    "open committed manifest",
                    Err(anyhow::anyhow!(
                        "MANIFEST is missing but {} retained commits exist in {} — a damaged \
                         store, not a new one",
                        retained.len(),
                        path.display()
                    )),
                );
            }
        };
        let retained_commit_count = container_retained_commits(&container).len();
        let container = std::sync::Arc::new(std::sync::Mutex::new(container));

        // No residue reconciliation and no fold truncation: a retained commit newer than live
        // cannot exist (one flip names everything), and the committed extent lists ARE the
        // truncation. The recovery passes the directory open performs here are unaskable.
        let fold = verification_integrity(
            "open committed fold",
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
                container.lock().expect("container lock poisoned").extent(&p.file).ok_or_else(
                    || {
                        anyhow::anyhow!(
                            "MANIFEST names part {} but {} does not hold it",
                            p.file,
                            path.display()
                        )
                    },
                )?;
            parts.push(Arc::new(verification_integrity(
                "open committed part",
                Part::open_reader_with_limits(Box::new(reader), pcache.clone(), read_limits),
            )?));
        }

        // Stage the sweep's frees now; they publish with the first flip that has anything else
        // to say. Nothing reads a free-listed member meanwhile — no manifest names it.
        {
            let mut c = container.lock().expect("container lock poisoned");
            sweep_unreachable_container(&mut c, &manifest, read_limits)?;
        }
        // A crashed merge leaves its spool scratch beside the store — pre-commit garbage, removed
        // whole. The member it was assembling is uncommitted noise needing nothing.
        let tmp_dir = file_tmp_dir(path);
        if tmp_dir.exists() {
            let _ = crate::vfs::remove_tree(&tmp_dir);
        }

        let wal_path = file_wal_path(path);
        let replay = Wal::replay_state_with_limits(&wal_path, read_limits)?;
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
                // bytes into the file for nothing. (The directory open still re-appends there —
                // a space cost, not a correctness one — and inherits this fix when it converges.)
                let known = fold.lookup(*h).is_some()
                    || parts.iter().rev().any(|p| matches!(p.lookup_piece(h), Ok(Some(_))));
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
        metrics.open_recovery.observe_success(recovery_duration);
        let mut events = crate::observability::EventJournal::default();
        let recovery_result: Result<()> = Ok(());
        events.observe(
            crate::observability::LifecycleOperation::OpenRecovery,
            recovery_duration,
            &recovery_result,
        );
        Ok(Store {
            home: Home::File { path: path.to_path_buf(), container },
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
        })
    }

    /// Settle and release this writer. For a single-file store this is what leaves exactly one
    /// file at rest: the memtable flushes if it holds anything, and the emptied WAL sidecar is
    /// removed. A store dropped without closing keeps its sidecar — present-at-open means crash,
    /// and the next open replays it — so close is a tidy, never a requirement.
    pub fn close(mut self) -> Result<()> {
        if !self.mem.is_empty() {
            self.flush()?;
        }
        if let Home::File { path, .. } = &self.home {
            if self.wal.frame_count() == 0 {
                let wal_path = file_wal_path(path);
                let _ = crate::vfs::unlink(&wal_path);
                if let Some(parent) = wal_path.parent() {
                    // "Exactly one file" is close's promise; it is not durable if this fails.
                    crate::vfs::sync_dir(parent).with_context(|| {
                        format!("sync {} after removing the write-ahead log", parent.display())
                    })?;
                }
            }
        }
        Ok(())
    }

    fn open_with_options(dir: &Path, options: StoreOptions) -> Result<Store> {
        let recovery_started = std::time::Instant::now();
        let StoreOptions { fold: cfg, write_limits, read_limits, part_cache_bytes } = options;
        let write_limits = write_limits.validate()?;
        let read_limits = read_limits.validate()?;
        if part_cache_bytes < crate::part::cache::BUDGET_MIN {
            bail!("part_cache_bytes must be at least {}", crate::part::cache::BUDGET_MIN);
        }
        crate::vfs::mkdir_all(dir)?;
        let manifest = match Manifest::load_with_limits(dir, read_limits) {
            Ok(m) => m,
            Err(e) => {
                // A crash inside the FIRST commit is the one state where MANIFEST can be
                // legitimately absent beside a commit log: the retained copy lands before the
                // rename, and commit 1 has no previous manifest to leave behind. A log of exactly
                // [1] with no MANIFEST is that signature — an intact copy COMPLETES the commit
                // (data before pointers makes promotion indistinguishable from the crash landing
                // a moment later), a torn copy VOIDS it (nothing was published). Every other
                // missing-manifest shape means a manifest that once existed is gone, and stays a
                // refusal. Found by the DST harness at the first flush's commit window.
                let retained = list_retained_with_limits(dir, read_limits)?;
                let live_absent = !crate::vfs::exists(&dir.join("MANIFEST"));
                if live_absent && retained == [1] {
                    if load_retained(dir, 1).is_ok() {
                        complete_first_commit_with_limits(dir, read_limits)?;
                    } else {
                        crate::vfs::unlink(&retained_path(dir, 1))?;
                        crate::vfs::sync_dir(dir)?;
                    }
                    verification_integrity(
                        "open committed manifest",
                        Manifest::load_with_limits(dir, read_limits),
                    )?
                } else if live_absent && !retained.is_empty() {
                    // The one other legitimate shape: a crash INSIDE the manifest publish on a
                    // platform whose replace-rename can leave neither the old name nor the new
                    // (Windows; tests/dst.rs `rename-neither`). The commit protocol publishes
                    // `MANIFEST.<commit>` before it touches the live name, so the newest retained
                    // copy IS the manifest that was being published — but only if it validates
                    // whole: the manifest, its fold at the candidate's tail, every part it names
                    // by digest and section, and every record it serves. Anything less stays the
                    // refusal below; an absent live manifest beside a damaged newest copy is a
                    // damaged store, and rolling back to an older copy is an operator's decision
                    // (`recover`), never an open's.
                    let newest = *retained.last().expect("non-empty");
                    match promote_newest_retained_if_whole(dir, cfg, newest, read_limits) {
                        Ok(()) => verification_integrity(
                            "open committed manifest",
                            Manifest::load_with_limits(dir, read_limits),
                        )?,
                        Err(why) => {
                            return verification_integrity(
                                "open committed manifest",
                                Err(e.context(format!(
                                    "MANIFEST is absent and the newest retained copy \
                                     (commit {newest}) does not validate whole, so it was not \
                                     promoted: {why:#}"
                                ))),
                            );
                        }
                    }
                } else {
                    return verification_integrity("open committed manifest", Err(e));
                }
            }
        };

        // The live MANIFEST is the only commit point, so a retained name it does not dominate is
        // residue, not state. Two shapes exist. A retained commit NEWER than live is a commit
        // that never took effect — a crash between the commit protocol's retained copy and its
        // rename; whatever it acknowledged is still in the WAL and replays below. A retained
        // manifest from ANOTHER FOLD GENERATION is a re-fold's purge that lost a race with a
        // crash, and keeps content the re-fold exists to erase readable. Left in place, either
        // shape pins files this manifest's counters will re-create by name, so a later flush
        // would truncate a file a retained manifest still claims a digest for — and verification
        // would report an inconsistency in a store that is behaving correctly. Removal is durable
        // and precedes the sweep, which then collects whatever only these names pinned. A torn
        // retained copy parses as nothing, pins nothing, and is left for window pruning.
        let mut reconciled = false;
        for c in list_retained_with_limits(dir, read_limits)? {
            let stale_timeline = c > manifest.commit;
            let stale_generation = !stale_timeline
                && load_retained(dir, c).map(|m| m.fold_gen != manifest.fold_gen).unwrap_or(false);
            if stale_timeline || stale_generation {
                crate::vfs::unlink(&retained_path(dir, c))?;
                reconciled = true;
            }
        }
        if reconciled {
            crate::vfs::sync_dir(dir)?;
        }

        let root_entries = count_directory_entries(dir, read_limits, "store directory")?;
        let fold_path = refold::fold_dir(dir, manifest.fold_gen);
        let additions = u64::from(!fold_path.exists()) + u64::from(!dir.join("WAL").exists());
        read_limits.admit_directory_entries(
            "store directory during writer open",
            root_entries.saturating_add(additions),
        )?;

        // Recovery is a truncate, not a negotiation: whatever the fold wrote past the committed tail
        // is discarded, and the log regenerates it. The punched declaration rides in with the tail
        // because recovery needs it DURING the scan: a crash mid-punch can leave a declared block's
        // payload partially zeroed, and without the declaration that frame reads as a torn write
        // and the committed tail as lost durable bytes.
        let mut fold = verification_integrity(
            "open committed fold",
            Fold::open_at_over_with_limits(
                &fold_path,
                cfg,
                manifest.fold_tail(),
                &manifest.punched,
                Vec::new(),
                read_limits,
            ),
        )?;

        let pcache = Arc::new(SectionCache::new(part_cache_bytes));
        let mut parts = Vec::with_capacity(manifest.parts.len());
        for p in &manifest.parts {
            let opened = verification_integrity(
                "open committed part",
                Part::open_in_with_limits(&dir.join(&p.file), pcache.clone(), read_limits),
            )?;
            parts.push(Arc::new(opened));
        }

        // A part file or fold generation no manifest names was written by a flush, merge, or
        // re-fold that crashed before committing, or has aged out of the retention window. Either
        // way it is unreachable. Safe to unlink even with readers attached: Unix keeps their open
        // mappings alive.
        sweep_unreachable_with_limits(dir, read_limits)?;
        // Crash litter: builder spools and staging files are all *.tmp, and every one of them is
        // pre-commit garbage. Swept ONLY at writer open, not at flush — an external packer's
        // staging file must not race a live writer's flush.
        // Transient names in the directory: staging files by exact grammar, pending publishes
        // anchored to a valid final name, retained copies past the window — removed, and a
        // removal that fails is this open's error with the path and the cause.
        let debris_removed = debris::remove_in_dir_layout(dir, read_limits)?;

        let wal_path = dir.join("WAL");
        let retained_commit_count = list_retained_with_limits(dir, read_limits)?.len();
        let replay = Wal::replay_state_with_limits(&wal_path, read_limits)?;
        let recovered_wal_frames = u64::try_from(replay.frames.len()).unwrap_or(u64::MAX);
        let physical_wal_frames = replay.physical_frames;
        let valid_wal_bytes = replay.valid_bytes;
        let mut mem: BTreeMap<String, Option<Record>> = BTreeMap::new();
        let mut mem_bytes = 0usize;
        for f in replay.frames {
            // Re-fold every piece this frame introduced. Content already below the committed tail
            // dedups; content discarded by the truncate is written again.
            for (h, bytes) in &f.novel {
                let put = fold.put_hashed(bytes, *h)?;
                debug_assert_eq!(put.hash, *h);
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
        metrics.open_recovery.observe_success(recovery_duration);
        let mut events = crate::observability::EventJournal::default();
        let recovery_result: Result<()> = Ok(());
        events.observe(
            crate::observability::LifecycleOperation::OpenRecovery,
            recovery_duration,
            &recovery_result,
        );
        Ok(Store {
            home: Home::Dir(dir.to_path_buf()),
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
        })
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

    fn admit_store_directory_growth(&self, additional: u64, operation: &str) -> Result<()> {
        let Home::Dir(dir) = &self.home else {
            // A single-file store has no directory to fill; member growth is admitted by the
            // container's own ceilings at staging time.
            return Ok(());
        };
        let current = count_directory_entries(dir, self.read_limits, "store directory")?;
        self.read_limits.admit_directory_entries(
            format!("store directory during {operation}"),
            current.saturating_add(additional),
        )?;
        Ok(())
    }

    /// A live part's on-disk size, wherever it lives: file metadata in a directory store, the
    /// member's logical length in a single-file one.
    fn part_file_bytes(&self, file: &str) -> Result<u64> {
        match &self.home {
            Home::Dir(dir) => Ok(crate::vfs::metadata(&dir.join(file))
                .with_context(|| format!("measure live part {file}"))?
                .len()),
            Home::File { container, .. } => container
                .lock()
                .expect("container lock poisoned")
                .member_len(file)
                .ok_or_else(|| {
                    anyhow::anyhow!("MANIFEST names part {file} but the container does not hold it")
                }),
        }
    }

    /// Where filesystem capacity questions are asked: the store directory, or the directory the
    /// store file lives in.
    fn fs_probe_path(&self) -> &Path {
        match &self.home {
            Home::Dir(dir) => dir,
            Home::File { path, .. } => path.parent().unwrap_or_else(|| Path::new(".")),
        }
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
            LifecycleOperation::OpenRecovery => {
                self.metrics.open_recovery.observe(duration, result)
            }
            LifecycleOperation::Sync => self.metrics.sync.observe(duration, result),
            LifecycleOperation::Flush => self.metrics.flush.observe(duration, result),
            LifecycleOperation::Compaction => self.metrics.compaction.observe(duration, result),
            LifecycleOperation::Backup => self.metrics.backup.observe(duration, result),
            LifecycleOperation::Verification => self.metrics.verification.observe(duration, result),
            LifecycleOperation::Punch => self.metrics.punch.observe(duration, result),
            LifecycleOperation::Refold => self.metrics.refold.observe(duration, result),
            LifecycleOperation::Erase => self.metrics.erase.observe(duration, result),
            LifecycleOperation::FormatMigration => {
                self.metrics.format_migration.observe(duration, result)
            }
        }
        self.events.observe(operation, duration, result);
    }

    /// Verify the retained manifest chain, all live immutable-part sections, and every fold frame.
    ///
    /// This covers the committed snapshot only. A writer that wants the report to include staged
    /// work must sync and flush first. Failures are classified at this integrity boundary and
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
        let chain = verification_integrity(
            "verify retained manifest chain",
            match &self.home {
                Home::Dir(dir) => verify_chain_dir(dir, self.read_limits, control),
                Home::File { container, .. } => {
                    let c = container.lock().expect("container lock poisoned");
                    verify_chain_container(&c, control)
                }
            },
        )?;
        let fold =
            verification_integrity("verify fold frames", self.fold.scrub_with_control(control))?;
        let mut part_sections = 0usize;
        for part in &self.parts {
            control.check("store verification")?;
            let sections = verification_integrity(
                "verify immutable part sections",
                part.verify_sections_with_control(control),
            )?;
            part_sections = part_sections
                .checked_add(sections)
                .ok_or_else(|| anyhow::anyhow!("verified part section count overflow"))?;
        }
        // Reconstruct every named value in the committed snapshot. Section and frame checks prove
        // the storage containers; this proves the references inside them resolve to the exact
        // content identities the records claim. Deliberately use the committed read core rather
        // than `Store::ids`/`Store::reconstruct_content`, which include staged memtable state.
        let ids = verification_integrity("enumerate committed records", read::ids(&self.parts))?;
        let mut content_values = 0usize;
        let mut content_bytes = 0u64;
        let mut content_identities = 0usize;
        let mut unidentified_content_values = 0usize;
        for id in &ids {
            control.check("store verification")?;
            let record =
                verification_integrity("decode committed record", read::get(&self.parts, id))?
                    .ok_or_else(|| {
                        anyhow::Error::new(crate::error::IntegrityError::new(
                            "decode committed record",
                            anyhow::anyhow!("live id {id:?} disappeared during verification"),
                        ))
                    })?;
            for content in &record.contents {
                control.check("store verification")?;
                let bytes = verification_integrity(
                    "reconstruct committed content",
                    read::reconstruct_content(&self.parts, &self.fold, id, &content.name),
                )?
                .ok_or_else(|| {
                    anyhow::Error::new(crate::error::IntegrityError::new(
                        "reconstruct committed content",
                        anyhow::anyhow!(
                            "record {id:?} lost named content {:?} during verification",
                            content.name
                        ),
                    ))
                })?;
                content_values = content_values
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("verified content value count overflow"))?;
                content_bytes = content_bytes
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("verified content byte count overflow"))?;
                match content.identity {
                    Some(want) => {
                        let got = crate::types::ContentHash::of(&bytes);
                        if got != want {
                            return Err(crate::error::IntegrityError::new(
                                "verify committed content identity",
                                anyhow::anyhow!(
                                    "record {id:?} content {:?} hashes to {got} but claims {want}",
                                    content.name
                                ),
                            )
                            .into());
                        }
                        content_identities =
                            content_identities.checked_add(1).ok_or_else(|| {
                                anyhow::anyhow!("verified content identity count overflow")
                            })?;
                    }
                    None => {
                        unidentified_content_values =
                            unidentified_content_values.checked_add(1).ok_or_else(|| {
                                anyhow::anyhow!("unidentified content value count overflow")
                            })?;
                    }
                }
            }
        }
        Ok(StoreVerification {
            chain,
            fold,
            parts: self.parts.len(),
            part_sections,
            records: ids.len(),
            content_values,
            content_bytes,
            content_identities,
            unidentified_content_values,
        })
    }

    /// Exact file-size and physical-row distribution for the current live immutable parts.
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
            let size = self.part_file_bytes(&part.file)?;
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

    /// Inspect exact live, dead, and block-reclaimable folded content for a settled snapshot.
    ///
    /// This walks visible record programs and reads each fold block header, but never decompresses
    /// content. A flushed memtable is required so unpublished references cannot make dead content
    /// appear safe to reclaim.
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
        let live_pieces = self.live_fold_pieces_with_control(control)?;
        let live_block_ids: HashSet<u32> =
            live_pieces.values().map(|location| location.block_id).collect();
        let mut report = crate::observability::ContentLiveness {
            live_pieces: u64::try_from(live_pieces.len())
                .map_err(|_| anyhow::anyhow!("live piece count exceeds u64"))?,
            ..crate::observability::ContentLiveness::default()
        };
        for location in live_pieces.values() {
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
    ///   Tier 1   every live part's dictionary  — everything ever committed, filter then search
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
    /// dictionary already sits below the tail that recovery truncates to. The bytes cannot be the ones
    /// a crash discards.
    fn fold_piece(&mut self, b: &[u8]) -> Result<crate::fold::Put> {
        let hash = PieceHash::of(b);
        let result = if let Some(loc) = self.locate(&hash)? {
            // Seed the window so further references in this flush interval answer from memory.
            self.fold.note(hash, loc);
            crate::fold::Put { hash, loc, deduped: true }
        } else {
            self.fold.put_hashed(b, hash)?
        };
        self.metrics.folded_content.observe(b.len(), result.deduped);
        Ok(result)
    }

    /// Fold the spans, log the record, and stage it. Durable only after [`Store::sync`].
    pub fn put(&mut self, id: &str, spans: &[Span], attrs: Vec<(String, AttrValue)>) -> Result<()> {
        let input = [ContentSpans::new(BODY_CONTENT, spans.to_vec())];
        input_record_admission_bytes(
            id,
            &input,
            &attrs,
            self.write_limits,
            self.read_limits,
            None,
        )?;
        self.wal.admit_additional_frames(1)?;
        let mut novel = Vec::new();
        let body = self.fold_spans(BODY_CONTENT, spans, &mut novel)?;
        let rec = Record::new(id, vec![body], attrs)?;
        self.stage_record(rec, novel)
    }

    /// Fold, log, and stage a general record with independently named content values.
    pub fn put_record(
        &mut self,
        id: &str,
        contents: &[ContentSpans<'_>],
        attrs: Vec<(String, AttrValue)>,
    ) -> Result<()> {
        // Validate and meter the whole map before `fold_spans` can append anything to the fold.
        input_record_admission_bytes(
            id,
            contents,
            &attrs,
            self.write_limits,
            self.read_limits,
            None,
        )?;
        self.wal.admit_additional_frames(1)?;
        let mut novel = Vec::new();
        let mut carved = Vec::with_capacity(contents.len());
        for content in contents {
            carved.push(self.fold_spans(content.name, &content.spans, &mut novel)?);
        }
        let rec = Record::new(id, carved, attrs)?;
        self.stage_record(rec, novel)
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
                    let put = self.fold_piece(b)?;
                    if !put.deduped {
                        // new content: the log must carry the bytes, because recovery discards
                        // anything the fold wrote past the committed tail
                        novel.push((put.hash, b.to_vec()));
                    }
                    ops.push(BodyOp::Piece { hash: put.hash, len: b.len() as u32 });
                }
            }
        }
        Ok(Content::identified(name, ops, ContentHash(identity.finalize().into())))
    }

    fn stage_record(&mut self, rec: Record, novel: Vec<(PieceHash, Vec<u8>)>) -> Result<()> {
        self.wal.append(self.manifest.next_seq, &rec, &novel)?;
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
    /// plus the commit marker goes to the log in one append. Replay applies the members only when
    /// the marker sealed them, so a crash anywhere inside this call replays nothing of the batch.
    /// (Content the fold gathered for an unreplayed batch is beyond the committed tail and is
    /// truncated at open, exactly like content from an unsynced put.)
    ///
    /// Durability is unchanged: the batch is ACKed by [`Store::sync`], like everything else.
    /// Within the batch, later members win over earlier ones on the same id, exactly as two puts
    /// would.
    pub fn apply(&mut self, batch: Batch) -> Result<()> {
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
        self.wal.admit_additional_frames(batch.items.len() as u64 + 1)?;
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
        self.wal.append_batch(self.manifest.next_seq, &framed)?;
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
        delete_admission_bytes(id, self.write_limits, self.read_limits, None)?;
        self.wal.admit_additional_frames(1)?;
        self.wal.append_tomb(self.manifest.next_seq, id)?;
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
    /// Once WAL fsync begins, cancellation is no longer observed: returning cancellation after the
    /// writes became durable would misreport the acknowledgement outcome.
    pub fn sync_with_control(&mut self, control: &crate::control::OperationControl) -> Result<()> {
        let started = std::time::Instant::now();
        let result = (|| {
            control.check("store sync")?;
            self.wal.sync()
        })();
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::Sync,
            started.elapsed(),
            &result,
        );
        result
    }

    /// Settle every accepted operation and publish a verified, immutable backup artifact.
    ///
    /// The destination must not exist. Holding `&mut self` prevents this process from changing the
    /// manifest while the packer walks the files it names — that part holds everywhere. Excluding a
    /// second writer *process* is the writer lock's job, and the lock is enforced only on Unix: on
    /// `wasm32-wasip1` it gates nothing, so a concurrent writer is admitted and the artifact may be
    /// a racing cut. See [the writer lock](https://github.com/turndb/turndb/blob/main/FORMAT.md#the-writer-lock).
    pub fn backup(&mut self, out: &Path) -> Result<crate::pack::BackupStats> {
        self.backup_with_control(out, &crate::control::OperationControl::default())
    }

    /// [`Store::backup`] with cooperative cancellation before atomic artifact publication.
    ///
    /// Sync/flush may publish an equivalent immutable representation of earlier accepted writes in
    /// the source store. Cancellation never publishes the backup destination.
    pub fn backup_with_control(
        &mut self,
        out: &Path,
        control: &crate::control::OperationControl,
    ) -> Result<crate::pack::BackupStats> {
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
    ) -> Result<crate::pack::BackupStats> {
        control.check("backup")?;
        crate::pack::ensure_destination_available(out)?;
        self.sync_with_control(control)?;
        self.flush_with_control(control)?;
        control.check("backup")?;
        match &self.home {
            // A backup of a single-file store IS a sealed container: the members a pack would
            // carry — MANIFEST, the live parts, the live generation — one aligned extent each,
            // flagged final, verified whole, and published only with a no-replace rename.
            Home::File { container, .. } => {
                seal_container_copy(container, &self.manifest, out, control)
            }
            // The one directory session left is the converter's, and its whole life is settle,
            // checkpoint, close. The pack writer is gone; converting IS the backup of this layout.
            Home::Dir(dir) => bail!(
                "{} is a retired directory layout; convert it to a single-file store first",
                dir.display()
            ),
        }
    }

    /// Seal the memtable into a part and commit it.
    ///
    /// Data before pointers, and the manifest last: the fold is durable before a part names any of
    /// it, and the part is durable before the manifest names the part.
    pub fn flush(&mut self) -> Result<Option<PartRef>> {
        self.flush_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::flush`] with cooperative checks before manifest publication.
    ///
    /// Fold sync may make accepted content bytes durable before a later cancellation, but the live
    /// manifest and memtable remain unchanged. An unpublished part is removed. Once manifest commit
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
        control.check("memtable flush")?;
        if self.mem.is_empty() {
            return Ok(None);
        }
        self.admit_store_directory_growth(3, "memtable flush")?;
        let tail = self.fold.sync()?;
        let seq = self.manifest.next_seq + 1;
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
                    recs.push(Record { id: id.clone(), contents: Vec::new(), attrs: Vec::new() });
                    tombs.push(true);
                }
            }
        }

        // Resolve every referenced piece through BOTH tiers, exactly as the write path does.
        //
        // A Tier-0-only resolve is correct only while the process that staged the records is still
        // alive: `fold_piece` notes a Tier-1 hit into the window, so the window covers it. After a
        // CRASH it does not. Replay re-folds only pieces the WAL carried bytes for, and a Tier-1 hit
        // carries none by design — the content was already durable in an older part. Those pieces are
        // then in no window at all, and a Tier-0-only resolve would fail here on every subsequent
        // flush attempt, permanently: records unreadable, WAL growing without bound. On the
        // high-duplication corpora this engine exists for, that is nearly every record after the
        // first flush.
        let mut locs: HashMap<PieceHash, Loc> = HashMap::new();
        for r in &recs {
            control.check("memtable flush planning")?;
            for content in &r.contents {
                for op in &content.ops {
                    control.check("memtable flush planning")?;
                    let BodyOp::Piece { hash, .. } = op else { continue };
                    if locs.contains_key(hash) {
                        continue;
                    }
                    let loc = self.locate(hash)?.ok_or_else(|| {
                        anyhow::anyhow!(
                            "staged piece {hash} is in neither the fold window nor any live part"
                        )
                    })?;
                    locs.insert(*hash, loc);
                }
            }
        }
        // From here the two homes diverge only in PLACEMENT and PUBLICATION: what a part is,
        // what the manifest says, and what the memtable becomes are identical either way.
        let (meta, digest, opened) = match &self.home {
            Home::Dir(dir) => {
                let path = dir.join(&file);
                let meta = match part::build_full_with_limits(
                    &path,
                    &recs,
                    &tombs,
                    seq,
                    seq,
                    self.cfg.level,
                    |h| locs.get(h).copied(),
                    &HashMap::new(),
                    self.read_limits,
                ) {
                    Ok(meta) => meta,
                    Err(error) => {
                        let _ = crate::vfs::unlink(&path);
                        return Err(error);
                    }
                };
                if let Err(error) = control.check("memtable flush") {
                    let _ = crate::vfs::unlink(&path);
                    return Err(error.into());
                }
                let digest = match hash_file_with_control(&path, control, "memtable flush") {
                    Ok(hash) => hash.to_hex().to_string(),
                    Err(error) => {
                        let _ = crate::vfs::unlink(&path);
                        return Err(error);
                    }
                };
                if let Err(error) = control.check("memtable flush publication") {
                    let _ = crate::vfs::unlink(&path);
                    return Err(error.into());
                }
                let opened =
                    Part::open_in_with_limits(&path, self.pcache.clone(), self.read_limits)?;
                (meta, digest, opened)
            }
            Home::File { container, .. } => {
                // The part assembles straight into the live file as a member, pinned in the same
                // pass that writes it — no named file, no rename, no second read for the hash. A
                // failure abandons the member write: its bytes are uncommitted noise past the
                // tail, released for whatever stages next.
                let member =
                    container.lock().expect("container lock poisoned").begin_member(&file)?;
                let built = part::build_full_into(
                    member,
                    &recs,
                    &tombs,
                    seq,
                    seq,
                    self.cfg.level,
                    |h| locs.get(h).copied(),
                    &HashMap::new(),
                    self.read_limits,
                );
                let (meta, member) = match built {
                    Ok(v) => v,
                    Err(error) => {
                        container.lock().expect("container lock poisoned").abandon_open_member();
                        return Err(error);
                    }
                };
                if let Err(error) = control.check("memtable flush publication") {
                    container.lock().expect("container lock poisoned").abandon_open_member();
                    return Err(error.into());
                }
                let digest = {
                    let mut c = container.lock().expect("container lock poisoned");
                    PieceHash(c.finish_member(member)?).to_hex()
                };
                let reader = container
                    .lock()
                    .expect("container lock poisoned")
                    .extent(&file)
                    .expect("the member was staged a moment ago");
                let opened = Part::open_reader_with_limits(
                    Box::new(reader),
                    self.pcache.clone(),
                    self.read_limits,
                )?;
                (meta, digest, opened)
            }
        };

        let mut m = self.manifest.clone();
        m.parts.push(PartRef {
            file: file.clone(),
            seq_lo: seq,
            seq_hi: seq,
            records: meta.n_records,
            b3: Some(digest),
        });
        m.fold_seg = tail.seg;
        m.fold_off = tail.off;
        m.next_seq = seq;
        match &self.home {
            Home::Dir(dir) => {
                m.commit_with_limits(dir, self.read_limits)?; // <- the linearization point
                                                              // The commit may have pruned a retained manifest; whatever only it named is now
                                                              // sweepable.
                sweep_unreachable_with_limits(dir, self.read_limits)?;
            }
            Home::File { container, .. } => {
                let mut c = container.lock().expect("container lock poisoned");
                m.commit_into_container(&mut c)?;
                // The sweep stages its frees BEFORE the flip, so pruned-manifest space publishes
                // in the same atomic state that prunes it.
                sweep_unreachable_container(&mut c, &m, self.read_limits)?;
                c.commit()?; // <- the linearization point: one flip names everything above
            }
        }

        self.parts.push(Arc::new(opened));
        self.manifest = m;
        self.note_manifest_commit();
        self.mem.clear();
        self.mem_bytes = 0;
        // Release Tier 0 — but only HERE, after the part is committed and open. Sealing any earlier
        // would drop the window while the part being built still needs it, and the part cannot answer
        // a Tier-1 lookup until it is committed and in `self.parts`. Everything the window covered is
        // now reachable through that part's dictionary, so nothing is lost but the memory.
        self.fold.seal_window();
        // Only now: the records are in a committed part, so the log that carried them is redundant.
        self.wal.truncate()?;
        Ok(self.manifest.parts.last().cloned())
    }

    /// Merge a CONTIGUOUS run of live parts into one, and publish it atomically.
    ///
    /// Contiguity is the correctness gate: parts resolve versions by sequence, so merging a
    /// non-adjacent set would drop whatever an excluded part said about a shared id. The range is
    /// therefore expressed as a slice of the live list, which cannot express a gap.
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
            crate::observability::LifecycleOperation::Compaction,
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
        control.check("part compaction")?;
        if len < 2 || lo + len > self.parts.len() {
            return Ok(None);
        }
        self.admit_store_directory_growth(3, "part compaction")?;
        let inputs: Vec<Arc<Part>> = self.parts[lo..lo + len].to_vec();
        // Named by the sequence RANGE it spans. The output's range strictly contains every input's
        // (the inputs are disjoint and there are at least two), so the name cannot collide with a part
        // this merge is about to replace — which the post-commit sweep would otherwise unlink.
        let seq_lo = self.manifest.parts[lo].seq_lo;
        let seq_hi = self.manifest.parts[lo + len - 1].seq_hi;
        let file = format!("part-{seq_lo:08}-{seq_hi:08}.part");
        debug_assert!(
            !self.manifest.parts.iter().any(|p| p.file == file),
            "merge output {file} collides with a live part"
        );
        // A tombstone may only be discarded when this merge covers the ENTIRE live list — otherwise a
        // part outside the run could still hold an older version of the deleted id, and dropping the
        // tombstone would resurrect it.
        let total = lo == 0 && len == self.parts.len();
        // Publish: the merged part is durable before the manifest names it — the part fsync in a
        // directory, the pre-flip barrier in a single file — and the manifest swap is the single
        // linearization point. A crash before it leaves the merged output unreachable: an orphan
        // file, or uncommitted noise past the tail. The INPUTS are not deleted here: retained
        // manifests still name them, so a reader inside the retention window keeps a complete
        // snapshot. They fall to the sweep when the window prunes past their last naming manifest.
        // Every fallible preparation step and the final cancellation checkpoint happen before
        // commit is attempted. Once commit starts, its ordinary crash protocol—not cancellation—
        // decides the outcome.
        let (meta, stats, digest, opened) = match &self.home {
            Home::Dir(dir) => {
                let path = dir.join(&file);
                let (meta, stats) = match crate::part::merge::merge_opts_with_control_and_limits(
                    &path,
                    &inputs,
                    self.cfg.level,
                    total,
                    control,
                    self.read_limits,
                ) {
                    Ok(built) => built,
                    Err(error) => {
                        let _ = crate::vfs::unlink(&path);
                        return Err(error);
                    }
                };
                let digest =
                    match hash_file_with_control(&path, control, "part compaction output hashing")
                        .map(|hash| hash.to_hex().to_string())
                    {
                        Ok(digest) => digest,
                        Err(error) => {
                            let _ = crate::vfs::unlink(&path);
                            return Err(error);
                        }
                    };
                if let Err(error) = control.check("part compaction") {
                    let _ = crate::vfs::unlink(&path);
                    return Err(error.into());
                }
                let opened =
                    Part::open_in_with_limits(&path, self.pcache.clone(), self.read_limits)?;
                (meta, stats, digest, opened)
            }
            Home::File { container, path } => {
                // Spools live in the transient scratch directory beside the store; the merged
                // part streams straight into the live file as a member, pinned in-pass.
                let tmp = file_tmp_dir(path);
                crate::vfs::mkdir_all(&tmp)?;
                let member =
                    container.lock().expect("container lock poisoned").begin_member(&file)?;
                let built = crate::part::merge::merge_into_with_control_for_operation(
                    member,
                    &tmp.join("m"),
                    &inputs,
                    self.cfg.level,
                    total,
                    control,
                    "part compaction",
                    self.read_limits,
                );
                let (meta, stats, member) = match built {
                    Ok(v) => v,
                    Err(error) => {
                        container.lock().expect("container lock poisoned").abandon_open_member();
                        let _ = crate::vfs::remove_tree(&tmp);
                        return Err(error);
                    }
                };
                if let Err(error) = control.check("part compaction") {
                    container.lock().expect("container lock poisoned").abandon_open_member();
                    let _ = crate::vfs::remove_tree(&tmp);
                    return Err(error.into());
                }
                let digest = {
                    let mut c = container.lock().expect("container lock poisoned");
                    PieceHash(c.finish_member(member)?).to_hex()
                };
                let _ = crate::vfs::remove_tree(&tmp);
                let reader = container
                    .lock()
                    .expect("container lock poisoned")
                    .extent(&file)
                    .expect("the member was staged a moment ago");
                let opened = Part::open_reader_with_limits(
                    Box::new(reader),
                    self.pcache.clone(),
                    self.read_limits,
                )?;
                (meta, stats, digest, opened)
            }
        };
        let mut m = self.manifest.clone();
        m.parts.splice(
            lo..lo + len,
            [PartRef {
                file: file.clone(),
                seq_lo: meta.seq_lo,
                seq_hi: meta.seq_hi,
                records: meta.n_records,
                b3: Some(digest),
            }],
        );
        match &self.home {
            Home::Dir(dir) => {
                m.commit_with_limits(dir, self.read_limits)?;
                sweep_unreachable_with_limits(dir, self.read_limits)?;
            }
            Home::File { container, .. } => {
                // A flip publishes EVERYTHING staged — including fold-delta extents appended by
                // puts since the last flush. The manifest must therefore claim the tail the flip
                // is about to make durable, or the committed member outgrows the manifest's
                // claim and the next open refuses the disagreement. (The reopen check that
                // demands this agreement is what caught the omission.)
                let t = self.fold.sync()?;
                m.fold_seg = t.seg;
                m.fold_off = t.off;
                let mut c = container.lock().expect("container lock poisoned");
                m.commit_into_container(&mut c)?;
                sweep_unreachable_container(&mut c, &m, self.read_limits)?;
                c.commit()?; // <- the linearization point
            }
        }
        self.manifest = m;
        self.note_manifest_commit();
        self.parts.splice(lo..lo + len, [Arc::new(opened)]);
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
    /// no tombstone settlement. Total merges are also the only ones allowed to drop tombstones,
    /// so deletes actually settle instead of shadowing forever.
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
                let bytes = self.part_file_bytes(&part.file)?;
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

    /// Estimate temporary output space for the current bounded-compaction plan.
    ///
    /// Input file lengths, section counts/raw bytes, retained-input bytes, and filesystem
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
        control.check("bounded compaction")?;
        let Some(plan) = self.plan_compaction(budget)? else {
            return Ok(None);
        };
        let merge = self
            .merge_range_with_control(plan.start_part, plan.input_parts, control)?
            .expect("a compaction plan always contains at least two parts");
        let output = &self.manifest.parts[plan.start_part];
        let output_bytes = self.part_file_bytes(&output.file)?;
        Ok(Some(BoundedCompaction { plan, output_bytes, merge }))
    }

    /// Report exact live-part format migration progress without decoding rows or content.
    pub fn format_migration_status(&self) -> Result<FormatMigrationStatus> {
        self.format_migration_status_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::format_migration_status`] with cooperative part checkpoints.
    pub fn format_migration_status_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<FormatMigrationStatus> {
        control.check("format migration status")?;
        let mut status = FormatMigrationStatus {
            target_part_version: crate::part::PART_VERSION,
            live_parts: self.parts.len(),
            ..FormatMigrationStatus::default()
        };
        for (part, part_ref) in self.parts.iter().zip(&self.manifest.parts) {
            control.check("format migration status")?;
            if part.format_version() == crate::part::PART_VERSION {
                status.current_parts = status
                    .current_parts
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("current format part count overflow"))?;
                continue;
            }
            status.legacy_parts = status
                .legacy_parts
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("legacy format part count overflow"))?;
            status.legacy_rows = status
                .legacy_rows
                .checked_add(u64::from(part_ref.records))
                .ok_or_else(|| anyhow::anyhow!("legacy format row count overflow"))?;
            status.legacy_bytes = status
                .legacy_bytes
                .checked_add(self.part_file_bytes(&part_ref.file)?)
                .ok_or_else(|| anyhow::anyhow!("legacy format byte count overflow"))?;
        }
        let live_files: HashSet<&str> =
            self.manifest.parts.iter().map(|part| part.file.as_str()).collect();
        let mut retained_seen = HashSet::new();
        let retained_manifests: Vec<(u64, Manifest)> = match &self.home {
            Home::Dir(dir) => {
                let mut v = Vec::new();
                for commit in list_retained_with_limits(dir, self.read_limits)? {
                    control.check("format migration status")?;
                    v.push((
                        commit,
                        load_retained(dir, commit).with_context(|| {
                            format!("inspect migration state at retained commit {commit}")
                        })?,
                    ));
                }
                v
            }
            Home::File { container, .. } => {
                let c = container.lock().expect("container lock poisoned");
                let mut v = Vec::new();
                for commit in container_retained_commits(&c) {
                    control.check("format migration status")?;
                    let bytes = c
                        .read_file_bounded(&format!("MANIFEST.{commit:08}"), MAX_MANIFEST_BYTES)
                        .with_context(|| {
                            format!("inspect migration state at retained commit {commit}")
                        })?;
                    v.push((
                        commit,
                        Manifest::parse(&bytes).with_context(|| {
                            format!("inspect migration state at retained commit {commit}")
                        })?,
                    ));
                }
                v
            }
        };
        for (_, manifest) in retained_manifests {
            for part_ref in manifest.parts {
                control.check("format migration status")?;
                if live_files.contains(part_ref.file.as_str())
                    || !retained_seen.insert(part_ref.file.clone())
                {
                    continue;
                }
                let part = match &self.home {
                    Home::Dir(dir) => Part::open_in_with_limits(
                        &dir.join(&part_ref.file),
                        self.pcache.clone(),
                        self.read_limits,
                    )?,
                    Home::File { container, .. } => {
                        let reader = container
                            .lock()
                            .expect("container lock poisoned")
                            .extent(&part_ref.file)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "retained manifest names {} but the container does not hold it",
                                    part_ref.file
                                )
                            })?;
                        Part::open_reader_with_limits(
                            Box::new(reader),
                            self.pcache.clone(),
                            self.read_limits,
                        )?
                    }
                };
                if part.format_version() == crate::part::PART_VERSION {
                    continue;
                }
                status.retained_legacy_parts = status
                    .retained_legacy_parts
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("retained legacy part count overflow"))?;
                status.retained_legacy_rows = status
                    .retained_legacy_rows
                    .checked_add(u64::from(part_ref.records))
                    .ok_or_else(|| anyhow::anyhow!("retained legacy row count overflow"))?;
                status.retained_legacy_bytes = status
                    .retained_legacy_bytes
                    .checked_add(self.part_file_bytes(&part_ref.file)?)
                    .ok_or_else(|| anyhow::anyhow!("retained legacy byte count overflow"))?;
            }
        }
        Ok(status)
    }

    /// Preflight the oldest remaining live legacy part for one resumable migration step.
    pub fn estimate_format_migration_space(&self) -> Result<Option<FormatMigrationPlan>> {
        self.estimate_format_migration_space_with_control(
            &crate::control::OperationControl::default(),
        )
    }

    /// [`Store::estimate_format_migration_space`] with cooperative section checkpoints.
    pub fn estimate_format_migration_space_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<Option<FormatMigrationPlan>> {
        control.check("format migration preflight")?;
        if !self.mem.is_empty() {
            bail!(
                "format migration preflight requires a flushed memtable; call sync() and flush() first"
            );
        }
        let Some(part_index) =
            self.parts.iter().position(|part| part.format_version() < crate::part::PART_VERSION)
        else {
            return Ok(None);
        };
        let part = &self.parts[part_index];
        let part_ref = &self.manifest.parts[part_index];
        let input_bytes = self.part_file_bytes(&part_ref.file)?;
        let mut input_sections = 0usize;
        let mut input_raw_section_bytes = 0u64;
        for (_, _, raw, _) in part.sections() {
            control.check("format migration preflight")?;
            input_sections = input_sections
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("migration section count overflow"))?;
            input_raw_section_bytes = input_raw_section_bytes
                .checked_add(u64::from(raw))
                .ok_or_else(|| anyhow::anyhow!("migration raw section byte count overflow"))?;
        }
        let input_rows = u64::from(part_ref.records);
        let row_allowance = input_rows
            .checked_mul(64)
            .ok_or_else(|| anyhow::anyhow!("migration row framing estimate overflow"))?;
        let section_allowance = u64::try_from(input_sections)
            .map_err(|_| anyhow::anyhow!("migration section count exceeds u64"))?
            .checked_mul(256)
            .ok_or_else(|| anyhow::anyhow!("migration section framing estimate overflow"))?;
        let estimated_stage_bytes = input_raw_section_bytes
            .checked_add(row_allowance)
            .and_then(|bytes| bytes.checked_add(section_allowance))
            .and_then(|bytes| bytes.checked_add(1 << 20))
            .ok_or_else(|| anyhow::anyhow!("migration stage estimate overflow"))?;
        Ok(Some(FormatMigrationPlan {
            part_index,
            source_part_version: part.format_version(),
            seq_lo: part_ref.seq_lo,
            seq_hi: part_ref.seq_hi,
            input_rows,
            input_bytes,
            input_sections,
            input_raw_section_bytes,
            estimated_stage_bytes,
            estimate_is_hard_bound: false,
            retained_input_bytes_after_commit: input_bytes,
            filesystem_available_bytes: crate::sys::filesystem_available_bytes(
                self.fs_probe_path(),
            )
            .with_context(|| {
                format!("measure available filesystem bytes at {}", self.fs_probe_path().display())
            })?,
        }))
    }

    /// Atomically rewrite the oldest remaining live legacy part in the current format.
    ///
    /// Each call is one durable progress unit. Cancellation removes its unpublished output; after
    /// publication, reopening observes the migrated part and a later call resumes with the next.
    /// Content bytes are not rewritten, and unavailable legacy identities remain unavailable.
    pub fn migrate_format_step(&mut self) -> Result<Option<FormatMigrationStep>> {
        self.migrate_format_step_with_control(&crate::control::OperationControl::default())
    }

    /// [`Store::migrate_format_step`] with cooperative checkpoints before publication.
    pub fn migrate_format_step_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<Option<FormatMigrationStep>> {
        let started = std::time::Instant::now();
        let result = self.migrate_format_step_inner_with_control(control);
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::FormatMigration,
            started.elapsed(),
            &result,
        );
        result
    }

    fn migrate_format_step_inner_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<Option<FormatMigrationStep>> {
        let Some(plan) = self.estimate_format_migration_space_with_control(control)? else {
            return Ok(None);
        };
        self.admit_store_directory_growth(3, "format migration")?;
        let input = self.parts[plan.part_index].clone();
        let file = format!(
            "part-mv{}-{:08}-{:08}.part",
            crate::part::PART_VERSION,
            plan.seq_lo,
            plan.seq_hi
        );
        let (meta, rewrite, digest, opened) = match &self.home {
            Home::Dir(dir) => {
                let path = dir.join(&file);
                if path.exists() {
                    bail!("format migration staging path already exists: {}", path.display());
                }
                let (meta, rewrite) =
                    match crate::part::merge::merge_opts_with_control_for_operation(
                        &path,
                        &[input],
                        self.cfg.level,
                        false,
                        control,
                        "format migration",
                        self.read_limits,
                    ) {
                        Ok(built) => built,
                        Err(error) => {
                            let _ = crate::vfs::unlink(&path);
                            return Err(error);
                        }
                    };
                let digest = match hash_file_with_control(&path, control, "format migration") {
                    Ok(hash) => hash.to_hex().to_string(),
                    Err(error) => {
                        let _ = crate::vfs::unlink(&path);
                        return Err(error);
                    }
                };
                if let Err(error) = control.check("format migration publication") {
                    let _ = crate::vfs::unlink(&path);
                    return Err(error.into());
                }
                let opened =
                    Part::open_in_with_limits(&path, self.pcache.clone(), self.read_limits)?;
                (meta, rewrite, digest, opened)
            }
            Home::File { container, path } => {
                // One live legacy part rewritten into a member and spliced with one flip — the
                // merge's placement discipline, applied to the migration's one-part rewrite.
                if container.lock().expect("container lock poisoned").contains(&file) {
                    bail!("format migration staging member already exists: {file}");
                }
                let tmp = file_tmp_dir(path);
                crate::vfs::mkdir_all(&tmp)?;
                let member =
                    container.lock().expect("container lock poisoned").begin_member(&file)?;
                let built = crate::part::merge::merge_into_with_control_for_operation(
                    member,
                    &tmp.join("m"),
                    &[input],
                    self.cfg.level,
                    false,
                    control,
                    "format migration",
                    self.read_limits,
                );
                let (meta, rewrite, member) = match built {
                    Ok(v) => v,
                    Err(error) => {
                        container.lock().expect("container lock poisoned").abandon_open_member();
                        let _ = crate::vfs::remove_tree(&tmp);
                        return Err(error);
                    }
                };
                if let Err(error) = control.check("format migration publication") {
                    container.lock().expect("container lock poisoned").abandon_open_member();
                    let _ = crate::vfs::remove_tree(&tmp);
                    return Err(error.into());
                }
                let digest = {
                    let mut c = container.lock().expect("container lock poisoned");
                    PieceHash(c.finish_member(member)?).to_hex()
                };
                let _ = crate::vfs::remove_tree(&tmp);
                let reader = container
                    .lock()
                    .expect("container lock poisoned")
                    .extent(&file)
                    .expect("the member was staged a moment ago");
                let opened = Part::open_reader_with_limits(
                    Box::new(reader),
                    self.pcache.clone(),
                    self.read_limits,
                )?;
                (meta, rewrite, digest, opened)
            }
        };
        let output_bytes = self.part_file_bytes(&file)?;
        let mut manifest = self.manifest.clone();
        manifest.parts[plan.part_index] = PartRef {
            file,
            seq_lo: meta.seq_lo,
            seq_hi: meta.seq_hi,
            records: meta.n_records,
            b3: Some(digest),
        };
        match &self.home {
            Home::Dir(dir) => {
                manifest.commit_with_limits(dir, self.read_limits)?;
                sweep_unreachable_with_limits(dir, self.read_limits)?;
            }
            Home::File { container, .. } => {
                // Same rule as the merge: the flip publishes staged fold deltas too, so the
                // manifest claims the tail it makes durable.
                let t = self.fold.sync()?;
                manifest.fold_seg = t.seg;
                manifest.fold_off = t.off;
                let mut c = container.lock().expect("container lock poisoned");
                manifest.commit_into_container(&mut c)?;
                sweep_unreachable_container(&mut c, &manifest, self.read_limits)?;
                c.commit()?; // <- the step's publication: one flip, restartable as ever
            }
        }
        self.parts[plan.part_index] = Arc::new(opened);
        self.manifest = manifest;
        self.note_manifest_commit();
        let remaining_legacy_parts = self
            .parts
            .iter()
            .filter(|part| part.format_version() < crate::part::PART_VERSION)
            .count();
        Ok(Some(FormatMigrationStep { plan, output_bytes, remaining_legacy_parts, rewrite }))
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
        // this adds over the committed read core.
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
        if let Some(l) = self.fold.lookup(*h) {
            return Ok(Some(l));
        }
        for p in self.parts.iter().rev() {
            if let Some(l) = p.lookup_piece(h)? {
                return Ok(Some(l));
            }
        }
        Ok(None)
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
    /// of both; the manifest snapshot it was opened at is already baked into `parts`.
    pub fn into_parts(self) -> (Arc<Fold>, Vec<Arc<Part>>) {
        (Arc::new(self.fold), self.parts)
    }

    pub fn part_count(&self) -> usize {
        self.parts.len()
    }

    /// The live parts, oldest to newest — the writer-side twin of [`ReadStore::parts`].
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

    /// ERASE records: tombstone, settle, and rewrite until this store no longer references the
    /// records and its old logical files have been removed.
    ///
    /// This is the compliance path, and it composes three operations that each already existed:
    /// deletes shadow the ids; a TOTAL merge drops the tombstones once nothing remains for them
    /// to shadow; and the re-fold rewrites the fold without the dropped content and rebuilds
    /// every part — so both the bytes AND the columnar metadata (ids, piece lengths, attribute
    /// values) of the erased records are gone when this returns. The re-fold also purges the
    /// retained commit log, which the erasure story REQUIRES: a snapshot that could still serve
    /// the erased record is not erasure.
    ///
    /// What this does NOT promise, stated because overclaiming here is a liability: media-byte
    /// non-recoverability on arbitrary or copy-on-write filesystems, through WASI, or for copies
    /// already made (packs, replicas, backups). The measurable claims are query absence, logical
    /// file-length reclamation, and the lifecycle event for this operation.
    ///
    /// Ids that do not exist are counted, not errored: a DSAR naming already-gone data is a
    /// normal outcome, and the record should say so rather than fail.
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

    fn live_fold_pieces_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<HashMap<PieceHash, Loc>> {
        let visible = read::visibility(&self.parts)?;
        let mut pieces: HashMap<PieceHash, Loc> = HashMap::new();
        for (part_index, rows) in visible.rows.iter().enumerate() {
            for &row in rows {
                control.check("content reachability")?;
                for content in self.parts[part_index].record(row)?.contents {
                    for operation in content.ops {
                        let BodyOp::Piece { hash, len } = operation else { continue };
                        if let Some(existing) = pieces.get(&hash) {
                            if existing.raw != len {
                                bail!(
                                    "live piece {hash} has inconsistent lengths {} and {len}",
                                    existing.raw
                                );
                            }
                            continue;
                        }
                        let location = self.locate(&hash)?.ok_or_else(|| {
                            anyhow::anyhow!("live record references absent piece {hash}")
                        })?;
                        if location.raw != len {
                            bail!(
                                "live piece {hash} is {} bytes but its record says {len}",
                                location.raw
                            );
                        }
                        pieces.insert(hash, location);
                    }
                }
            }
        }
        Ok(pieces)
    }

    /// Reclaim erased space IN PLACE: punch every fold block no live record can reach.
    ///
    /// The cheap half of erasure, and the one a sealed store wants. A re-fold reclaims the same
    /// bytes by rewriting the world — correct, thorough, and O(store); this walks the live
    /// records' piece references, finds blocks nothing reaches, records them in the manifest, and
    /// deallocates their extents. Offsets do not move, so no part is rebuilt and no reader is
    /// disturbed.
    ///
    /// **Order matters and is the whole safety argument**: the manifest names the punched blocks
    /// BEFORE the bytes go, so a crash between the two leaves blocks marked punched that are
    /// still readable (harmless — the next call re-punches them), never punched blocks that
    /// nothing accounts for (an ops fire drill: zeros that look exactly like corruption).
    ///
    /// Requires a flushed memtable, for the same reason a re-fold does: staged records reference
    /// content this would otherwise consider unreachable.
    pub fn punch_unreferenced(&mut self) -> Result<PunchStats> {
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
    ) -> Result<PunchStats> {
        let started = std::time::Instant::now();
        let result = self.punch_unreferenced_inner_with_control(control);
        self.observe_lifecycle(
            crate::observability::LifecycleOperation::Punch,
            started.elapsed(),
            &result,
        );
        result
    }

    fn punch_unreferenced_inner_with_control(
        &mut self,
        control: &crate::control::OperationControl,
    ) -> Result<PunchStats> {
        control.check("content punching")?;
        if !self.mem.is_empty() {
            bail!("punching requires a flushed memtable; call sync() and flush() first");
        }
        // Every block a live record can still reach, via the piece dictionaries of live rows.
        let live_blocks: HashSet<u32> = self
            .live_fold_pieces_with_control(control)?
            .values()
            .map(|location| location.block_id)
            .collect();
        // ... against every block the fold holds.
        let mut dead: Vec<u32> =
            self.fold.block_ids().into_iter().filter(|b| !live_blocks.contains(b)).collect();
        dead.sort_unstable();
        let already: HashSet<u32> =
            self.manifest.punched.iter().flat_map(|&(lo, hi)| lo..=hi).collect();
        if dead.is_empty() {
            return Ok(PunchStats::default());
        }

        // Record first, punch second. Already-declared blocks stay in `dead`: a crash or
        // cancellation can land after this authority is durable but before every hole is punched,
        // and retrying those blocks is how the operation actually resumes.
        if dead.iter().any(|block| !already.contains(block)) {
            control.check("content punching")?;
            self.admit_store_directory_growth(2, "content punching")?;
            let mut m = self.manifest.clone();
            let mut all: Vec<u32> = already.into_iter().chain(dead.iter().copied()).collect();
            all.sort_unstable();
            m.punched = to_ranges(&all);
            match &self.home {
                Home::Dir(dir) => m.commit_with_limits(dir, self.read_limits)?,
                Home::File { container, .. } => {
                    let t = self.fold.sync()?;
                    m.fold_seg = t.seg;
                    m.fold_off = t.off;
                    let mut c = container.lock().expect("container lock poisoned");
                    m.commit_into_container(&mut c)?;
                    c.commit()?; // the declaration is durable BEFORE any byte is destroyed
                }
            }
            self.manifest = m;
            self.note_manifest_commit();
        }
        // Declare before destroying, the same order the manifest write follows and for the same
        // reason: at no point may a block's bytes be gone while this fold still calls it content.
        self.fold.declare_punched(&self.manifest.punched);

        let punched = self.fold.punch_blocks_with_control(&dead, control)?;
        Ok(PunchStats { blocks_punched: punched.len(), blocks_examined: dead.len() })
    }

    /// Rewrite the fold, keeping only content that live records still reference.
    ///
    /// The ONLY operation that touches content. Everything else asserts it does not, which is why this
    /// is a separate call rather than a flag: a reader of the merge path should never have to wonder.
    ///
    /// Requires a flushed memtable — staged records reference the old fold, and rebuilding parts under
    /// them would leave their pieces unresolvable.
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
                .checked_add(self.part_file_bytes(&part_ref.file)?)
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
    /// Cancellation removes the unpublished generation and rebuilt parts. Once manifest commit is
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
        control.check("content refold")?;
        if !self.mem.is_empty() {
            bail!("refold requires a flushed memtable; call sync() and flush() first");
        }
        if self.parts.is_empty() {
            return Ok(refold::RefoldStats::default());
        }
        if let Home::File { container, .. } = &self.home {
            let container = container.clone();
            return self.refold_in_file(container, control);
        }
        let dir = self.home.dir()?.to_path_buf();
        self.admit_store_directory_growth(self.parts.len() as u64 + 3, "content refold")?;
        let seqs: Vec<(u64, u64)> =
            self.manifest.parts.iter().map(|p| (p.seq_lo, p.seq_hi)).collect();
        let (new_gen, built, mut stats) = refold::refold_with_control_and_limits(
            self.home.dir()?,
            &self.parts,
            &seqs,
            &self.fold,
            self.manifest.fold_gen,
            self.cfg,
            control,
            self.read_limits,
        )?;

        // Data before pointers, exactly as everywhere else: the new fold and the new parts are durable
        // before the manifest names either, and the manifest swap is the instant it takes effect.
        let new_dir = refold::fold_dir(&dir, new_gen);
        let prepared = (|| -> Result<Manifest> {
            let mut m = self.manifest.clone();
            m.parts = built
                .iter()
                .map(|(file, lo, hi, n)| {
                    control.check("content refold")?;
                    Ok(PartRef {
                        file: file.clone(),
                        seq_lo: *lo,
                        seq_hi: *hi,
                        records: *n,
                        b3: Some(
                            hash_file_with_control(
                                &self.home.dir()?.join(file),
                                control,
                                "content refold output hashing",
                            )?
                            .to_hex()
                            .to_string(),
                        ),
                    })
                })
                .collect::<Result<_>>()?;
            m.fold_gen = new_gen;
            // Block ids are PER GENERATION. The new fold was rewritten without erased content and
            // therefore has no holes inherited from the old generation.
            m.punched.clear();
            let f = Fold::open_with_limits(&new_dir, self.cfg, self.read_limits)?;
            let t = f.tail();
            m.fold_seg = t.seg;
            m.fold_off = t.off;
            control.check("content refold")?;
            Ok(m)
        })();
        let mut m = match prepared {
            Ok(manifest) => manifest,
            Err(error) => {
                cleanup_refold_stage(&dir, new_gen, &built);
                return Err(error);
            }
        };
        // No cancellation checkpoint after this call begins. A failed commit can have durably
        // written its retained copy, so staged files must remain for ordinary recovery.
        m.commit_with_limits(self.home.dir()?, self.read_limits)?;

        // Everything past here is cleanup: a crash leaves orphans, which open() sweeps.
        let old_gen = self.manifest.fold_gen;
        self.manifest = m;
        // PURGE the retained log down to this commit alone. Erasure semantics trump snapshots: a
        // re-fold exists to make dropped content unreachable in this store, and a retained manifest would keep the old
        // generation — deleted records included — readable for MANIFEST_RETAIN more commits.
        // Time travel does not cross a re-fold, by design; that is the point of running one.
        //
        // The unlinks are durable and propagated, because they are the erasure claim, not window
        // pruning: a swallowed failure or a lost dirent would leave the old generation readable
        // through a name this method just promised was gone. The re-fold itself is already
        // committed when this runs, so an error here means "committed, but the purge is
        // incomplete — reopen the store", and reopening completes it: writer open durably removes
        // retained manifests from any other fold generation.
        for c in list_retained_with_limits(&dir, self.read_limits)? {
            if c != self.manifest.commit {
                crate::vfs::unlink(&retained_path(&dir, c)).with_context(|| {
                    format!(
                        "re-fold committed, but purging retained manifest {c} failed — reopen \
                         the store to complete the purge"
                    )
                })?;
            }
        }
        crate::vfs::sync_dir(self.home.dir()?)?;
        self.retained_commit_count = 1;
        let part_cache_budget = self.pcache.budget();
        self.pcache = Arc::new(SectionCache::new(part_cache_budget));
        self.parts.clear();
        for p in &self.manifest.parts {
            self.parts.push(Arc::new(Part::open_in_with_limits(
                &dir.join(&p.file),
                self.pcache.clone(),
                self.read_limits,
            )?));
        }
        self.fold = Fold::open_at_with_limits(
            &new_dir,
            self.cfg,
            self.manifest.fold_tail(),
            // Empty by construction — a new generation has no punched declaration, which is what
            // `m.punched.clear()` above committed.
            &self.manifest.punched,
            self.read_limits,
        )?;
        sweep_unreachable_with_limits(self.home.dir()?, self.read_limits)?;
        // Reported, not swallowed. Claiming `bytes_reclaimed()` while the old generation still
        // occupies the disk would be a stat that says the opposite of the truth. The re-fold itself
        // is already committed and correct; this is only honest about what is left behind.
        if refold::fold_dir(&dir, old_gen).exists() {
            stats.stale_generation_left = true;
        }
        Ok(stats)
    }

    /// The refold's single-file form. The build stages a whole new generation and its rebuilt
    /// parts as uncommitted members; publication is ONE flip carrying the swap, the retained-log
    /// purge, and the sweep's frees. The directory protocol's hardest ordering problem — a crash
    /// between the commit and the purge leaving erased content readable through a retained name —
    /// cannot occur: the purge IS part of the commit. Time travel does not cross a refold, here
    /// by construction rather than by propagated unlinks.
    fn refold_in_file(
        &mut self,
        container: std::sync::Arc<std::sync::Mutex<crate::container::Container>>,
        control: &crate::control::OperationControl,
    ) -> Result<refold::RefoldStats> {
        let seqs: Vec<(u64, u64)> =
            self.manifest.parts.iter().map(|p| (p.seq_lo, p.seq_hi)).collect();
        let (new_gen, built, mut stats, nf) =
            refold::refold_into_container_with_control_and_limits(
                container.clone(),
                &self.parts,
                &seqs,
                &self.fold,
                self.manifest.fold_gen,
                self.cfg,
                control,
                self.read_limits,
            )?;

        let mut m = self.manifest.clone();
        m.parts = built
            .iter()
            .map(|(file, lo, hi, n, b3)| PartRef {
                file: file.clone(),
                seq_lo: *lo,
                seq_hi: *hi,
                records: *n,
                b3: Some(b3.clone()),
            })
            .collect();
        m.fold_gen = new_gen;
        // Block ids are PER GENERATION; the new fold has no holes to declare.
        m.punched.clear();
        let t = nf.tail();
        m.fold_seg = t.seg;
        m.fold_off = t.off;
        if let Err(error) = control.check("content refold publication") {
            let _ = container.lock().expect("container lock poisoned").discard_staged();
            return Err(error.into());
        }
        {
            let mut c = container.lock().expect("container lock poisoned");
            if let Err(error) = m.commit_into_container(&mut c) {
                let _ = c.discard_staged();
                return Err(error);
            }
            // The purge, staged into the SAME commit: every retained manifest except this
            // commit's own goes, because a retained name would keep the superseded generation —
            // deleted content included — readable for MANIFEST_RETAIN more commits.
            for commit in container_retained_commits(&c) {
                if commit != m.commit {
                    let _ = c.remove(&format!("MANIFEST.{commit:08}"));
                }
            }
            // With no retained pins left, the sweep frees the old generation and every
            // superseded part in the same state that abandons them.
            sweep_unreachable_container(&mut c, &m, self.read_limits)?;
            c.commit().context(
                "re-fold staged completely but the publishing flip failed — nothing was \
                 published; reopen the store",
            )?;
        }

        self.manifest = m;
        self.note_manifest_commit();
        self.retained_commit_count = 1;
        let part_cache_budget = self.pcache.budget();
        self.pcache = Arc::new(SectionCache::new(part_cache_budget));
        self.parts.clear();
        for p in &self.manifest.parts {
            let reader =
                container.lock().expect("container lock poisoned").extent(&p.file).ok_or_else(
                    || anyhow::anyhow!("refold committed {} but the container lost it", p.file),
                )?;
            self.parts.push(Arc::new(Part::open_reader_with_limits(
                Box::new(reader),
                self.pcache.clone(),
                self.read_limits,
            )?));
        }
        self.fold = nf;
        // Freed in the same flip that abandoned it: there is no stale generation to report.
        stats.stale_generation_left = false;
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
    /// A directory store refuses: it returns space by unlinking, and has nothing to punch.
    pub fn punch_free_space(&mut self) -> Result<crate::container::FreePunchStats> {
        match &self.home {
            Home::Dir(_) => bail!(
                "a directory store returns freed space by unlinking; punch_free_space is the \
                 single-file store's reclamation"
            ),
            Home::File { container, .. } => {
                let c = container.lock().expect("container lock poisoned");
                c.punch_free_extents(MANIFEST_RETAIN as u64)
            }
        }
    }

    /// Bytes pinned by every open part's section caches, against their shared budget.
    pub fn part_cache_bytes(&self) -> (usize, usize) {
        (self.pcache.bytes(), self.pcache.budget())
    }

    /// Pieces resident in the Tier-0 dedup window. Bounded by the flush interval, not by store size.
    pub fn dedup_window_len(&self) -> usize {
        self.fold.window_len()
    }
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

    /// Walk the store directory once and classify every regular file by manifest reachability.
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
        match &self.home {
            Home::Dir(dir) => store_space_usage(dir, &self.manifest, self.read_limits, control),
            Home::File { container, path } => {
                let c = container.lock().expect("container lock poisoned");
                container_space_usage(&c, path, &self.manifest, self.read_limits, control)
            }
        }
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
/// — live manifest, retained manifests, their fold generations — with the WAL sidecar counted
/// live and the free list counted unclassified: bytes the file holds that no reachable name
/// claims, which is exactly what that bucket means. `total` is what the store occupies on disk —
/// the file itself plus its sidecar — so superblocks, the directory, and alignment padding are
/// inside it, as they are inside the file.
fn container_space_usage(
    c: &crate::container::Container,
    path: &Path,
    live_manifest: &Manifest,
    _read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<StoreSpaceUsage> {
    control.check("store space inventory")?;
    let mut live_files: std::collections::HashSet<String> =
        std::iter::once("MANIFEST".to_string()).collect();
    live_files.extend(live_manifest.parts.iter().map(|p| p.file.clone()));
    let live_prefix = crate::fold::fold_member_prefix(live_manifest.fold_gen);

    let mut retained_files: std::collections::HashSet<String> = Default::default();
    let mut retained_prefixes: std::collections::HashSet<String> = Default::default();
    for commit in container_retained_commits(c) {
        control.check("store space inventory")?;
        let name = format!("MANIFEST.{commit:08}");
        let bytes = c
            .read_file_bounded(&name, MAX_MANIFEST_BYTES)
            .with_context(|| format!("account retained manifest {commit}"))?;
        let m = Manifest::parse(&bytes)
            .with_context(|| format!("account retained manifest {commit}"))?;
        retained_files.insert(name);
        retained_files.extend(m.parts.into_iter().map(|p| p.file));
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
        amount.files += 1;
        amount.logical_bytes += bytes;
    };
    for name in c.names().map(String::from).collect::<Vec<String>>() {
        control.check("store space inventory")?;
        let len = c.member_len(&name).unwrap_or(0);
        let gen = fold_generation_of_member(&name);
        let is_live = live_files.contains(&name)
            || gen.map(|g| crate::fold::fold_member_prefix(g) == live_prefix).unwrap_or(false);
        let is_retained = retained_files.contains(&name)
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
    // The free list: bytes present under no reachable name. Zero files — extents are not files.
    usage.unclassified.logical_bytes += c.free_bytes();
    // Totals stay bucket-additive — the accounting identity every consumer of this struct
    // checks. Structural overhead (superblocks, the directory, alignment padding) is uncounted,
    // exactly as the directory layout never counted its inodes; the file's own length is one
    // stat away for anyone asking the other question.
    usage.total.files = usage.live.files + usage.retained_only.files + usage.unclassified.files;
    usage.total.logical_bytes = usage.live.logical_bytes
        + usage.retained_only.logical_bytes
        + usage.unclassified.logical_bytes;
    Ok(usage)
}

fn store_space_usage(
    dir: &Path,
    live_manifest: &Manifest,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<StoreSpaceUsage> {
    control.check("store space inventory")?;
    let mut live_files: HashSet<PathBuf> =
        [PathBuf::from("MANIFEST"), PathBuf::from("WAL")].into_iter().collect();
    live_files.extend(live_manifest.parts.iter().map(|part| PathBuf::from(&part.file)));
    let live_fold = fold_relative_dir(dir, live_manifest.fold_gen)?;

    let mut retained_files = HashSet::new();
    let mut retained_folds = HashSet::new();
    for commit in list_retained_with_limits(dir, read_limits)? {
        control.check("store space inventory")?;
        let retained_path = retained_path(dir, commit);
        retained_files.insert(relative_store_path(dir, &retained_path)?);
        let manifest = load_retained(dir, commit)
            .with_context(|| format!("account retained manifest {commit}"))?;
        retained_files.extend(manifest.parts.iter().map(|part| PathBuf::from(&part.file)));
        retained_folds.insert(fold_relative_dir(dir, manifest.fold_gen)?);
    }

    let mut usage = StoreSpaceUsage {
        filesystem_available_bytes: crate::sys::filesystem_available_bytes(dir)
            .with_context(|| format!("measure available filesystem bytes at {}", dir.display()))?,
        ..StoreSpaceUsage::default()
    };
    let reachability = SpaceReachability {
        live_files: &live_files,
        live_fold: &live_fold,
        retained_files: &retained_files,
        retained_folds: &retained_folds,
    };
    account_store_files(dir, dir, &reachability, &mut usage, read_limits, control)?;
    Ok(usage)
}

fn fold_relative_dir(dir: &Path, generation: u32) -> Result<PathBuf> {
    relative_store_path(dir, &refold::fold_dir(dir, generation))
}

fn relative_store_path(dir: &Path, path: &Path) -> Result<PathBuf> {
    path.strip_prefix(dir)
        .map(Path::to_path_buf)
        .with_context(|| format!("{} is outside store {}", path.display(), dir.display()))
}

struct SpaceReachability<'a> {
    live_files: &'a HashSet<PathBuf>,
    live_fold: &'a Path,
    retained_files: &'a HashSet<PathBuf>,
    retained_folds: &'a HashSet<PathBuf>,
}

fn account_store_files(
    root: &Path,
    start: &Path,
    reachability: &SpaceReachability<'_>,
    usage: &mut StoreSpaceUsage,
    read_limits: ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<()> {
    let mut directories = vec![start.to_path_buf()];
    let mut visited = 0u64;
    while let Some(dir) = directories.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("read store space directory {}", dir.display()))?
        {
            visited = visited.saturating_add(1);
            read_limits.admit_directory_entries("store filesystem inventory", visited)?;
            control.check("store space inventory")?;
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file() {
                let path = entry.path();
                let relative = relative_store_path(root, &path)?;
                let metadata = entry.metadata()?;
                add_space(&mut usage.total, &path, &metadata)?;
                let live = reachability.live_files.contains(&relative)
                    || relative.starts_with(reachability.live_fold);
                let retained = reachability.retained_files.contains(&relative)
                    || reachability.retained_folds.iter().any(|fold| relative.starts_with(fold));
                if live {
                    add_space(&mut usage.live, &path, &metadata)?;
                } else if retained {
                    add_space(&mut usage.retained_only, &path, &metadata)?;
                } else {
                    add_space(&mut usage.unclassified, &path, &metadata)?;
                }
            }
        }
    }
    Ok(())
}

fn add_space(amount: &mut SpaceAmount, path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    amount.files =
        amount.files.checked_add(1).ok_or_else(|| anyhow::anyhow!("store file count overflow"))?;
    amount.logical_bytes = amount
        .logical_bytes
        .checked_add(metadata.len())
        .ok_or_else(|| anyhow::anyhow!("store logical byte count overflow"))?;
    amount.allocated_bytes =
        match (amount.allocated_bytes, crate::sys::allocated_bytes(path, metadata)) {
            (Some(total), Some(bytes)) => Some(
                total
                    .checked_add(bytes)
                    .ok_or_else(|| anyhow::anyhow!("store allocated byte count overflow"))?,
            ),
            _ => None,
        };
    Ok(())
}

/// Exact physical-input bounds for one incremental compaction step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionBudget {
    /// Maximum number of adjacent immutable parts admitted to one work unit.
    pub max_input_parts: usize,
    /// Maximum physical rows across the admitted input parts.
    pub max_input_rows: u64,
    /// Maximum sum of the admitted input part file lengths.
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
/// An exact contiguous input selection from the current live part list.
pub struct CompactionPlan {
    /// Zero-based index of the oldest selected part.
    pub start_part: usize,
    /// Number of adjacent parts selected.
    pub input_parts: usize,
    /// Physical rows across the selected files.
    pub input_rows: u64,
    /// Sum of the selected files' exact on-disk lengths.
    pub input_bytes: u64,
    /// Whether this run covers the complete live list and may settle tombstones.
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
pub struct PunchStats {
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

/// A reader over the committed state. No lock, no writer, no daemon. Clones retain the same
/// immutable parts and fold handles, making ownership by concurrent query streams cheap.
#[derive(Clone)]
pub struct ReadStore {
    fold: Arc<Fold>,
    parts: Vec<Arc<Part>>,
    manifest: Manifest,
    read_limits: ReadLimits,
}

/// A read-only store IS the committed read core, with nothing layered on top — so every method here
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

    /// Bounded structured paging over this immutable manifest snapshot.
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
    /// of both; the manifest snapshot it was opened at is already baked into `parts`.
    pub fn into_parts(self) -> (Arc<Fold>, Vec<Arc<Part>>) {
        (self.fold, self.parts)
    }

    pub fn part_count(&self) -> usize {
        self.parts.len()
    }
    /// The live parts, oldest to newest — for tools that walk them (verification, inspection).
    pub fn parts(&self) -> &[Arc<Part>] {
        &self.parts
    }
    /// The fold, for tools that scrub or measure it.
    pub fn fold(&self) -> &Fold {
        &self.fold
    }
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

/// Reject a store directory that is obviously not one, before doing anything destructive.
pub fn looks_like_store(dir: &Path) -> bool {
    dir.join("MANIFEST").exists() || dir.join("fold").exists()
}

// Positioned reads are the one thing with no fallback: every read in the engine is "n bytes at
// offset o", and emulating that with seek-then-read is not safe across threads. Unix and WASI both
// provide it. What WASI does NOT provide — advisory locking and hole punching — is degraded
// explicitly in `crate::sys` rather than refused here.

#[cfg(test)]
mod tests {

    /// Build a directory-layout store with two commits and close it: `MANIFEST`,
    /// `MANIFEST.00000001`, `MANIFEST.00000002`, one part per commit.
    fn two_commit_dir_store(tag: &str) -> (std::path::PathBuf, crate::fold::FoldCfg) {
        let d = std::env::temp_dir().join(format!(
            "turndb-promote-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let cfg = crate::fold::FoldCfg::default();
        let mut store = super::Store::open(&d, cfg).unwrap();
        store.put("one", &[crate::store::Span::Piece(b"first commit")], vec![]).unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
        store.put("two", &[crate::store::Span::Piece(b"second commit")], vec![]).unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
        store.close().unwrap();
        assert!(d.join("MANIFEST").is_file());
        assert!(super::retained_path(&d, 1).is_file() && super::retained_path(&d, 2).is_file());
        (d, cfg)
    }

    /// The crash state a Windows replace-rename can leave: no live `MANIFEST`, a published
    /// newest retained copy. Open promotes it and serves both commits.
    #[test]
    fn an_absent_manifest_beside_a_whole_newest_retained_copy_is_promoted_at_open() {
        let (d, cfg) = two_commit_dir_store("whole");
        std::fs::remove_file(d.join("MANIFEST")).unwrap();
        let store = super::Store::open(&d, cfg).expect("open promotes the whole newest copy");
        assert_eq!(store.manifest().commit, 2);
        assert_eq!(store.reconstruct("one").unwrap().as_deref(), Some(&b"first commit"[..]));
        assert_eq!(store.reconstruct("two").unwrap().as_deref(), Some(&b"second commit"[..]));
        drop(store);
        assert!(d.join("MANIFEST").is_file(), "promotion published the live name");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Absent live manifest, damaged newest retained copy: refuse. The older copy is intact,
    /// and it must NOT be promoted — that is a rollback, an operator's decision, not an open's.
    #[test]
    fn an_absent_manifest_beside_a_damaged_newest_retained_copy_is_refused_not_rolled_back() {
        let (d, cfg) = two_commit_dir_store("damaged");
        std::fs::remove_file(d.join("MANIFEST")).unwrap();
        let p = super::retained_path(&d, 2);
        let mut bytes = std::fs::read(&p).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&p, &bytes).unwrap();
        let err = match super::Store::open(&d, cfg) {
            Ok(_) => panic!("a damaged newest copy must not be promoted"),
            Err(e) => e,
        };
        let text = format!("{err:#}");
        assert!(text.contains("does not validate whole"), "{text}");
        assert!(!d.join("MANIFEST").exists(), "nothing was published: {text}");
        assert!(super::retained_path(&d, 1).is_file(), "the older copy is untouched");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Absent live manifest, newest retained copy intact but a part it names is gone: refuse.
    /// A manifest checksum alone is not validation.
    #[test]
    fn an_absent_manifest_whose_newest_retained_copy_names_a_missing_part_is_refused() {
        let (d, cfg) = two_commit_dir_store("missing-part");
        std::fs::remove_file(d.join("MANIFEST")).unwrap();
        let newest = super::load_retained(&d, 2).unwrap();
        let part = newest.parts.last().expect("commit 2 wrote a part").file.clone();
        std::fs::remove_file(d.join(&part)).unwrap();
        let err = match super::Store::open(&d, cfg) {
            Ok(_) => panic!("a candidate missing a part must not be promoted"),
            Err(e) => e,
        };
        let text = format!("{err:#}");
        assert!(text.contains("does not validate whole"), "{text}");
        assert!(!d.join("MANIFEST").exists(), "nothing was published: {text}");
        std::fs::remove_dir_all(&d).ok();
    }
    fn manifest_bytes_with_part(file: &str) -> Vec<u8> {
        serde_json::to_vec(&super::Manifest {
            parts: vec![super::PartRef {
                file: file.into(),
                seq_lo: 1,
                seq_hi: 1,
                records: 1,
                b3: None,
            }],
            next_seq: 2,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn manifest_part_names_cannot_escape_the_store_root() {
        for hostile in
            ["../secret.part", "/absolute/secret.part", "nested/secret.part", "..\\secret.part"]
        {
            let error =
                super::Manifest::parse(&manifest_bytes_with_part(hostile)).unwrap_err().to_string();
            assert!(error.contains("store-local path component"), "{hostile:?}: {error}");
        }
        assert!(
            super::Manifest::parse(&manifest_bytes_with_part("part-00000001.part")).is_ok(),
            "an ordinary legacy manifest remains accepted"
        );
    }

    #[test]
    fn manifest_semantics_reject_ambiguous_or_malformed_authority() {
        let part = super::PartRef {
            file: "part-00000001.part".into(),
            seq_lo: 2,
            seq_hi: 1,
            records: 1,
            b3: None,
        };
        let inverted = super::Manifest { parts: vec![part.clone()], ..Default::default() };
        assert!(super::Manifest::parse(&serde_json::to_vec(&inverted).unwrap()).is_err());

        let duplicate = super::Manifest {
            parts: vec![
                super::PartRef { seq_lo: 1, seq_hi: 1, ..part.clone() },
                super::PartRef { seq_lo: 2, seq_hi: 2, ..part.clone() },
            ],
            ..Default::default()
        };
        assert!(super::Manifest::parse(&serde_json::to_vec(&duplicate).unwrap()).is_err());

        let bad_digest = super::Manifest {
            parts: vec![super::PartRef {
                seq_lo: 1,
                seq_hi: 1,
                b3: Some("not-a-blake3-digest".into()),
                ..part
            }],
            ..Default::default()
        };
        assert!(super::Manifest::parse(&serde_json::to_vec(&bad_digest).unwrap()).is_err());

        let punched = super::Manifest { punched: vec![(5, 9), (9, 12)], ..Default::default() };
        assert!(super::Manifest::parse(&serde_json::to_vec(&punched).unwrap()).is_err());
    }

    #[test]
    fn manifest_size_is_refused_before_reading_a_sparse_body() {
        let d = std::env::temp_dir().join(format!("turndb-manifest-limit-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("MANIFEST");
        std::fs::File::create(&path).unwrap().set_len(super::MAX_MANIFEST_BYTES + 1).unwrap();
        let error = format!("{:#}", super::Manifest::load(&d).unwrap_err());
        std::fs::remove_dir_all(d).ok();
        assert!(error.contains("exceeding") && error.contains("limit"), "{error}");
    }

    /// The bug this exists to prevent: corruption that still PARSES. A shortened `fold_off` here
    /// would have been believed, and recovery would then have truncated durable fold bytes to
    /// match it — data destroyed by one flipped bit with no error anywhere.
    #[test]
    fn a_flipped_byte_that_still_parses_is_refused() {
        let d = std::env::temp_dir().join(format!("turndb-mancrc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut m = super::Manifest { fold_off: 4096, next_seq: 9, ..Default::default() };
        m.commit(&d).unwrap();

        let mut b = std::fs::read(d.join("MANIFEST")).unwrap();
        let at = b.windows(4).position(|w| w == b"4096").expect("fold_off literal in the JSON");
        b[at] = b'1'; // now claims fold_off 1096 — valid JSON, wrong bytes
        std::fs::write(d.join("MANIFEST"), &b).unwrap();

        let err = super::Manifest::load(&d).unwrap_err().to_string();
        assert!(err.contains("checksum"), "must refuse via the checksum, got: {err}");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Damage to the TRAILER must not demote a checksummed manifest to a trusted legacy one.
    #[test]
    fn a_damaged_trailer_is_not_read_as_legacy() {
        let d = std::env::temp_dir().join(format!("turndb-mantrail-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut m = super::Manifest::default();
        m.commit(&d).unwrap();

        let mut b = std::fs::read(d.join("MANIFEST")).unwrap();
        let at = b.len() - 14; // the 'c' of the final "crc32=XXXXXXXX" line
        b[at] = b'x';
        std::fs::write(d.join("MANIFEST"), &b).unwrap();

        assert!(
            super::Manifest::load(&d).is_err(),
            "trailing bytes must fail JSON parsing, not be ignored"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// The retained log: every commit leaves a copy, the window prunes, recovery promotes.
    #[test]
    fn the_commit_log_retains_prunes_and_recovers() {
        let d = std::env::temp_dir().join(format!("turndb-manlog-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut m = super::Manifest::default();
        for i in 1..=6u32 {
            m.fold_off = i * 100; // distinguishable states
            m.commit(&d).unwrap();
        }
        assert_eq!(m.commit, 6);
        assert_eq!(
            super::list_retained_with_limits(&d, super::ReadLimits::default()).unwrap(),
            vec![3, 4, 5, 6],
            "window of {} commits",
            super::MANIFEST_RETAIN
        );

        // Bit rot in MANIFEST: open refuses; recovery promotes the newest copy — same commit,
        // nothing lost.
        let mut b = std::fs::read(d.join("MANIFEST")).unwrap();
        b[10] ^= 0xFF;
        std::fs::write(d.join("MANIFEST"), &b).unwrap();
        assert!(super::Manifest::load(&d).is_err());
        assert_eq!(super::complete_first_commit(&d).unwrap(), 6);
        assert_eq!(super::Manifest::load(&d).unwrap().fold_off, 600);

        // MANIFEST *and* the newest copies damaged: recovery rolls back to the newest intact one
        // and truncates the log to it — the abandoned copies cannot be promoted later.
        let mut b = std::fs::read(d.join("MANIFEST")).unwrap();
        b[10] ^= 0xFF;
        std::fs::write(d.join("MANIFEST"), &b).unwrap();
        std::fs::write(super::retained_path(&d, 6), b"garbage").unwrap();
        std::fs::write(super::retained_path(&d, 5), b"garbage").unwrap();
        assert_eq!(super::complete_first_commit(&d).unwrap(), 4);
        assert_eq!(super::Manifest::load(&d).unwrap().fold_off, 400);
        assert_eq!(
            super::list_retained_with_limits(&d, super::ReadLimits::default()).unwrap(),
            vec![3, 4],
            "the abandoned timeline is cleared"
        );

        // An intact store refuses rollback.
        assert!(
            super::complete_first_commit(&d).is_err(),
            "recovery of a healthy store must refuse"
        );

        // A MISSING manifest beside a commit log is damage, not a new store.
        std::fs::remove_file(d.join("MANIFEST")).unwrap();
        assert!(super::Manifest::load(&d).is_err(), "missing MANIFEST + commit log must refuse");
        assert_eq!(super::complete_first_commit(&d).unwrap(), 4);
        assert_eq!(super::Manifest::load(&d).unwrap().fold_off, 400);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn manifest_roundtrips() {
        let d = std::env::temp_dir().join(format!("turndb-man-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut m = super::Manifest {
            parts: vec![super::PartRef {
                file: "p.part".into(),
                seq_lo: 1,
                seq_hi: 1,
                records: 7,
                b3: None,
            }],
            fold_seg: 2,
            fold_off: 4096,
            next_seq: 9,
            fold_gen: 3,
            commit: 0,
            prev: None,
            punched: Vec::new(),
        };
        m.commit(&d).unwrap();
        let got = super::Manifest::load(&d).unwrap();
        assert_eq!(got.parts.len(), 1);
        assert_eq!(got.fold_off, 4096);
        assert_eq!(got.fold_gen, 3);
        assert_eq!(got.next_seq, 9);
        assert_eq!(got.commit, 1, "commit() must advance the commit counter");
        assert!(!d.join("MANIFEST.tmp").exists(), "staging file must not survive a commit");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn format_migration_is_one_part_atomic_resumable_and_content_preserving() {
        let d = std::env::temp_dir().join(format!(
            "turndb-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let mut fold =
            crate::fold::Fold::open(&d.join("fold"), crate::fold::FoldCfg::default()).unwrap();
        fold.sync().unwrap();
        let tail = fold.tail();
        drop(fold);
        let mut part_refs = Vec::new();
        for (seq, id) in [(1, "legacy-one"), (2, "legacy-two")] {
            let file = format!("legacy-{seq}.part");
            let meta = crate::part::build_revision_one_fixture(&d.join(&file), seq, id).unwrap();
            part_refs.push(super::PartRef {
                file,
                seq_lo: meta.seq_lo,
                seq_hi: meta.seq_hi,
                records: meta.n_records,
                b3: None,
            });
        }
        let mut manifest = super::Manifest {
            parts: part_refs,
            fold_seg: tail.seg,
            fold_off: tail.off,
            next_seq: 2,
            ..super::Manifest::default()
        };
        manifest.commit(&d).unwrap();

        let mut store = super::Store::open(&d, crate::fold::FoldCfg::default()).unwrap();
        let before = store.format_migration_status().unwrap();
        assert_eq!(before.legacy_parts, 2);
        assert_eq!(before.current_parts, 0);
        let plan = store.estimate_format_migration_space().unwrap().unwrap();
        assert_eq!(plan.source_part_version, 1);
        assert_eq!(plan.input_rows, 1);
        assert!(!plan.estimate_is_hard_bound);

        let cancellation = crate::control::CancellationToken::new();
        cancellation.cancel();
        let error = store
            .migrate_format_step_with_control(&crate::control::OperationControl {
                deadline: None,
                cancellation: Some(cancellation),
            })
            .unwrap_err();
        assert!(error.downcast_ref::<crate::control::OperationInterrupted>().is_some());
        assert_eq!(store.format_migration_status().unwrap().legacy_parts, 2);

        let step = store.migrate_format_step().unwrap().unwrap();
        assert_eq!(step.plan.part_index, plan.part_index);
        assert_eq!(step.plan.source_part_version, plan.source_part_version);
        assert_eq!((step.plan.seq_lo, step.plan.seq_hi), (plan.seq_lo, plan.seq_hi));
        assert_eq!(step.plan.input_bytes, plan.input_bytes);
        assert_eq!(step.rewrite.inputs, 1);
        assert_eq!(step.remaining_legacy_parts, 1);
        assert!(step.output_bytes <= plan.estimated_stage_bytes);
        let record = store.get("legacy-one").unwrap().unwrap();
        assert_eq!(record.contents[0].name, crate::types::BODY_CONTENT);
        assert_eq!(record.contents[0].identity, None, "migration must not invent identity");
        assert_eq!(record.contents[0].ops, [crate::BodyOp::Lit(b"legacy".to_vec())]);
        drop(store);

        let mut reopened = super::Store::open(&d, crate::fold::FoldCfg::default()).unwrap();
        let midway = reopened.format_migration_status().unwrap();
        assert_eq!(midway.legacy_parts, 1);
        assert_eq!(midway.current_parts, 1);
        assert_eq!(midway.retained_legacy_parts, 1);
        let resumed = reopened.migrate_format_step().unwrap().unwrap();
        assert_eq!(resumed.remaining_legacy_parts, 0);
        assert!(reopened.migrate_format_step().unwrap().is_none());
        let after = reopened.format_migration_status().unwrap();
        assert_eq!(after.legacy_parts, 0);
        assert_eq!(after.current_parts, 2);
        assert_eq!(after.retained_legacy_parts, 2);
        assert!(after.retained_legacy_bytes > 0);
        assert_eq!(reopened.get("legacy-one").unwrap().unwrap(), record);
        assert!(reopened.get("legacy-two").unwrap().is_some());
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn punching_retries_blocks_already_declared_before_a_crash() {
        let d = std::env::temp_dir().join(format!(
            "turndb-punch-resume-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let cfg = crate::fold::FoldCfg { block_target: 1, ..Default::default() };
        let mut store = super::Store::open(&d, cfg).unwrap();
        let old = vec![0x41; 64 * 1024];
        let live = vec![0x42; 64 * 1024];
        store.put("k", &[super::Span::Piece(&old)], vec![]).unwrap();
        store.sync().unwrap();
        store.flush().unwrap();
        store.put("k", &[super::Span::Piece(&live)], vec![]).unwrap();
        store.sync().unwrap();
        store.flush().unwrap();

        let live_block = store.locate(&crate::PieceHash::of(&live)).unwrap().unwrap().block_id;
        let dead: Vec<u32> =
            store.fold.block_ids().into_iter().filter(|&block| block != live_block).collect();
        assert!(!dead.is_empty());

        // The durable state left by a crash immediately after "record first, punch second".
        let mut manifest = store.manifest.clone();
        manifest.punched = super::to_ranges(&dead);
        manifest.commit(&d).unwrap();
        store.manifest = manifest;
        store.fold.declare_punched(&store.manifest.punched);

        let stats = store.punch_unreferenced().unwrap();
        assert_eq!(stats.blocks_examined, dead.len());
        assert_eq!(stats.blocks_punched, dead.len(), "declared blocks must be retried");
        assert_eq!(store.reconstruct("k").unwrap().unwrap(), live);
        std::fs::remove_dir_all(&d).ok();
    }
}

#[cfg(test)]
mod compat_tests {
    /// A manifest written before fold generations existed must still load, naming generation 0 — the
    /// original `fold/` directory. Otherwise this change would silently orphan every existing store.
    #[test]
    fn a_manifest_without_fold_gen_reads_as_generation_zero() {
        let d = std::env::temp_dir().join(format!("turndb-oldman-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("MANIFEST"),
            br#"{"parts":[],"fold_seg":0,"fold_off":48,"next_seq":4}"#,
        )
        .unwrap();
        let m = super::Manifest::load(&d).unwrap();
        assert_eq!(m.fold_gen, 0, "a pre-generation manifest must mean the original fold/");
        assert_eq!(m.next_seq, 4);
        assert_eq!(super::refold::fold_dir(&d, m.fold_gen), d.join("fold"));
        std::fs::remove_dir_all(&d).ok();
    }
}
