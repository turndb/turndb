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
//! [`Store::open_read`] takes no lock, replays nothing, and is safe to run
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

pub mod read;
pub mod refold;
pub mod wal;

use crate::fold::{Fold, FoldCfg, FoldTail, Loc};
use crate::part::cache::SectionCache;
use crate::part::{self, Part};
use crate::types::{AttrValue, BodyOp, Content, ContentHash, PieceHash, Record, BODY_CONTENT};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::fs::File;
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
    /// One decompressed-section cache budget shared by every immutable part in this handle.
    pub part_cache_bytes: usize,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            fold: FoldCfg::default(),
            write_limits: WriteLimits::default(),
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
    Ok(size)
}

fn delete_admission_bytes(id: &str, limits: WriteLimits, item: Option<usize>) -> Result<u64> {
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

impl Manifest {
    /// A MISSING manifest is a new store. An UNREADABLE one is an error.
    ///
    /// These were conflated, and the orphan sweep made the conflation destructive: a transient EACCES
    /// or EIO yielded an empty manifest, and the sweep then unlinked every part it did not name. One
    /// unreadable byte turned a live store into an empty directory.
    fn load(dir: &Path) -> Result<Manifest> {
        match std::fs::read(dir.join("MANIFEST")) {
            Ok(b) => Manifest::parse(&b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A missing manifest is a new store — UNLESS a commit log exists, in which case
                // this store has committed before and `MANIFEST` was lost. Opening it as new
                // would be the destructive conflation all over again, one deletion further
                // upstream: an empty manifest followed by the sweep.
                let retained = list_retained(dir);
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
            Err(e) => Err(anyhow::Error::new(e).context(format!(
                "cannot read {} — refusing to treat an unreadable manifest as an empty store",
                dir.join("MANIFEST").display()
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
        serde_json::from_slice(payload).context("corrupt MANIFEST")
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
    /// describing a commit that never took effect — which is exactly the old manifest's state plus
    /// a counter bump, and harmless: promotion would reproduce the state the store is already in.
    ///
    /// One directory fsync at the end covers both dirents. Pruning runs last and is best-effort —
    /// a retained manifest that outlives its window is swept space, never a correctness problem.
    fn commit(&mut self, dir: &Path) -> Result<()> {
        self.commit += 1;
        // Chain onto whatever is being replaced. Hashed from disk rather than from memory,
        // because the chain's claim is about the BYTES a verifier can read back.
        self.prev =
            std::fs::read(dir.join("MANIFEST")).ok().map(|b| blake3::hash(&b).to_hex().to_string());
        let bytes = self.encode()?;
        {
            let p = retained_path(dir, self.commit);
            let f = crate::vfs::create(&p)?;
            crate::vfs::write_all_at(&f, &p, &bytes, 0)?;
            crate::vfs::sync_file(&f, &p)?;
        }
        let tmp = dir.join("MANIFEST.tmp");
        let f = crate::vfs::create(&tmp)?;
        crate::vfs::write_all_at(&f, &tmp, &bytes, 0)?;
        crate::vfs::sync_file(&f, &tmp)?;
        drop(f);
        crate::vfs::rename(&tmp, &dir.join("MANIFEST"))?;
        crate::vfs::sync_dir(dir)?;
        for c in list_retained(dir) {
            if c + (MANIFEST_RETAIN as u64) <= self.commit {
                let _ = crate::vfs::unlink(&retained_path(dir, c));
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

/// Retained commits on disk, ascending. Parsed NUMERICALLY, the same rule as segment names:
/// lexicographic order breaks past the padding width. `MANIFEST.tmp` does not match.
fn list_retained(dir: &Path) -> Vec<u64> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix("MANIFEST.") {
                if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                    if let Ok(n) = rest.parse::<u64>() {
                        out.push(n);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out
}

/// The snapshot commits currently available to [`Store::open_read_at`], ascending.
pub fn retained_commits(dir: &Path) -> Vec<u64> {
    list_retained(dir)
}

/// Parse manifest bytes from an external source (a pack), trailer verification included.
pub(crate) fn manifest_from_bytes(b: &[u8]) -> Result<Manifest> {
    Manifest::parse(b)
}

/// Open a READER over a pack — the store in one file, served through bounded extents.
///
/// Everything [`ReadStore`] can do over a directory it does here identically: same manifest, same
/// parts, same fold, same version resolution. There is no writer role to take — a pack is
/// immutable by definition — and no retry loop to need, because nothing can sweep files out from
/// under an open handle on an immutable artifact.
pub fn open_read_pack(path: &Path, cfg: FoldCfg) -> Result<ReadStore> {
    let pack = crate::pack::Pack::open(path)?;
    let manifest = Manifest::parse(&pack.read_file("MANIFEST")?)?;

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
            segs.push(crate::fold::SegmentInput {
                seg: n,
                reader: Arc::new(pack.file(&name).expect("named file exists"))
                    as Arc<dyn crate::readat::ReadAt>,
                sidecar: pack.read_file(&format!("{fold_rel}/seg-{n:08}.dir")).ok(),
            });
        } else if rest.starts_with("zdict-") && rest.ends_with(".zd") {
            dict_files.push(pack.read_file(&name)?);
        }
    }
    let fold = Fold::open_read_from(segs, dict_files, cfg, path)?;

    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    for p in &manifest.parts {
        let ext = pack.file(&p.file).ok_or_else(|| {
            anyhow::anyhow!("pack manifest names {} but the pack does not hold it", p.file)
        })?;
        parts.push(Arc::new(Part::open_reader(Box::new(ext), pcache.clone())?));
    }
    Ok(ReadStore { fold: Arc::new(fold), parts, manifest })
}

fn load_retained(dir: &Path, commit: u64) -> Result<Manifest> {
    let p = retained_path(dir, commit);
    let b = std::fs::read(&p).with_context(|| {
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
fn sweep_unreachable(dir: &Path) -> Result<()> {
    let mut keep: Vec<Manifest> = vec![Manifest::load(dir)?];
    for c in list_retained(dir) {
        if let Ok(m) = load_retained(dir, c) {
            keep.push(m);
        }
    }
    let live_parts: HashSet<&str> =
        keep.iter().flat_map(|m| m.parts.iter().map(|p| p.file.as_str())).collect();
    let live_gens: HashSet<u32> = keep.iter().map(|m| m.fold_gen).collect();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
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

/// What [`verify_chain`] checked.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChainReport {
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
}

/// Verify the manifest hash chain and every part pin it carries, across the retained window.
///
/// Catches what the per-section checksums cannot: a part swapped for another valid part, a
/// manifest restored out of order, a file replaced wholesale. Each of those is internally
/// consistent and only the chain notices. Verifiable across the retained window; silent about
/// commits whose bytes have been pruned.
pub fn verify_chain(dir: &Path) -> Result<ChainReport> {
    verify_chain_with_control(dir, &crate::control::OperationControl::default())
}

/// [`verify_chain`] with cooperative checks between retained manifests and part digests.
pub fn verify_chain_with_control(
    dir: &Path,
    control: &crate::control::OperationControl,
) -> Result<ChainReport> {
    let mut report = ChainReport::default();
    let commits = list_retained(dir);
    let mut prev_bytes: Option<Vec<u8>> = None;
    for &c in &commits {
        control.check("manifest verification")?;
        let bytes = std::fs::read(retained_path(dir, c))?;
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
                    let got = blake3::hash(
                        &std::fs::read(dir.join(&p.file))
                            .with_context(|| format!("part {} named by commit {c}", p.file))?,
                    )
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
        let live = std::fs::read(dir.join("MANIFEST"))?;
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
fn complete_first_commit(dir: &Path) -> Result<u64> {
    if Manifest::load(dir).is_ok() {
        bail!("MANIFEST at {} is intact — refusing to roll back a healthy store", dir.display());
    }
    for c in list_retained(dir).into_iter().rev() {
        if load_retained(dir, c).is_err() {
            continue;
        }
        let bytes = std::fs::read(retained_path(dir, c))?;
        promote_manifest(dir, c, &bytes)?;
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

/// Recover a damaged manifest only after excluding writers and validating the complete candidate.
///
/// This is an explicit offline operator action, never an automatic fallback. In the common case,
/// the newest retained copy carries the same commit as the damaged live manifest and no data is
/// abandoned. Falling back farther requires an explicit [`RecoveryOptions::max_rollback_commits`]
/// allowance. Before publication, TurnDB validates the candidate's exact committed fold prefix,
/// every named part and section, every visible content program, and every available whole-value
/// identity. A healthy store and a concurrently open writer are both refused.
pub fn recover_manifest(
    dir: &Path,
    cfg: FoldCfg,
    options: RecoveryOptions,
) -> Result<RecoveryReport> {
    recover_manifest_with_control(dir, cfg, options, &crate::control::OperationControl::default())
}

/// [`recover_manifest`] with cooperative cancellation before manifest promotion.
///
/// Candidate discovery and complete validation are read-only. The last cancellation checkpoint is
/// immediately before `promote_manifest`; once promotion begins, its crash-safe protocol owns the
/// outcome and recovery will not report cancellation after changing the live commit point.
pub fn recover_manifest_with_control(
    dir: &Path,
    cfg: FoldCfg,
    options: RecoveryOptions,
    control: &crate::control::OperationControl,
) -> Result<RecoveryReport> {
    control.check("manifest recovery")?;
    let _locks = recovery_locks(dir, control)?;
    control.check("manifest recovery")?;
    if Manifest::load(dir).is_ok() {
        return Err(RecoveryError::Healthy(dir.to_path_buf()).into());
    }
    let commits = list_retained(dir);
    let newest = commits.last().copied().unwrap_or(0);
    let mut examined = 0usize;
    let mut last_reason = "no retained manifests exist".to_string();
    for c in commits.into_iter().rev() {
        control.check("manifest recovery validation")?;
        examined += 1;
        let manifest = match load_retained(dir, c) {
            Ok(manifest) => manifest,
            Err(error) => {
                last_reason = error.to_string();
                continue;
            }
        };
        match validate_recovery_candidate(dir, cfg, manifest, control) {
            Ok(mut report) => {
                let rollback_commits = newest.saturating_sub(c);
                if rollback_commits > options.max_rollback_commits {
                    return Err(RecoveryError::RollbackLimit {
                        needed: rollback_commits,
                        allowed: options.max_rollback_commits,
                    }
                    .into());
                }
                let bytes = std::fs::read(retained_path(dir, c))?;
                // No cancellation checkpoint after this point. Promotion can change MANIFEST and
                // prune newer retained manifests, so its actual outcome must be reported.
                control.check("manifest recovery publication")?;
                promote_manifest(dir, c, &bytes)?;
                report.commit = c;
                report.rollback_commits = rollback_commits;
                return Ok(report);
            }
            Err(error) => {
                if crate::error::classify(&error) == crate::error::ErrorClass::Cancelled {
                    return Err(error);
                }
                last_reason = error.to_string();
            }
        }
    }
    Err(RecoveryError::NoUsableCandidate { examined, reason: last_reason }.into())
}

fn recovery_locks(dir: &Path, control: &crate::control::OperationControl) -> Result<Vec<File>> {
    let mut folds = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read store directory {} for recovery", dir.display()))?
        .flatten()
    {
        control.check("manifest recovery locking")?;
        if entry.path().is_dir()
            && refold::parse_fold_gen(&entry.file_name().to_string_lossy()).is_some()
        {
            folds.push(entry.path());
        }
    }
    folds.sort();
    folds
        .into_iter()
        .map(|path| {
            control.check("manifest recovery locking")?;
            crate::fold::acquire_writer_lock(&path)
        })
        .collect()
}

fn validate_recovery_candidate(
    dir: &Path,
    cfg: FoldCfg,
    manifest: Manifest,
    control: &crate::control::OperationControl,
) -> Result<RecoveryReport> {
    control.check("manifest recovery validation")?;
    let fold_dir = refold::fold_dir(dir, manifest.fold_gen);
    let mut fold = match manifest.fold_tail() {
        Some(tail) => Fold::open_read_at(&fold_dir, cfg, tail)?,
        None => Fold::open_read(&fold_dir, cfg)?,
    };
    fold.declare_punched(&manifest.punched);
    let scrub = fold.scrub_with_control(control)?;
    let pcache = SectionCache::shared();
    let mut parts = Vec::with_capacity(manifest.parts.len());
    let mut part_sections = 0usize;
    for part_ref in &manifest.parts {
        control.check("manifest recovery validation")?;
        let path = dir.join(&part_ref.file);
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
        let part = Arc::new(Part::open_in(&path, pcache.clone())?);
        part_sections += part.verify_sections_with_control(control)?;
        parts.push(part);
    }
    let reader = ReadStore { fold: Arc::new(fold), parts, manifest };
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
    Ok(RecoveryReport {
        records: ids.len(),
        content_values,
        parts: reader.parts.len(),
        part_sections,
        fold_segments: scrub.segments,
        fold_blocks: scrub.blocks,
        fold_bytes: scrub.bytes,
        ..RecoveryReport::default()
    })
}

fn hash_file_with_control(
    path: &Path,
    control: &crate::control::OperationControl,
    operation: &'static str,
) -> Result<blake3::Hash> {
    use std::io::Read;

    let mut file = File::open(path)?;
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

fn promote_manifest(dir: &Path, commit: u64, bytes: &[u8]) -> Result<()> {
    let tmp = dir.join("MANIFEST.tmp");
    let f = crate::vfs::create(&tmp)?;
    crate::vfs::write_all_at(&f, &tmp, bytes, 0)?;
    crate::vfs::sync_file(&f, &tmp)?;
    drop(f);
    crate::vfs::rename(&tmp, &dir.join("MANIFEST"))?;
    crate::vfs::sync_dir(dir)?;
    for retained in list_retained(dir) {
        if retained > commit {
            let _ = crate::vfs::unlink(&retained_path(dir, retained));
        }
    }
    Ok(())
}

pub struct Store {
    dir: PathBuf,
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
            allocated_bytes: if cfg!(unix) { Some(0) } else { None },
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

    /// Open for writing. Takes the writer lock (through the fold) and recovers.
    ///
    /// **On Unix** that lock is `flock` and the kernel enforces it. **On `wasm32-wasip1` it is not
    /// enforced at all** — WASI has no advisory locking, so the lock file is created and gates
    /// nothing, and the single-writer invariant becomes the embedder's: at most one open writer per
    /// store directory, across every process and every instance. See `src/sys.rs` and FORMAT.md.
    pub fn open(dir: &Path, cfg: FoldCfg) -> Result<Store> {
        Self::open_with_options(dir, StoreOptions { fold: cfg, ..StoreOptions::default() })
    }

    /// Open a writer with explicit runtime admission policy.
    pub fn open_with_limits(dir: &Path, cfg: FoldCfg, write_limits: WriteLimits) -> Result<Store> {
        Self::open_with_options(
            dir,
            StoreOptions { fold: cfg, write_limits, ..StoreOptions::default() },
        )
    }

    /// Open a writer with explicit storage, cache, and admission configuration.
    pub fn open_with_options(dir: &Path, options: StoreOptions) -> Result<Store> {
        let recovery_started = std::time::Instant::now();
        let StoreOptions { fold: cfg, write_limits, part_cache_bytes } = options;
        let write_limits = write_limits.validate()?;
        if part_cache_bytes < crate::part::cache::BUDGET_MIN {
            bail!("part_cache_bytes must be at least {}", crate::part::cache::BUDGET_MIN);
        }
        crate::vfs::mkdir_all(dir)?;
        let manifest = match Manifest::load(dir) {
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
                let retained = list_retained(dir);
                if !dir.join("MANIFEST").exists() && retained == [1] {
                    if load_retained(dir, 1).is_ok() {
                        complete_first_commit(dir)?;
                    } else {
                        crate::vfs::unlink(&retained_path(dir, 1))?;
                        crate::vfs::sync_dir(dir)?;
                    }
                    Manifest::load(dir)?
                } else {
                    return Err(e);
                }
            }
        };

        // Recovery is a truncate, not a negotiation: whatever the fold wrote past the committed tail
        // is discarded, and the log regenerates it.
        let mut fold =
            Fold::open_at(&refold::fold_dir(dir, manifest.fold_gen), cfg, manifest.fold_tail())?;
        fold.declare_punched(&manifest.punched);

        let pcache = Arc::new(SectionCache::new(part_cache_bytes));
        let mut parts = Vec::with_capacity(manifest.parts.len());
        for p in &manifest.parts {
            parts.push(Arc::new(Part::open_in(&dir.join(&p.file), pcache.clone())?));
        }

        // A part file or fold generation no manifest names was written by a flush, merge, or
        // re-fold that crashed before committing, or has aged out of the retention window. Either
        // way it is unreachable. Safe to unlink even with readers attached: Unix keeps their open
        // mappings alive.
        sweep_unreachable(dir)?;
        // Crash litter: builder spools and staging files are all *.tmp, and every one of them is
        // pre-commit garbage. Swept ONLY at writer open, not at flush — an external packer's
        // staging file must not race a live writer's flush.
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(".tmp") && e.path().is_file() {
                    let _ = crate::vfs::unlink(&e.path());
                }
            }
        }

        let wal_path = dir.join("WAL");
        let frames = Wal::replay(&wal_path)?;
        let recovered_wal_frames = u64::try_from(frames.len()).unwrap_or(u64::MAX);
        let mut mem: BTreeMap<String, Option<Record>> = BTreeMap::new();
        let mut mem_bytes = 0usize;
        for f in frames {
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
        let wal = Wal::open(&wal_path)?;

        let mut metrics = crate::observability::StoreMetrics {
            recovered_wal_frames,
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
            dir: dir.to_path_buf(),
            fold,
            parts,
            manifest,
            mem,
            mem_bytes,
            wal,
            cfg,
            write_limits,
            pcache,
            metrics,
            events,
        })
    }

    /// The policy governing future writes through this handle.
    pub fn write_limits(&self) -> WriteLimits {
        self.write_limits
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
            verify_chain_with_control(&self.dir, control),
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
        Ok(StoreVerification { chain, fold, parts: self.parts.len(), part_sections })
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
            let size = std::fs::metadata(self.dir.join(&part.file))
                .with_context(|| format!("measure live part {}", part.file))?
                .len();
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

    /// Open for reading only: no lock, no replay, no daemon.
    ///
    /// Sees exactly the committed manifest — uncommitted records in some writer's memtable are
    /// invisible, which is the correct snapshot. Safe alongside a live writer because parts are
    /// immutable and the fold is append-only.
    pub fn open_read(dir: &Path, cfg: FoldCfg) -> Result<ReadStore> {
        // Reading the manifest and opening the fold generation plus parts it names is not atomic. A
        // writer may commit a merge or re-fold and unlink the replaced files in between, or commit a
        // flush whose new part names fold blocks a reader scanned just before they landed. The
        // manifest IS the linearization point, so every attempt starts from one manifest and opens the
        // fold and parts belonging to that exact snapshot. Once open, Unix keeps all of those handles
        // alive through a later unlink.
        //
        // Bounded, because a manifest naming a genuinely absent part must eventually surface as an
        // error rather than spin.
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..8 {
            let manifest = Manifest::load(dir)?;
            let fold_path = refold::fold_dir(dir, manifest.fold_gen);
            let fold = match manifest.fold_tail().map_or_else(
                || Fold::open_read(&fold_path, cfg),
                |tail| Fold::open_read_at(&fold_path, cfg, tail),
            ) {
                Ok(mut fold) => {
                    // A live record never references a punched block — `punch_unreferenced` walks
                    // live visibility to decide. Declared anyway so that if a stale `Loc` ever does
                    // reach one, it is named as erasure rather than as a failing disk.
                    fold.declare_punched(&manifest.punched);
                    fold
                }
                Err(e) => {
                    let gone = e
                        .downcast_ref::<std::io::Error>()
                        .map(|io| io.kind() == std::io::ErrorKind::NotFound)
                        .unwrap_or(false)
                        || !fold_path.exists();
                    if !gone {
                        return Err(e);
                    }
                    last = Some(e);
                    continue;
                }
            };
            let pcache = SectionCache::shared();
            let mut parts = Vec::with_capacity(manifest.parts.len());
            let mut missed = false;
            for p in &manifest.parts {
                match Part::open_in(&dir.join(&p.file), pcache.clone()) {
                    Ok(part) => parts.push(Arc::new(part)),
                    Err(e) => {
                        let gone = e
                            .downcast_ref::<std::io::Error>()
                            .map(|io| io.kind() == std::io::ErrorKind::NotFound)
                            .unwrap_or(false)
                            || !dir.join(&p.file).exists();
                        if !gone {
                            return Err(e);
                        }
                        last = Some(e);
                        missed = true;
                        break;
                    }
                }
            }
            if !missed {
                // A re-fold commit changes the address space itself: the new parts' Locs are only
                // meaningful against the new fold generation. If that swap happened while this
                // attempt was opening files, retry even when every individual open succeeded.
                if Manifest::load(dir)?.fold_gen != manifest.fold_gen {
                    last = Some(anyhow::anyhow!(
                        "fold generation changed while opening a reader snapshot"
                    ));
                    continue;
                }
                return Ok(ReadStore { fold: Arc::new(fold), parts, manifest });
            }
        }
        Err(last.unwrap_or_else(|| {
            anyhow::anyhow!("manifest snapshot names storage that does not exist")
        }))
    }

    /// Open a READER on a retained snapshot: the store exactly as commit `commit` left it.
    ///
    /// Only commits still inside the retention window exist — [`retained_commits`] lists them, and
    /// a re-fold empties the list on purpose (time travel must not resurrect erased content).
    ///
    /// No retry loop, unlike [`Store::open_read`], deliberately: the files a LIVE manifest names
    /// can be superseded while opening them, but a retained snapshot's files are pinned on disk by
    /// its manifest, and the one way they vanish is the window advancing past it — a real error,
    /// reported as one.
    pub fn open_read_at(dir: &Path, cfg: FoldCfg, commit: u64) -> Result<ReadStore> {
        let manifest = load_retained(dir, commit)?;
        let fold_dir = refold::fold_dir(dir, manifest.fold_gen);
        let mut fold = match manifest.fold_tail() {
            Some(tail) => Fold::open_read_at(&fold_dir, cfg, tail)?,
            None => Fold::open_read(&fold_dir, cfg)?,
        };
        // Erasure is declared by the LIVE manifest, not by this one. Punching commits a new manifest,
        // so a retained copy predates every punch that followed it and declares nothing — which is
        // exactly how a deliberate erasure came to be reported as a checksum failure. `punched` is
        // cumulative, so the live copy is the whole truth about what is gone.
        //
        // The load is PROPAGATED, not tolerated. Treating an unreadable live manifest as "nothing
        // was erased" is the same false fallback this whole fix exists to remove: the reader would
        // proceed with no declaration and report a punched payload as checksum corruption, which is
        // the misattribution, arrived at by a different route. An unreadable manifest is an error
        // and not an empty one — the store's existing rule, and it applies with more force here,
        // because this manifest is the ONLY authority for telling erasure from damage.
        let live = Manifest::load(dir).with_context(|| {
            format!(
                "retained snapshot at commit {commit} needs the live manifest in {} to tell erased \
                 blocks from damaged ones, and it could not be read",
                dir.display()
            )
        })?;
        // Guarded on the generation because block ids are per generation: applying one generation's
        // punched ranges to another would name live blocks. A re-fold purges the retained log, so a
        // mismatch should be unreachable — and if it happens anyway, there is no surviving authority
        // for the old generation's erasures (a re-fold clears `punched`, having rewritten the world
        // without the erased content). No authority means refuse, for the same reason as above.
        if live.fold_gen != manifest.fold_gen {
            bail!(
                "retained snapshot at commit {commit} is fold generation {} but the live manifest is \
                 {}, so no erasure declaration covers it",
                manifest.fold_gen,
                live.fold_gen
            );
        }
        fold.declare_punched(&live.punched);
        let pcache = SectionCache::shared();
        let mut parts = Vec::with_capacity(manifest.parts.len());
        for p in &manifest.parts {
            parts.push(Arc::new(Part::open_in(&dir.join(&p.file), pcache.clone())?));
        }
        Ok(ReadStore { fold: Arc::new(fold), parts, manifest })
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
        input_record_admission_bytes(id, &input, &attrs, self.write_limits, None)?;
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
        input_record_admission_bytes(id, contents, &attrs, self.write_limits, None)?;
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
                    Some(index),
                )?,
                BatchItem::Delete { id } => {
                    delete_admission_bytes(id, self.write_limits, Some(index))?
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
        delete_admission_bytes(id, self.write_limits, None)?;
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
    /// manifest while the packer walks the files it names; the writer lock excludes a second writer
    /// process.
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
        let stats = crate::pack::write_committed_with_control(&self.dir, out, control)?;
        Ok(crate::pack::BackupStats {
            files: stats.files,
            bytes: stats.bytes,
            commit: self.manifest.commit,
        })
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
        let tail = self.fold.sync()?;
        let seq = self.manifest.next_seq + 1;
        let file = format!("part-{seq:08}.part");
        let path = self.dir.join(&file);
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
        let meta = match part::build_full(
            &path,
            &recs,
            &tombs,
            seq,
            seq,
            self.cfg.level,
            |h| locs.get(h).copied(),
            &HashMap::new(),
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
        if let Err(error) = control.check("memtable flush publication") {
            let _ = crate::vfs::unlink(&path);
            return Err(error.into());
        }
        m.commit(&self.dir)?; // <- the linearization point

        self.parts.push(Arc::new(Part::open_in(&path, self.pcache.clone())?));
        self.manifest = m;
        // The commit may have pruned a retained manifest; whatever only it named is now sweepable.
        sweep_unreachable(&self.dir)?;
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

    /// [`Store::merge_range`] with cooperative checkpoints before its manifest publication.
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
        let path = self.dir.join(&file);
        // A tombstone may only be discarded when this merge covers the ENTIRE live list — otherwise a
        // part outside the run could still hold an older version of the deleted id, and dropping the
        // tombstone would resurrect it.
        let total = lo == 0 && len == self.parts.len();
        let (meta, stats) = match crate::part::merge::merge_opts_with_control(
            &path,
            &inputs,
            self.cfg.level,
            total,
            control,
        ) {
            Ok(built) => built,
            Err(error) => {
                let _ = crate::vfs::unlink(&path);
                return Err(error);
            }
        };

        // Publish: the merged part is durable (part::build fsyncs) before the manifest names it, and
        // the manifest swap is the single linearization point. A crash before it leaves the merged
        // file as an unreachable orphan. The INPUTS are not deleted here: retained manifests still
        // name them, so a reader inside the retention window keeps a complete snapshot on disk.
        // They fall to the sweep when the window prunes past their last naming manifest.
        // Every fallible preparation step and the final cancellation checkpoint happen before
        // commit is attempted. Once commit starts, its ordinary crash protocol—not cancellation—
        // decides the outcome, and the output must remain available to any retained manifest that
        // may have landed.
        let digest =
            match std::fs::read(&path).map(|bytes| blake3::hash(&bytes).to_hex().to_string()) {
                Ok(digest) => digest,
                Err(error) => {
                    let _ = crate::vfs::unlink(&path);
                    return Err(error.into());
                }
            };
        if let Err(error) = control.check("part compaction") {
            let _ = crate::vfs::unlink(&path);
            return Err(error.into());
        }
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
        m.commit(&self.dir)?;

        self.parts.splice(lo..lo + len, [Arc::new(Part::open_in(&path, self.pcache.clone())?)]);
        self.manifest = m;
        sweep_unreachable(&self.dir)?;
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
        if self.parts.len() < trigger {
            return Ok(None);
        }
        self.merge_range(0, run.min(self.parts.len()))
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

    /// [`Store::auto_compact`] with cooperative checkpoints.
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
                let bytes = std::fs::metadata(self.dir.join(&part.file))
                    .with_context(|| format!("measure compaction input {}", part.file))?
                    .len();
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
            filesystem_available_bytes: crate::sys::filesystem_available_bytes(&self.dir)
                .with_context(|| {
                    format!("measure available filesystem bytes at {}", self.dir.display())
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
        let output_bytes = std::fs::metadata(self.dir.join(&output.file))
            .with_context(|| format!("measure compaction output {}", output.file))?
            .len();
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
                .checked_add(std::fs::metadata(self.dir.join(&part_ref.file))?.len())
                .ok_or_else(|| anyhow::anyhow!("legacy format byte count overflow"))?;
        }
        let live_files: HashSet<&str> =
            self.manifest.parts.iter().map(|part| part.file.as_str()).collect();
        let mut retained_seen = HashSet::new();
        for commit in list_retained(&self.dir) {
            control.check("format migration status")?;
            let manifest = load_retained(&self.dir, commit)
                .with_context(|| format!("inspect migration state at retained commit {commit}"))?;
            for part_ref in manifest.parts {
                control.check("format migration status")?;
                if live_files.contains(part_ref.file.as_str())
                    || !retained_seen.insert(part_ref.file.clone())
                {
                    continue;
                }
                let path = self.dir.join(&part_ref.file);
                let part = Part::open_in(&path, self.pcache.clone())?;
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
                    .checked_add(std::fs::metadata(path)?.len())
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
        let input_bytes = std::fs::metadata(self.dir.join(&part_ref.file))?.len();
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
            filesystem_available_bytes: crate::sys::filesystem_available_bytes(&self.dir)
                .with_context(|| {
                    format!("measure available filesystem bytes at {}", self.dir.display())
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
        let input = self.parts[plan.part_index].clone();
        let file = format!(
            "part-mv{}-{:08}-{:08}.part",
            crate::part::PART_VERSION,
            plan.seq_lo,
            plan.seq_hi
        );
        let path = self.dir.join(&file);
        if path.exists() {
            bail!("format migration staging path already exists: {}", path.display());
        }
        let (meta, rewrite) = match crate::part::merge::merge_opts_with_control_for_operation(
            &path,
            &[input],
            self.cfg.level,
            false,
            control,
            "format migration",
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
        let output_bytes = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                let _ = crate::vfs::unlink(&path);
                return Err(error.into());
            }
        };
        let mut manifest = self.manifest.clone();
        manifest.parts[plan.part_index] = PartRef {
            file,
            seq_lo: meta.seq_lo,
            seq_hi: meta.seq_hi,
            records: meta.n_records,
            b3: Some(digest),
        };
        manifest.commit(&self.dir)?;
        self.parts[plan.part_index] = Arc::new(Part::open_in(&path, self.pcache.clone())?);
        self.manifest = manifest;
        sweep_unreachable(&self.dir)?;
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
        read::get(&self.parts, id)
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
        read::reconstruct_content(&self.parts, &self.fold, id, name)
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

    /// ERASE records: tombstone, settle, and rewrite until the content is physically gone.
    ///
    /// This is the compliance path, and it composes three operations that each already existed:
    /// deletes shadow the ids; a TOTAL merge drops the tombstones once nothing remains for them
    /// to shadow; and the re-fold rewrites the fold without the dropped content and rebuilds
    /// every part — so both the bytes AND the columnar metadata (ids, piece lengths, attribute
    /// values) of the erased records are gone when this returns. The re-fold also purges the
    /// retained commit log, which the erasure story REQUIRES: a snapshot that could still serve
    /// the erased record is not erasure.
    ///
    /// What this does NOT promise, stated because overclaiming here is a liability: nothing about
    /// copies outside this store — packs written earlier, replicas, backups. It removes data from
    /// THIS store, and only that.
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
    /// physical erasure would make a retry mistake the ids for previously absent records and falsely
    /// report completion. Erasure therefore either stops before mutation or drives its full safety
    /// protocol to completion.
    pub fn erase_ids_with_control(
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
            return Ok(ErasureStats { requested: ids.len(), tombstoned, absent, refold: None });
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
        Ok(ErasureStats { requested: ids.len(), tombstoned, absent, refold: Some(refold) })
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
            let mut m = self.manifest.clone();
            let mut all: Vec<u32> = already.into_iter().chain(dead.iter().copied()).collect();
            all.sort_unstable();
            m.punched = to_ranges(&all);
            m.commit(&self.dir)?;
            self.manifest = m;
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
                .checked_add(std::fs::metadata(self.dir.join(&part_ref.file))?.len())
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
        let seqs: Vec<(u64, u64)> =
            self.manifest.parts.iter().map(|p| (p.seq_lo, p.seq_hi)).collect();
        let (new_gen, built, mut stats) = refold::refold_with_control(
            &self.dir,
            &self.parts,
            &seqs,
            &self.fold,
            self.manifest.fold_gen,
            self.cfg,
            control,
        )?;

        // Data before pointers, exactly as everywhere else: the new fold and the new parts are durable
        // before the manifest names either, and the manifest swap is the instant it takes effect.
        let new_dir = refold::fold_dir(&self.dir, new_gen);
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
                            blake3::hash(&std::fs::read(self.dir.join(file))?).to_hex().to_string(),
                        ),
                    })
                })
                .collect::<Result<_>>()?;
            m.fold_gen = new_gen;
            // Block ids are PER GENERATION. The new fold was rewritten without erased content and
            // therefore has no holes inherited from the old generation.
            m.punched.clear();
            let f = Fold::open(&new_dir, self.cfg)?;
            let t = f.tail();
            m.fold_seg = t.seg;
            m.fold_off = t.off;
            control.check("content refold")?;
            Ok(m)
        })();
        let mut m = match prepared {
            Ok(manifest) => manifest,
            Err(error) => {
                cleanup_refold_stage(&self.dir, new_gen, &built);
                return Err(error);
            }
        };
        // No cancellation checkpoint after this call begins. A failed commit can have durably
        // written its retained copy, so staged files must remain for ordinary recovery.
        m.commit(&self.dir)?;

        // Everything past here is cleanup: a crash leaves orphans, which open() sweeps.
        let old_gen = self.manifest.fold_gen;
        self.manifest = m;
        // PURGE the retained log down to this commit alone. Erasure semantics trump snapshots: a
        // re-fold exists to make dropped content GONE, and a retained manifest would keep the old
        // generation — deleted records included — readable for MANIFEST_RETAIN more commits.
        // Time travel does not cross a re-fold, by design; that is the point of running one.
        for c in list_retained(&self.dir) {
            if c != self.manifest.commit {
                let _ = crate::vfs::unlink(&retained_path(&self.dir, c));
            }
        }
        let part_cache_budget = self.pcache.budget();
        self.pcache = Arc::new(SectionCache::new(part_cache_budget));
        self.parts.clear();
        for p in &self.manifest.parts {
            self.parts.push(Arc::new(Part::open_in(&self.dir.join(&p.file), self.pcache.clone())?));
        }
        self.fold = Fold::open_at(&new_dir, self.cfg, self.manifest.fold_tail())?;
        sweep_unreachable(&self.dir)?;
        // Reported, not swallowed. Claiming `bytes_reclaimed()` while the old generation still
        // occupies the disk would be a stat that says the opposite of the truth. The re-fold itself
        // is already committed and correct; this is only honest about what is left behind.
        if refold::fold_dir(&self.dir, old_gen).exists() {
            stats.stale_generation_left = true;
        }
        Ok(stats)
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
            dedup_window_entries: self.fold.window_len(),
            retained_commits: retained_commits(&self.dir).len(),
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
        store_space_usage(&self.dir, &self.manifest, control)
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

fn store_space_usage(
    dir: &Path,
    live_manifest: &Manifest,
    control: &crate::control::OperationControl,
) -> Result<StoreSpaceUsage> {
    control.check("store space inventory")?;
    let mut live_files: HashSet<PathBuf> =
        [PathBuf::from("MANIFEST"), PathBuf::from("WAL")].into_iter().collect();
    live_files.extend(live_manifest.parts.iter().map(|part| PathBuf::from(&part.file)));
    let live_fold = fold_relative_dir(dir, live_manifest.fold_gen)?;

    let mut retained_files = HashSet::new();
    let mut retained_folds = HashSet::new();
    for commit in list_retained(dir) {
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
    account_store_files(dir, dir, &reachability, &mut usage, control)?;
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
    dir: &Path,
    reachability: &SpaceReachability<'_>,
    usage: &mut StoreSpaceUsage,
    control: &crate::control::OperationControl,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("read store space directory {}", dir.display()))?
    {
        control.check("store space inventory")?;
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            account_store_files(root, &entry.path(), reachability, usage, control)?;
        } else if file_type.is_file() {
            let relative = relative_store_path(root, &entry.path())?;
            let metadata = entry.metadata()?;
            add_space(&mut usage.total, &metadata)?;
            let live = reachability.live_files.contains(&relative)
                || relative.starts_with(reachability.live_fold);
            let retained = reachability.retained_files.contains(&relative)
                || reachability.retained_folds.iter().any(|fold| relative.starts_with(fold));
            if live {
                add_space(&mut usage.live, &metadata)?;
            } else if retained {
                add_space(&mut usage.retained_only, &metadata)?;
            } else {
                add_space(&mut usage.unclassified, &metadata)?;
            }
        }
    }
    Ok(())
}

fn add_space(amount: &mut SpaceAmount, metadata: &std::fs::Metadata) -> Result<()> {
    amount.files =
        amount.files.checked_add(1).ok_or_else(|| anyhow::anyhow!("store file count overflow"))?;
    amount.logical_bytes = amount
        .logical_bytes
        .checked_add(metadata.len())
        .ok_or_else(|| anyhow::anyhow!("store logical byte count overflow"))?;
    amount.allocated_bytes = match (amount.allocated_bytes, crate::sys::allocated_bytes(metadata)) {
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
}

/// A read-only store IS the committed read core, with nothing layered on top — so every method here
/// is a direct delegation, and there is no second implementation to keep in step.
impl ReadStore {
    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        read::get(&self.parts, id)
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
        read::reconstruct(&self.parts, &self.fold, id)
    }

    /// Byte-exact named content, if both the record and value are present.
    pub fn reconstruct_content(&self, id: &str, name: &str) -> Result<Option<Vec<u8>>> {
        read::reconstruct_content(&self.parts, &self.fold, id, name)
    }

    /// Distinct committed ids, sorted — the union across parts, newest-wins.
    pub fn ids(&self) -> Result<Vec<String>> {
        read::ids(&self.parts)
    }

    /// Bounded structured paging over this immutable manifest snapshot.
    pub fn scan(&self, request: &crate::scan::ScanRequest) -> Result<crate::scan::ScanPage> {
        crate::scan::scan_read_store(self, request)
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
#[cfg(not(any(unix, target_os = "wasi")))]
compile_error!(
    "turndb needs positioned file reads (pread). Unix and WASI provide them; \
     wasm32-unknown-unknown has no filesystem at all — build for wasm32-wasip1 instead."
);

#[cfg(test)]
mod tests {
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
            super::list_retained(&d),
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
        assert_eq!(super::list_retained(&d), vec![3, 4], "the abandoned timeline is cleared");

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
            let meta = crate::part::build_revision_two_fixture(&d.join(&file), seq, id).unwrap();
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
        assert_eq!(plan.source_part_version, 2);
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
        assert_eq!(record.contents[0].name, "payload");
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

    #[test]
    fn revision_three_reference_pack_matches_the_checked_in_node_fixture() {
        let root = std::env::temp_dir().join(format!(
            "turndb-v3-reference-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let d = root.join("store");
        let artifact = root.join("revision-three.turndb");
        std::fs::create_dir_all(&d).unwrap();
        let mut fold =
            crate::fold::Fold::open(&d.join("fold"), crate::fold::FoldCfg::default()).unwrap();
        fold.sync().unwrap();
        let tail = fold.tail();
        drop(fold);
        let mut part_refs = Vec::new();
        for (seq, id, payload) in [
            (1, "legacy/0001", b"revision three request".as_slice()),
            (2, "legacy/0002", b"revision three response".as_slice()),
        ] {
            let file = format!("legacy-{seq}.part");
            let meta = crate::part::build_revision_three_fixture(&d.join(&file), seq, id, payload)
                .unwrap();
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
        store.backup(&artifact).unwrap();
        drop(store);
        let bytes = std::fs::read(&artifact).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let actual = bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        if std::env::var_os("TURNDB_PRINT_REFERENCE_FIXTURE").is_some() {
            println!("{actual}");
            return;
        }
        let expected =
            include_str!("../../bindings/node/qualification/fixtures/revision-three.turndb.hex")
                .split_ascii_whitespace()
                .collect::<String>();
        assert_eq!(
            actual, expected,
            "regenerate only when the legacy fixture intentionally changes"
        );
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
