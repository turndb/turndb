//! Native Node.js interface for TurnDB.
//!
//! The binding translates values and schedules work; the Rust core remains responsible for
//! atomicity, visibility, cursor validity, filtering, ordering, and content reconstruction.

#![deny(rustdoc::broken_intra_doc_links)]

mod actor;

use actor::{Actor, CompactResult, OwnedContent, VerifyResult, WriteOp};
use napi::bindgen_prelude::{BigInt, Buffer};
use napi::{Error, Result, Status};
use napi_derive::napi;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use turndb::fold::FoldCfg;
use turndb::scan::{
    Compare, ContentMode, ContentSelect, Direction, Predicate, ProjectedContent, ScanPage,
    ScanRequest, ScanRow,
};
use turndb::store::{ReadStore, Store};
use turndb::types::AttrValue;

fn failure(context: &str, error: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, format!("{context}: {error:#}"))
}

fn coded_failure(code: &str, reason: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, format!("[TURNDB_CODE:{code}] {reason}"))
}

#[napi(object)]
pub struct NativeAttr {
    pub name: String,
    /// `string`, `int`, `float`, or `bool`.
    pub kind: String,
    pub string_value: Option<String>,
    pub int_value: Option<BigInt>,
    pub float_value: Option<f64>,
    pub bool_value: Option<bool>,
}

#[napi(object)]
pub struct NativeContent {
    pub name: String,
    pub bytes: Buffer,
}

#[napi(object)]
pub struct NativeWriteOp {
    /// `put` or `delete`.
    pub kind: String,
    pub id: String,
    pub contents: Option<Vec<NativeContent>>,
    /// Ordered array; duplicate names are preserved.
    pub attrs: Option<Vec<NativeAttr>>,
}

#[napi(object)]
pub struct NativeContentSelect {
    pub name: String,
    /// `metadata` or `bytes`.
    pub mode: String,
}

#[napi(object)]
pub struct NativePredicate {
    /// `id`, `attr`, `attr_exists`, or `content_exists`.
    pub kind: String,
    /// `eq`, `ne`, `lt`, `lte`, `gt`, or `gte` for value predicates.
    pub op: Option<String>,
    pub name: Option<String>,
    pub value: Option<NativeAttr>,
    pub id_value: Option<String>,
    pub present: Option<bool>,
}

#[napi(object)]
pub struct NativeScanRequest {
    pub from: Option<String>,
    pub to: Option<String>,
    /// `forward` or `reverse`.
    pub direction: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub max_examined: Option<u32>,
    pub attrs: Option<Vec<String>>,
    pub contents: Option<Vec<NativeContentSelect>>,
    pub predicates: Option<Vec<NativePredicate>>,
}

#[napi(object)]
pub struct NativeProjectedContent {
    pub name: String,
    pub present: bool,
    pub len: Option<BigInt>,
    pub pieces: Option<u32>,
    pub bytes: Option<Buffer>,
}

#[napi(object)]
pub struct NativeScanRow {
    pub id: String,
    pub attrs: Vec<NativeAttr>,
    pub contents: Vec<NativeProjectedContent>,
}

#[napi(object)]
pub struct NativeScanStats {
    pub examined: u32,
    pub returned: u32,
    pub shadowed_attr_occurrences: u32,
    pub content_values_reconstructed: u32,
    pub reconstructed_bytes: BigInt,
}

#[napi(object)]
pub struct NativeScanPage {
    pub rows: Vec<NativeScanRow>,
    pub next: Option<String>,
    pub stats: NativeScanStats,
}

#[napi(object)]
pub struct NativeCapabilities {
    pub part_format_write: u8,
    pub part_format_read_max: u8,
    pub writer_exclusion: String,
    pub physical_erasure: String,
    pub positioned_io: bool,
    pub threads: bool,
    pub columnar: bool,
    pub sql: bool,
    pub portable_wasm: bool,
    pub native_node: bool,
    pub napi_version: u8,
    pub command_queue_capacity: u32,
    pub immutable_snapshots: bool,
    pub lifecycle_operations: bool,
    pub health_snapshots: bool,
    pub schema_discovery: bool,
}

#[napi]
pub fn capabilities() -> NativeCapabilities {
    let c = turndb::capabilities::capabilities();
    NativeCapabilities {
        part_format_write: c.part_format_write,
        part_format_read_max: c.part_format_read_max,
        writer_exclusion: match c.writer_exclusion {
            turndb::capabilities::WriterExclusion::OsEnforced => "os_enforced",
            turndb::capabilities::WriterExclusion::EmbedderEnforced => "embedder_enforced",
        }
        .into(),
        physical_erasure: match c.physical_erasure {
            turndb::capabilities::PhysicalErasure::PunchOrRefold => "punch_or_refold",
            turndb::capabilities::PhysicalErasure::RefoldOnly => "refold_only",
        }
        .into(),
        positioned_io: c.positioned_io,
        threads: c.threads,
        columnar: c.columnar,
        sql: c.sql,
        portable_wasm: c.portable_wasm,
        native_node: true,
        napi_version: 6,
        command_queue_capacity: 64,
        immutable_snapshots: true,
        lifecycle_operations: true,
        health_snapshots: true,
        schema_discovery: true,
    }
}

#[napi(object)]
pub struct NativeMergeStats {
    pub inputs: BigInt,
    pub records_in: BigInt,
    pub records_out: BigInt,
    pub superseded: BigInt,
    pub tombstones_kept: BigInt,
    pub tombstones_dropped: BigInt,
    pub fold_bytes_touched: BigInt,
}

#[napi(object)]
pub struct NativeCompactResult {
    pub flushed: bool,
    pub parts_before: BigInt,
    pub parts_after: BigInt,
    pub merge: Option<NativeMergeStats>,
}

#[napi(object)]
pub struct NativeVerifyResult {
    pub manifest_links: BigInt,
    pub part_digests: BigInt,
    pub undigested_parts: BigInt,
    pub parts: BigInt,
    pub part_sections: BigInt,
    pub fold_segments: u32,
    pub fold_blocks: BigInt,
    pub fold_bytes: BigInt,
    pub trailing_uncommitted_bytes: BigInt,
}

#[napi(object)]
pub struct NativePunchResult {
    pub blocks_examined: BigInt,
    pub blocks_punched: BigInt,
}

#[napi(object)]
pub struct NativeRefoldResult {
    pub parts_in: BigInt,
    pub parts_out: BigInt,
    pub records_kept: BigInt,
    pub records_dropped: BigInt,
    pub tombstones_dropped: BigInt,
    pub pieces_kept: BigInt,
    pub pieces_dropped: BigInt,
    pub fold_bytes_before: BigInt,
    pub fold_bytes_after: BigInt,
    pub bytes_reclaimed: BigInt,
    pub stale_generation_left: bool,
}

#[napi(object)]
pub struct NativeEraseResult {
    pub requested: BigInt,
    pub tombstoned: BigInt,
    pub absent: BigInt,
    pub refold: Option<NativeRefoldResult>,
}

#[napi(object)]
pub struct NativeHealth {
    pub commit: BigInt,
    pub fold_generation: u32,
    pub parts: BigInt,
    pub part_rows: BigInt,
    pub memtable_entries: BigInt,
    pub memtable_bytes: BigInt,
    pub wal_bytes: BigInt,
    pub fold_disk_bytes: BigInt,
    pub fold_segments: u32,
    pub fold_cache_hits: BigInt,
    pub fold_cache_misses: BigInt,
    pub part_cache_bytes: BigInt,
    pub part_cache_budget: BigInt,
    pub dedup_window_entries: BigInt,
    pub retained_commits: BigInt,
    pub punched_blocks: BigInt,
}

#[napi(object)]
pub struct NativeAttributeSchema {
    pub name: String,
    pub types: Vec<String>,
}

#[napi(object)]
pub struct NativeSchema {
    pub attributes: Vec<NativeAttributeSchema>,
    pub contents: Vec<String>,
    pub may_include_shadowed_fields: bool,
}

struct SnapshotState {
    store: Mutex<Option<Arc<ReadStore>>>,
    commit: u64,
}

impl SnapshotState {
    fn new(store: ReadStore) -> SnapshotState {
        let commit = store.manifest().commit;
        SnapshotState { store: Mutex::new(Some(Arc::new(store))), commit }
    }

    fn get(&self) -> Result<Arc<ReadStore>> {
        self.store
            .lock()
            .map_err(|_| coded_failure("INTERNAL", "TurnDB snapshot state is poisoned"))?
            .clone()
            .ok_or_else(|| coded_failure("CLOSED", "TurnDB snapshot is closed"))
    }

    fn close(&self) -> Result<()> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| coded_failure("INTERNAL", "TurnDB snapshot state is poisoned"))?;
        if store.take().is_none() {
            return Err(coded_failure("CLOSED", "TurnDB snapshot is already closed"));
        }
        Ok(())
    }
}

/// An immutable manifest snapshot. Independent operations may execute concurrently.
#[napi]
pub struct NativeSnapshot {
    state: Arc<SnapshotState>,
}

impl NativeSnapshot {
    fn from_store(store: ReadStore) -> NativeSnapshot {
        NativeSnapshot { state: Arc::new(SnapshotState::new(store)) }
    }
}

#[napi]
impl NativeSnapshot {
    /// Open the currently published manifest without taking the writer lock or replaying its WAL.
    #[napi(factory)]
    pub async fn open(path: String) -> Result<NativeSnapshot> {
        if path.is_empty() {
            return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
        }
        let store = napi::tokio::task::spawn_blocking(move || {
            Store::open_read(&PathBuf::from(path), FoldCfg::default())
        })
        .await
        .map_err(|error| failure("join TurnDB snapshot open", error))?
        .map_err(|error| failure("open TurnDB snapshot", error))?;
        Ok(NativeSnapshot::from_store(store))
    }

    /// Open one retained manifest commit. Retention is bounded and erasure can purge history.
    #[napi(factory)]
    pub async fn open_at(path: String, commit: BigInt) -> Result<NativeSnapshot> {
        if path.is_empty() {
            return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
        }
        let commit = decode_u64(commit, "snapshot commit")?;
        let store = napi::tokio::task::spawn_blocking(move || {
            Store::open_read_at(&PathBuf::from(path), FoldCfg::default(), commit)
        })
        .await
        .map_err(|error| failure("join retained TurnDB snapshot open", error))?
        .map_err(|error| failure("open retained TurnDB snapshot", error))?;
        Ok(NativeSnapshot::from_store(store))
    }

    #[napi(getter)]
    pub fn commit(&self) -> BigInt {
        BigInt::from(self.state.commit)
    }

    #[napi]
    pub async fn scan(&self, request: Option<NativeScanRequest>) -> Result<NativeScanPage> {
        let request = request.map(decode_scan).transpose()?.unwrap_or_default();
        let store = self.state.get()?;
        napi::tokio::task::spawn_blocking(move || store.scan(&request))
            .await
            .map_err(|error| failure("join TurnDB snapshot scan", error))?
            .map(encode_page)
            .map_err(|error| failure("scan TurnDB snapshot", error))
    }

    #[napi]
    pub async fn read_content(&self, id: String, name: String) -> Result<Option<Buffer>> {
        let store = self.state.get()?;
        napi::tokio::task::spawn_blocking(move || store.reconstruct_content(&id, &name))
            .await
            .map_err(|error| failure("join TurnDB snapshot content read", error))?
            .map(|bytes| bytes.map(Buffer::from))
            .map_err(|error| failure("read TurnDB snapshot content", error))
    }

    /// Discover field names and scalar types from metadata without decoding values or content.
    #[napi]
    pub async fn schema(&self) -> Result<NativeSchema> {
        let store = self.state.get()?;
        napi::tokio::task::spawn_blocking(move || store.schema())
            .await
            .map_err(|error| failure("join TurnDB snapshot schema discovery", error))?
            .map(encode_schema)
            .map_err(|error| failure("discover TurnDB snapshot schema", error))
    }

    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.state.close()
    }
}

/// Retained commits currently available to [`NativeSnapshot::open_at`].
#[napi]
pub async fn retained_commits(path: String) -> Result<Vec<BigInt>> {
    if path.is_empty() {
        return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
    }
    napi::tokio::task::spawn_blocking(move || turndb::store::retained_commits(&PathBuf::from(path)))
        .await
        .map(|commits| commits.into_iter().map(BigInt::from).collect())
        .map_err(|error| failure("list retained TurnDB commits", error))
}

/// A native writer handle. All operations are asynchronous and serialized by its Rust actor.
#[napi]
pub struct NativeStore {
    actor: Actor,
}

#[napi]
impl NativeStore {
    /// Open a writer. Resolves only after recovery and writer-lock acquisition complete.
    #[napi(factory)]
    pub async fn open(path: String) -> Result<NativeStore> {
        if path.is_empty() {
            return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
        }
        let actor = napi::tokio::task::spawn_blocking(move || Actor::open(&PathBuf::from(path)))
            .await
            .map_err(|error| failure("join TurnDB open", error))?
            .map_err(|error| failure("open TurnDB store", error))?;
        Ok(NativeStore { actor })
    }

    /// Apply an ordered batch atomically. `durable=true` syncs the WAL before resolving.
    #[napi]
    pub async fn write(&self, ops: Vec<NativeWriteOp>, durable: Option<bool>) -> Result<()> {
        let ops = ops.into_iter().map(decode_write).collect::<Result<Vec<_>>>()?;
        self.actor
            .write(ops, durable.unwrap_or(false))
            .await
            .map_err(|error| failure("write TurnDB batch", error))
    }

    /// Make every previously accepted write crash-durable.
    #[napi]
    pub async fn sync(&self) -> Result<()> {
        self.actor.sync().await.map_err(|error| failure("sync TurnDB store", error))
    }

    /// Seal the current memtable into an immutable part. Returns whether a part was written.
    #[napi]
    pub async fn flush(&self) -> Result<bool> {
        self.actor.flush().await.map_err(|error| failure("flush TurnDB store", error))
    }

    /// Page the writer's read-your-writes view using an opaque, checked cursor.
    #[napi]
    pub async fn scan(&self, request: Option<NativeScanRequest>) -> Result<NativeScanPage> {
        let request = request.map(decode_scan).transpose()?.unwrap_or_default();
        self.actor
            .scan(request)
            .await
            .map(encode_page)
            .map_err(|error| failure("scan TurnDB store", error))
    }

    /// Reconstruct one named content value without reading its siblings.
    #[napi]
    pub async fn read_content(&self, id: String, name: String) -> Result<Option<Buffer>> {
        self.actor
            .read_content(id, name)
            .await
            .map(|bytes| bytes.map(Buffer::from))
            .map_err(|error| failure("read TurnDB content", error))
    }

    /// Publish all earlier accepted writes and return an immutable reader at that exact cut.
    #[napi]
    pub async fn snapshot(&self) -> Result<NativeSnapshot> {
        self.actor
            .snapshot()
            .await
            .map(NativeSnapshot::from_store)
            .map_err(|error| failure("create TurnDB snapshot", error))
    }

    /// Settle earlier writes and compact. `full=true` merges every live part; false uses policy.
    #[napi]
    pub async fn compact(&self, full: Option<bool>) -> Result<NativeCompactResult> {
        self.actor
            .compact(full.unwrap_or(false))
            .await
            .map(encode_compact)
            .map_err(|error| failure("compact TurnDB store", error))
    }

    /// Settle earlier writes, then verify manifest pins, every part section, and every fold frame.
    #[napi]
    pub async fn verify(&self) -> Result<NativeVerifyResult> {
        self.actor
            .verify()
            .await
            .map(encode_verify)
            .map_err(|error| failure("verify TurnDB store", error))
    }

    /// Physically erase ids from this store, including retained history. External copies are out of scope.
    #[napi]
    pub async fn erase(&self, ids: Vec<String>) -> Result<NativeEraseResult> {
        self.actor
            .erase(ids)
            .await
            .map(|stats| NativeEraseResult {
                requested: BigInt::from(stats.requested as u64),
                tombstoned: BigInt::from(stats.tombstoned as u64),
                absent: BigInt::from(stats.absent as u64),
                refold: stats.refold.map(encode_refold),
            })
            .map_err(|error| failure("erase TurnDB records", error))
    }

    /// Reclaim unreachable fold blocks in place where the platform supports punching.
    #[napi]
    pub async fn punch(&self) -> Result<NativePunchResult> {
        self.actor
            .punch()
            .await
            .map(|stats| NativePunchResult {
                blocks_examined: BigInt::from(stats.blocks_examined as u64),
                blocks_punched: BigInt::from(stats.blocks_punched as u64),
            })
            .map_err(|error| failure("punch unreferenced TurnDB content", error))
    }

    /// Rewrite all live content into a new fold generation and purge retained history.
    #[napi]
    pub async fn refold(&self) -> Result<NativeRefoldResult> {
        self.actor
            .refold()
            .await
            .map(encode_refold)
            .map_err(|error| failure("refold TurnDB store", error))
    }

    /// Return cheap operational counters without decoding records or content.
    #[napi]
    pub async fn health(&self) -> Result<NativeHealth> {
        self.actor
            .health()
            .await
            .map(encode_health)
            .map_err(|error| failure("read TurnDB health", error))
    }

    /// Discover the part field universe plus accepted writer-memtable fields.
    #[napi]
    pub async fn schema(&self) -> Result<NativeSchema> {
        self.actor
            .schema()
            .await
            .map(encode_schema)
            .map_err(|error| failure("discover TurnDB schema", error))
    }

    /// Close the handle. Durability defaults to true; pass false only for an explicit no-sync close.
    #[napi]
    pub async fn close(&self, durable: Option<bool>) -> Result<()> {
        self.actor
            .close(durable.unwrap_or(true))
            .await
            .map_err(|error| failure("close TurnDB store", error))
    }
}

fn decode_write(op: NativeWriteOp) -> Result<WriteOp> {
    match op.kind.as_str() {
        "put" => Ok(WriteOp::Put {
            id: op.id,
            contents: op
                .contents
                .unwrap_or_default()
                .into_iter()
                .map(|content| OwnedContent { name: content.name, bytes: content.bytes.to_vec() })
                .collect(),
            attrs: op
                .attrs
                .unwrap_or_default()
                .into_iter()
                .map(decode_attr)
                .collect::<Result<Vec<_>>>()?,
        }),
        "delete" => {
            if op.contents.is_some() || op.attrs.is_some() {
                return Err(Error::new(
                    Status::InvalidArg,
                    "delete write operation must not carry contents or attrs",
                ));
            }
            Ok(WriteOp::Delete { id: op.id })
        }
        other => Err(Error::new(
            Status::InvalidArg,
            format!("unknown write operation kind {other:?}; expected put or delete"),
        )),
    }
}

fn decode_attr(attr: NativeAttr) -> Result<(String, AttrValue)> {
    let NativeAttr { name, kind, string_value, int_value, float_value, bool_value } = attr;
    let supplied = u8::from(string_value.is_some())
        + u8::from(int_value.is_some())
        + u8::from(float_value.is_some())
        + u8::from(bool_value.is_some());
    if supplied != 1 {
        return Err(Error::new(
            Status::InvalidArg,
            format!("attribute {name:?} must carry exactly one typed value"),
        ));
    }
    let missing = |field: &str| {
        Error::new(Status::InvalidArg, format!("attribute {name:?} of kind {kind:?} needs {field}"))
    };
    let value = match kind.as_str() {
        "string" => AttrValue::Str(string_value.ok_or_else(|| missing("stringValue"))?),
        "int" => {
            let value = int_value.ok_or_else(|| missing("intValue"))?;
            let (value, lossless) = value.get_i64();
            if !lossless {
                return Err(Error::new(
                    Status::InvalidArg,
                    "intValue is outside the signed i64 range",
                ));
            }
            AttrValue::Int(value)
        }
        "float" => AttrValue::Float(float_value.ok_or_else(|| missing("floatValue"))?),
        "bool" => AttrValue::Bool(bool_value.ok_or_else(|| missing("boolValue"))?),
        other => {
            return Err(Error::new(
                Status::InvalidArg,
                format!("attribute {name:?} has unknown kind {other:?}"),
            ))
        }
    };
    Ok((name, value))
}

fn decode_scan(input: NativeScanRequest) -> Result<ScanRequest> {
    let mut request = ScanRequest {
        from: input.from,
        to: input.to,
        direction: match input.direction.as_deref().unwrap_or("forward") {
            "forward" => Direction::Forward,
            "reverse" => Direction::Reverse,
            other => {
                return Err(Error::new(
                    Status::InvalidArg,
                    format!("unknown scan direction {other:?}; expected forward or reverse"),
                ))
            }
        },
        cursor: input.cursor,
        attrs: input.attrs.unwrap_or_default(),
        contents: input
            .contents
            .unwrap_or_default()
            .into_iter()
            .map(|content| {
                Ok(ContentSelect {
                    name: content.name,
                    mode: match content.mode.as_str() {
                        "metadata" => ContentMode::Metadata,
                        "bytes" => ContentMode::Bytes,
                        other => {
                            return Err(Error::new(
                                Status::InvalidArg,
                                format!(
                                    "unknown content projection mode {other:?}; expected metadata or bytes"
                                ),
                            ))
                        }
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?,
        predicates: input
            .predicates
            .unwrap_or_default()
            .into_iter()
            .map(decode_predicate)
            .collect::<Result<Vec<_>>>()?,
        ..ScanRequest::default()
    };
    if let Some(limit) = input.limit {
        request.limit = limit as usize;
    }
    if let Some(max_examined) = input.max_examined {
        request.max_examined = max_examined as usize;
    }
    Ok(request)
}

fn decode_predicate(input: NativePredicate) -> Result<Predicate> {
    match input.kind.as_str() {
        "id" => Ok(Predicate::Id {
            op: decode_compare(required(input.op, "id predicate needs op")?)?,
            value: required(input.id_value, "id predicate needs idValue")?,
        }),
        "attr" => {
            let op = decode_compare(required(input.op, "attr predicate needs op")?)?;
            let (name, value) = decode_attr(required(input.value, "attr predicate needs value")?)?;
            Ok(Predicate::Attr { name, op, value })
        }
        "attr_exists" => Ok(Predicate::AttrExists {
            name: required(input.name, "attr_exists predicate needs name")?,
            present: required(input.present, "attr_exists predicate needs present")?,
        }),
        "content_exists" => Ok(Predicate::ContentExists {
            name: required(input.name, "content_exists predicate needs name")?,
            present: required(input.present, "content_exists predicate needs present")?,
        }),
        other => Err(Error::new(
            Status::InvalidArg,
            format!(
                "unknown predicate kind {other:?}; expected id, attr, attr_exists, or content_exists"
            ),
        )),
    }
}

fn required<T>(value: Option<T>, message: &str) -> Result<T> {
    value.ok_or_else(|| Error::new(Status::InvalidArg, message))
}

fn decode_u64(value: BigInt, what: &str) -> Result<u64> {
    let (negative, value, lossless) = value.get_u64();
    if negative || !lossless {
        return Err(Error::new(Status::InvalidArg, format!("{what} is outside the u64 range")));
    }
    Ok(value)
}

fn decode_compare(op: String) -> Result<Compare> {
    match op.as_str() {
        "eq" => Ok(Compare::Eq),
        "ne" => Ok(Compare::Ne),
        "lt" => Ok(Compare::Lt),
        "lte" => Ok(Compare::LtEq),
        "gt" => Ok(Compare::Gt),
        "gte" => Ok(Compare::GtEq),
        other => Err(Error::new(
            Status::InvalidArg,
            format!("unknown comparison {other:?}; expected eq, ne, lt, lte, gt, or gte"),
        )),
    }
}

fn encode_attr((name, value): (String, AttrValue)) -> NativeAttr {
    let mut attr = NativeAttr {
        name,
        kind: String::new(),
        string_value: None,
        int_value: None,
        float_value: None,
        bool_value: None,
    };
    match value {
        AttrValue::Str(value) => {
            attr.kind = "string".into();
            attr.string_value = Some(value);
        }
        AttrValue::Int(value) => {
            attr.kind = "int".into();
            attr.int_value = Some(BigInt::from(value));
        }
        AttrValue::Float(value) => {
            attr.kind = "float".into();
            attr.float_value = Some(value);
        }
        AttrValue::Bool(value) => {
            attr.kind = "bool".into();
            attr.bool_value = Some(value);
        }
    }
    attr
}

fn encode_content(content: ProjectedContent) -> NativeProjectedContent {
    NativeProjectedContent {
        name: content.name,
        present: content.present,
        len: content.len.map(BigInt::from),
        pieces: content.pieces.map(|pieces| pieces as u32),
        bytes: content.bytes.map(Buffer::from),
    }
}

fn encode_row(row: ScanRow) -> NativeScanRow {
    NativeScanRow {
        id: row.id,
        attrs: row.attrs.into_iter().map(encode_attr).collect(),
        contents: row.contents.into_iter().map(encode_content).collect(),
    }
}

fn encode_page(page: ScanPage) -> NativeScanPage {
    NativeScanPage {
        rows: page.rows.into_iter().map(encode_row).collect(),
        next: page.next,
        stats: NativeScanStats {
            examined: page.stats.examined as u32,
            returned: page.stats.returned as u32,
            shadowed_attr_occurrences: page.stats.shadowed_attr_occurrences as u32,
            content_values_reconstructed: page.stats.content_values_reconstructed as u32,
            reconstructed_bytes: BigInt::from(page.stats.reconstructed_bytes),
        },
    }
}

fn encode_merge(stats: turndb::part::merge::MergeStats) -> NativeMergeStats {
    NativeMergeStats {
        inputs: BigInt::from(stats.inputs as u64),
        records_in: BigInt::from(stats.records_in as u64),
        records_out: BigInt::from(stats.records_out as u64),
        superseded: BigInt::from(stats.superseded as u64),
        tombstones_kept: BigInt::from(stats.tombstones_kept as u64),
        tombstones_dropped: BigInt::from(stats.tombstones_dropped as u64),
        fold_bytes_touched: BigInt::from(stats.fold_bytes_touched),
    }
}

fn encode_compact(result: CompactResult) -> NativeCompactResult {
    NativeCompactResult {
        flushed: result.flushed,
        parts_before: BigInt::from(result.parts_before as u64),
        parts_after: BigInt::from(result.parts_after as u64),
        merge: result.merge.map(encode_merge),
    }
}

fn encode_verify(result: VerifyResult) -> NativeVerifyResult {
    NativeVerifyResult {
        manifest_links: BigInt::from(result.chain.links as u64),
        part_digests: BigInt::from(result.chain.part_digests as u64),
        undigested_parts: BigInt::from(result.chain.undigested as u64),
        parts: BigInt::from(result.parts as u64),
        part_sections: BigInt::from(result.part_sections as u64),
        fold_segments: result.fold.segments,
        fold_blocks: BigInt::from(result.fold.blocks as u64),
        fold_bytes: BigInt::from(result.fold.bytes),
        trailing_uncommitted_bytes: BigInt::from(result.fold.trailing_uncommitted),
    }
}

fn encode_refold(stats: turndb::store::refold::RefoldStats) -> NativeRefoldResult {
    NativeRefoldResult {
        parts_in: BigInt::from(stats.parts_in as u64),
        parts_out: BigInt::from(stats.parts_out as u64),
        records_kept: BigInt::from(stats.records_kept as u64),
        records_dropped: BigInt::from(stats.records_dropped as u64),
        tombstones_dropped: BigInt::from(stats.tombstones_dropped as u64),
        pieces_kept: BigInt::from(stats.pieces_kept as u64),
        pieces_dropped: BigInt::from(stats.pieces_dropped as u64),
        fold_bytes_before: BigInt::from(stats.fold_bytes_before),
        fold_bytes_after: BigInt::from(stats.fold_bytes_after),
        bytes_reclaimed: BigInt::from(stats.bytes_reclaimed()),
        stale_generation_left: stats.stale_generation_left,
    }
}

fn encode_health(health: turndb::store::StoreHealth) -> NativeHealth {
    NativeHealth {
        commit: BigInt::from(health.commit),
        fold_generation: health.fold_generation,
        parts: BigInt::from(health.parts as u64),
        part_rows: BigInt::from(health.part_rows),
        memtable_entries: BigInt::from(health.memtable_entries as u64),
        memtable_bytes: BigInt::from(health.memtable_bytes as u64),
        wal_bytes: BigInt::from(health.wal_bytes),
        fold_disk_bytes: BigInt::from(health.fold_disk_bytes),
        fold_segments: health.fold_segments,
        fold_cache_hits: BigInt::from(health.fold_cache_hits),
        fold_cache_misses: BigInt::from(health.fold_cache_misses),
        part_cache_bytes: BigInt::from(health.part_cache_bytes as u64),
        part_cache_budget: BigInt::from(health.part_cache_budget as u64),
        dedup_window_entries: BigInt::from(health.dedup_window_entries as u64),
        retained_commits: BigInt::from(health.retained_commits as u64),
        punched_blocks: BigInt::from(health.punched_blocks),
    }
}

fn encode_schema(schema: turndb::schema::Schema) -> NativeSchema {
    NativeSchema {
        attributes: schema
            .attributes
            .into_iter()
            .map(|attribute| NativeAttributeSchema {
                name: attribute.name,
                types: attribute
                    .types
                    .into_iter()
                    .map(|kind| {
                        match kind {
                            turndb::schema::AttrType::String => "string",
                            turndb::schema::AttrType::Int => "int",
                            turndb::schema::AttrType::Float => "float",
                            turndb::schema::AttrType::Bool => "bool",
                        }
                        .to_string()
                    })
                    .collect(),
            })
            .collect(),
        contents: schema.contents,
        may_include_shadowed_fields: schema.may_include_shadowed_fields,
    }
}
