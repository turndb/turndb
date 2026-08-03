//! Native Node.js interface for TurnDB.
//!
//! The binding translates values and schedules work; the Rust core remains responsible for
//! atomicity, visibility, cursor validity, filtering, ordering, and content reconstruction.

#![deny(rustdoc::broken_intra_doc_links)]

mod actor;

use actor::{Actor, OwnedContent, WriteOp};
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
    }
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
            .map_err(|_| Error::new(Status::GenericFailure, "TurnDB snapshot state is poisoned"))?
            .clone()
            .ok_or_else(|| Error::new(Status::GenericFailure, "TurnDB snapshot is closed"))
    }

    fn close(&self) -> Result<()> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| Error::new(Status::GenericFailure, "TurnDB snapshot state is poisoned"))?;
        if store.take().is_none() {
            return Err(Error::new(Status::GenericFailure, "TurnDB snapshot is already closed"));
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
