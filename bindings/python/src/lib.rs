//! Python binding. One worker owns each mutable store; Python only submits commands.

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyModule};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use turndb::carve::Carve;
use turndb::error::classify as classify_engine_error;
use turndb::fold::FoldCfg;
use turndb::scan::{
    Compare, ContentMode, ContentSelect, Direction, Predicate, ScanExplanation, ScanPage,
    ScanRequest,
};
use turndb::schema::{AttrType, Schema};
use turndb::store::{
    open_read_file, Batch, CompactionBudget, ContentSpans, ReadStore, Store as EngineStore,
};
use turndb::types::AttrValue;

create_exception!(_native, TurnDbError, PyException);
create_exception!(_native, InvalidArgumentError, TurnDbError);
create_exception!(_native, NotFoundError, TurnDbError);
create_exception!(_native, CorruptionError, TurnDbError);
create_exception!(_native, CancelledError, TurnDbError);
create_exception!(_native, UnsupportedError, TurnDbError);
create_exception!(_native, BusyError, TurnDbError);
create_exception!(_native, ClosedError, TurnDbError);

const DEFAULT_QUEUE_CAPACITY: usize = 64;
const MAX_QUEUE_CAPACITY: usize = 65_536;

fn python_error(error: anyhow::Error) -> PyErr {
    let message = format!("{error:#}");
    if message.contains("store command queue is full") {
        return BusyError::new_err(message);
    }
    if message.contains("operation is unavailable on a read-only snapshot") {
        return UnsupportedError::new_err(message);
    }
    match classify_engine_error(&error).code() {
        "INVALID_ARGUMENT" => InvalidArgumentError::new_err(message),
        "NOT_FOUND" => NotFoundError::new_err(message),
        "CORRUPTION" => CorruptionError::new_err(message),
        "CANCELLED" | "DEADLINE_EXCEEDED" => CancelledError::new_err(message),
        "UNSUPPORTED" => UnsupportedError::new_err(message),
        _ if error.chain().any(|cause| cause.to_string() == "store is closed") => {
            ClosedError::new_err(message)
        }
        _ => TurnDbError::new_err(message),
    }
}

fn py_to_value(value: &Bound<'_, PyAny>) -> PyResult<Value> {
    let json_module = PyModule::import_bound(value.py(), "json")?;
    let text: String = json_module.call_method1("dumps", (value,))?.extract()?;
    serde_json::from_str(&text).map_err(|error| InvalidArgumentError::new_err(error.to_string()))
}

fn value_to_py(py: Python<'_>, value: Value) -> PyResult<PyObject> {
    let json_module = PyModule::import_bound(py, "json")?;
    Ok(json_module.call_method1("loads", (value.to_string(),))?.unbind())
}

#[derive(Debug)]
struct OwnedContent {
    name: String,
    bytes: Vec<u8>,
}

#[derive(Debug)]
enum WriteOp {
    Put { id: String, contents: Vec<OwnedContent>, attrs: Vec<(String, AttrValue)> },
    Delete { id: String },
}

enum Operation {
    Write { operations: Vec<WriteOp>, durable: bool },
    Sync,
    Flush,
    Backup(PathBuf),
    Scan(ScanRequest),
    Explain(ScanRequest),
    ReadContent { id: String, name: String },
    Schema,
    Verify,
    SpaceUsage,
    CompactBounded(CompactionBudget),
    Refold,
    Erase(Vec<String>),
}

enum Command {
    Operation { operation: Operation, reply: mpsc::SyncSender<Result<Value>> },
    Snapshot { reply: mpsc::SyncSender<Result<ReadStore>> },
    Close { durable: bool, reply: mpsc::SyncSender<Result<()>> },
}

enum Handle {
    Writer { store: EngineStore, path: PathBuf },
    Reader(ReadStore),
}

struct ActorInner {
    tx: mpsc::SyncSender<Command>,
    closed: AtomicBool,
    capacity: usize,
}

#[derive(Clone)]
struct Actor(Arc<ActorInner>);

impl Actor {
    fn open_writer(path: PathBuf, capacity: usize) -> Result<Actor> {
        if !(1..=MAX_QUEUE_CAPACITY).contains(&capacity) {
            return Err(anyhow!(
                "command queue capacity must be between 1 and {MAX_QUEUE_CAPACITY}, got {capacity}"
            ));
        }
        Self::spawn(capacity, move || {
            let store = EngineStore::open_file(&path, FoldCfg::default())?;
            Ok(Handle::Writer { store, path })
        })
    }

    fn open_reader(path: PathBuf) -> Result<Actor> {
        Self::spawn(DEFAULT_QUEUE_CAPACITY, move || {
            open_read_file(&path, FoldCfg::default()).map(Handle::Reader)
        })
    }

    fn from_reader(reader: ReadStore) -> Result<Actor> {
        Self::spawn(DEFAULT_QUEUE_CAPACITY, move || Ok(Handle::Reader(reader)))
    }

    fn spawn(
        capacity: usize,
        open: impl FnOnce() -> Result<Handle> + Send + 'static,
    ) -> Result<Actor> {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("turndb-python-store".into())
            .spawn(move || match open() {
                Ok(handle) => {
                    let _ = ready_tx.send(Ok(()));
                    run_actor(handle, rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .context("spawn TurnDB Python store actor")?;
        ready_rx.recv().context("TurnDB actor exited during open")??;
        Ok(Actor(Arc::new(ActorInner { tx, closed: AtomicBool::new(false), capacity })))
    }

    fn call(&self, operation: Operation) -> Result<Value> {
        if self.0.closed.load(Ordering::Acquire) {
            return Err(anyhow!("store is closed"));
        }
        let (reply, receive) = mpsc::sync_channel(1);
        self.0.tx.try_send(Command::Operation { operation, reply }).map_err(
            |error| match error {
                mpsc::TrySendError::Full(_) => {
                    anyhow!("store command queue is full (capacity {})", self.0.capacity)
                }
                mpsc::TrySendError::Disconnected(_) => anyhow!("store worker has exited"),
            },
        )?;
        receive.recv().context("store worker exited before replying")?
    }

    fn snapshot(&self) -> Result<ReadStore> {
        if self.0.closed.load(Ordering::Acquire) {
            return Err(anyhow!("store is closed"));
        }
        let (reply, receive) = mpsc::sync_channel(1);
        self.0.tx.send(Command::Snapshot { reply }).context("submit snapshot")?;
        receive.recv().context("store worker exited before snapshot reply")?
    }

    fn close(&self, durable: bool) -> Result<()> {
        if self.0.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let (reply, receive) = mpsc::sync_channel(1);
        self.0.tx.send(Command::Close { durable, reply }).context("submit close")?;
        receive.recv().context("store worker exited before close reply")?
    }
}

fn run_actor(mut handle: Handle, rx: mpsc::Receiver<Command>) {
    while let Ok(command) = rx.recv() {
        match command {
            Command::Operation { operation, reply } => {
                let _ = reply.send(run_operation(&mut handle, operation));
            }
            Command::Snapshot { reply } => {
                let result = match &mut handle {
                    Handle::Writer { store, path } => {
                        let limits = store.read_limits();
                        let cfg = store.fold_cfg();
                        store.flush().and_then(|_| {
                            turndb::store::open_read_container_with_limits(path, cfg, limits)
                        })
                    }
                    Handle::Reader(reader) => Ok(reader.clone()),
                };
                let _ = reply.send(result);
            }
            Command::Close { durable, reply } => {
                let result = match handle {
                    Handle::Writer { mut store, .. } => {
                        if durable {
                            store.sync().and_then(|_| store.close())
                        } else {
                            drop(store);
                            Ok(())
                        }
                    }
                    Handle::Reader(_) => Ok(()),
                };
                let _ = reply.send(result);
                return;
            }
        }
    }
}

fn run_operation(handle: &mut Handle, operation: Operation) -> Result<Value> {
    match operation {
        Operation::Scan(request) => match handle {
            Handle::Writer { store, .. } => store.scan(&request).map(encode_page),
            Handle::Reader(store) => store.scan(&request).map(encode_page),
        },
        Operation::Explain(request) => match handle {
            Handle::Writer { store, .. } => store.explain_scan(&request).map(encode_explanation),
            Handle::Reader(store) => store.explain_scan(&request).map(encode_explanation),
        },
        Operation::ReadContent { id, name } => {
            let bytes = match handle {
                Handle::Writer { store, .. } => store.reconstruct_content(&id, &name),
                Handle::Reader(store) => store.reconstruct_content(&id, &name),
            }?;
            Ok(bytes.map(|bytes| Value::String(BASE64.encode(bytes))).unwrap_or(Value::Null))
        }
        Operation::Schema => match handle {
            Handle::Writer { store, .. } => store.schema().map(encode_schema),
            Handle::Reader(store) => store.schema().map(encode_schema),
        },
        other => {
            let Handle::Writer { store, .. } = handle else {
                return Err(anyhow!("operation is unavailable on a read-only snapshot"));
            };
            run_writer_operation(store, other)
        }
    }
}

fn run_writer_operation(store: &mut EngineStore, operation: Operation) -> Result<Value> {
    match operation {
        Operation::Write { operations, durable } => {
            let applied = operations.len();
            apply(store, operations, durable)?;
            Ok(json!({ "applied": applied, "durable": durable }))
        }
        Operation::Sync => store.sync().map(|_| Value::Null),
        Operation::Flush => store.flush().map(|part| Value::Bool(part.is_some())),
        Operation::Backup(path) => store.backup(&path).map(|result| {
            json!({
                "members": result.members.to_string(),
                "bytes": result.bytes.to_string(),
                "commit": result.commit.to_string(),
            })
        }),
        Operation::Verify => store.verify().map(encode_verification),
        Operation::SpaceUsage => store.space_usage().map(encode_space_usage),
        Operation::CompactBounded(budget) => {
            store.sync()?;
            store.flush()?;
            let before = store.part_count();
            let result = store.compact_bounded(budget)?;
            Ok(json!({
                "partsBefore": before,
                "partsAfter": store.part_count(),
                "compacted": result.is_some(),
                "inputParts": result.map(|value| value.plan.input_parts),
            }))
        }
        Operation::Refold => {
            store.sync()?;
            store.flush()?;
            let result = store.refold()?;
            Ok(json!({
                "partsIn": result.parts_in,
                "partsOut": result.parts_out,
                "recordsKept": result.records_kept,
                "recordsDropped": result.records_dropped,
                "piecesKept": result.pieces_kept,
                "piecesDropped": result.pieces_dropped,
                "foldBytesBefore": result.fold_bytes_before.to_string(),
                "foldBytesAfter": result.fold_bytes_after.to_string(),
                "staleGenerationLeft": result.stale_generation_left,
            }))
        }
        Operation::Erase(ids) => {
            let result = store.erase_ids(&ids)?;
            Ok(json!({
                "requested": result.requested,
                "erased": result.tombstoned,
                "absent": result.absent,
                "remaining": result.remaining,
            }))
        }
        _ => Err(anyhow!("invalid writer operation dispatch")),
    }
}

fn apply(store: &mut EngineStore, operations: Vec<WriteOp>, durable: bool) -> Result<()> {
    let carve = Carve::default();
    let mut batch = Batch::new();
    for operation in operations {
        match operation {
            WriteOp::Put { id, contents, attrs } => {
                let spans: Vec<_> = contents
                    .iter()
                    .map(|content| ContentSpans::carve(&content.name, &content.bytes, &carve))
                    .collect();
                batch.put_record(&id, &spans, attrs)?;
            }
            WriteOp::Delete { id } => batch.delete(&id),
        }
    }
    store.apply(batch)?;
    if durable {
        store.sync()?;
    }
    Ok(())
}

fn decode_write_operations(value: Value) -> Result<Vec<WriteOp>> {
    let operations =
        value.as_array().ok_or_else(|| anyhow!("write operations must be an array"))?;
    operations
        .iter()
        .enumerate()
        .map(|(index, operation)| {
            let kind = required_string(operation, "kind")?;
            let id = required_string(operation, "id")?.to_string();
            match kind {
                "delete" => Ok(WriteOp::Delete { id }),
                "put" => {
                    let attrs = operation
                        .get("attrs")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(decode_attribute)
                        .collect::<Result<Vec<_>>>()?;
                    let contents = operation
                        .get("contents")
                        .and_then(Value::as_array)
                        .ok_or_else(|| anyhow!("write operation {index} needs contents"))?
                        .iter()
                        .map(|content| {
                            Ok(OwnedContent {
                                name: required_string(content, "name")?.to_string(),
                                bytes: BASE64
                                    .decode(required_string(content, "base64")?)
                                    .context("content is not canonical base64")?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok(WriteOp::Put { id, contents, attrs })
                }
                other => Err(anyhow!("unknown write operation kind {other:?}")),
            }
        })
        .collect()
}

fn decode_attribute(attribute: &Value) -> Result<(String, AttrValue)> {
    Ok((
        required_string(attribute, "name")?.to_string(),
        decode_scalar(attribute.get("value").ok_or_else(|| anyhow!("attribute needs value"))?)?,
    ))
}

fn decode_scalar(value: &Value) -> Result<AttrValue> {
    match required_string(value, "type")? {
        "string" => Ok(AttrValue::Str(required_string(value, "value")?.to_string())),
        "i64" => Ok(AttrValue::Int(required_string(value, "decimal")?.parse()?)),
        "f64" => {
            let bits = required_string(value, "bitsHex")?;
            if bits.len() != 16
                || !bits.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(anyhow!("f64 bitsHex must be sixteen lowercase hexadecimal digits"));
            }
            Ok(AttrValue::Float(f64::from_bits(u64::from_str_radix(bits, 16)?)))
        }
        "bool" => Ok(AttrValue::Bool(
            value
                .get("value")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("bool needs value"))?,
        )),
        "u64" => Ok(AttrValue::UInt(required_string(value, "decimal")?.parse()?)),
        "binary" => Ok(AttrValue::Bytes(BASE64.decode(required_string(value, "base64")?)?)),
        "timestampNs" => Ok(AttrValue::TimestampNs(required_string(value, "decimal")?.parse()?)),
        "null" => Ok(AttrValue::Null),
        other => Err(anyhow!("unknown scalar type {other:?}")),
    }
}

fn decode_scan(value: Value) -> Result<ScanRequest> {
    let version = value.get("contractVersion").and_then(Value::as_u64);
    if version != Some(1) {
        return Err(anyhow!("scan request contractVersion must be 1"));
    }
    let mut request = ScanRequest::default();
    request.from = optional_string(&value, "from")?;
    request.to = optional_string(&value, "to")?;
    request.cursor = optional_string(&value, "cursor")?;
    request.direction = match value.get("direction").and_then(Value::as_str).unwrap_or("forward") {
        "forward" => Direction::Forward,
        "reverse" => Direction::Reverse,
        other => return Err(anyhow!("unknown scan direction {other:?}")),
    };
    if let Some(limit) = value.get("limit") {
        request.limit = usize_value(limit, "limit")?;
    }
    if let Some(limit) = value.get("maxExamined") {
        request.max_examined = usize_value(limit, "maxExamined")?;
    }
    if let Some(limit) = value.get("maxResolutionEntries") {
        request.max_resolution_entries = usize_value(limit, "maxResolutionEntries")?;
    }
    if let Some(limit) = value.get("maxReconstructedBytes") {
        request.max_reconstructed_bytes =
            required_value_string(limit, "maxReconstructedBytes")?.parse()?;
    }
    request.attrs = string_array(value.get("attrs"), "attrs")?;
    request.contents = value
        .get("contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|content| {
            Ok(ContentSelect {
                name: required_string(content, "name")?.to_string(),
                mode: match required_string(content, "mode")? {
                    "metadata" => ContentMode::Metadata,
                    "bytes" => ContentMode::Bytes,
                    other => return Err(anyhow!("unknown content mode {other:?}")),
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    request.predicates = value
        .get("predicates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(decode_predicate)
        .collect::<Result<Vec<_>>>()?;
    Ok(request)
}

fn decode_predicate(value: &Value) -> Result<Predicate> {
    match required_string(value, "kind")? {
        "id" => Ok(Predicate::Id {
            op: decode_compare(required_string(value, "op")?)?,
            value: required_string(value, "value")?.to_string(),
        }),
        "attr" => Ok(Predicate::Attr {
            name: required_string(value, "name")?.to_string(),
            op: decode_compare(required_string(value, "op")?)?,
            value: decode_scalar(
                value.get("value").ok_or_else(|| anyhow!("attribute predicate needs value"))?,
            )?,
        }),
        "attrExists" => Ok(Predicate::AttrExists {
            name: required_string(value, "name")?.to_string(),
            present: required_bool(value, "present")?,
        }),
        "contentExists" => Ok(Predicate::ContentExists {
            name: required_string(value, "name")?.to_string(),
            present: required_bool(value, "present")?,
        }),
        other => Err(anyhow!("unknown predicate kind {other:?}")),
    }
}

fn decode_compare(value: &str) -> Result<Compare> {
    match value {
        "eq" => Ok(Compare::Eq),
        "ne" => Ok(Compare::Ne),
        "lt" => Ok(Compare::Lt),
        "lte" => Ok(Compare::LtEq),
        "gt" => Ok(Compare::Gt),
        "gte" => Ok(Compare::GtEq),
        other => Err(anyhow!("unknown comparison {other:?}")),
    }
}

fn encode_scalar(value: &AttrValue) -> Value {
    match value {
        AttrValue::Str(value) => json!({ "type": "string", "value": value }),
        AttrValue::Int(value) => json!({ "type": "i64", "decimal": value.to_string() }),
        AttrValue::Float(value) => {
            json!({ "type": "f64", "bitsHex": format!("{:016x}", value.to_bits()) })
        }
        AttrValue::Bool(value) => json!({ "type": "bool", "value": value }),
        AttrValue::UInt(value) => json!({ "type": "u64", "decimal": value.to_string() }),
        AttrValue::Bytes(value) => json!({ "type": "binary", "base64": BASE64.encode(value) }),
        AttrValue::TimestampNs(value) => {
            json!({ "type": "timestampNs", "decimal": value.to_string() })
        }
        AttrValue::Null => json!({ "type": "null" }),
    }
}

fn encode_page(page: ScanPage) -> Value {
    let rows: Vec<_> = page.rows.into_iter().map(|row| json!({
        "id": row.id,
        "attrs": row.attrs.into_iter().map(|(name, value)| json!({ "name": name, "value": encode_scalar(&value) })).collect::<Vec<_>>(),
        "contents": row.contents.into_iter().map(|content| {
            let mut value = json!({ "name": content.name, "present": content.present });
            if let Some(len) = content.len { value["len"] = Value::String(len.to_string()); }
            if let Some(pieces) = content.pieces { value["pieces"] = Value::String(pieces.to_string()); }
            if let Some(identity) = content.identity { value["identityHex"] = Value::String(identity.to_hex()); }
            if let Some(bytes) = content.bytes { value["base64"] = Value::String(BASE64.encode(bytes)); }
            value
        }).collect::<Vec<_>>(),
    })).collect();
    let mut value = json!({
        "contractVersion": 1,
        "rows": rows,
        "stats": {
            "durationNs": page.stats.duration_ns.to_string(),
            "examined": page.stats.examined.to_string(),
            "returned": page.stats.returned.to_string(),
            "predicatePrunedRows": page.stats.predicate_pruned_rows.to_string(),
            "duplicateAttrOccurrences": page.stats.duplicate_attr_occurrences.to_string(),
            "contentValuesReconstructed": page.stats.content_values_reconstructed.to_string(),
            "reconstructedBytes": page.stats.reconstructed_bytes.to_string(),
            "reconstructionBudgetExhausted": page.stats.reconstruction_budget_exhausted,
            "io": {
                "partSectionsTouched": page.stats.io.part_sections_touched.to_string(),
                "partSectionCacheHits": page.stats.io.part_section_cache_hits.to_string(),
                "partSectionCacheMisses": page.stats.io.part_section_cache_misses.to_string(),
                "partStoredBytesRead": page.stats.io.part_stored_bytes_read.to_string(),
                "partRawBytesDecoded": page.stats.io.part_raw_bytes_decoded.to_string(),
                "foldBlocksTouched": page.stats.io.fold_blocks_touched.to_string(),
                "foldBlockCacheHits": page.stats.io.fold_block_cache_hits.to_string(),
                "foldBlockCacheMisses": page.stats.io.fold_block_cache_misses.to_string(),
                "foldStoredBytesRead": page.stats.io.fold_stored_bytes_read.to_string(),
                "foldRawBytesDecoded": page.stats.io.fold_raw_bytes_decoded.to_string(),
            },
            "resolution": {
                "physicalRows": page.stats.resolution.physical_rows.to_string(),
                "supersededRows": page.stats.resolution.superseded_rows.to_string(),
                "tombstones": page.stats.resolution.tombstones.to_string(),
                "memtableEntries": page.stats.resolution.memtable_entries.to_string(),
                "budgetExhausted": page.stats.resolution.budget_exhausted,
            }
        }
    });
    if let Some(next) = page.next {
        value["next"] = Value::String(next);
    }
    value
}

fn encode_explanation(value: ScanExplanation) -> Value {
    json!({
        "direction": match value.direction { Direction::Forward => "forward", Direction::Reverse => "reverse" },
        "usesCursor": value.uses_cursor,
        "effectiveFrom": value.effective_from,
        "effectiveTo": value.effective_to,
        "emptyRange": value.empty_range,
        "projectedAttrs": value.projected_attrs,
        "requiredAttrs": value.required_attrs,
        "predicateOnlyAttrs": value.predicate_only_attrs,
        "limit": value.limit,
        "maxExamined": value.max_examined,
        "physical": {
            "immutablePartsConsidered": value.physical.immutable_parts_considered.to_string(),
            "immutablePartsWithRows": value.physical.immutable_parts_with_rows.to_string(),
            "immutableRowsInBounds": value.physical.immutable_rows_in_bounds.to_string(),
            "memtableEntriesInBounds": value.physical.memtable_entries_in_bounds.to_string(),
        }
    })
}

fn attr_type(value: AttrType) -> &'static str {
    match value {
        AttrType::String => "string",
        AttrType::Int => "i64",
        AttrType::Float => "f64",
        AttrType::Bool => "bool",
        AttrType::UInt => "u64",
        AttrType::Binary => "binary",
        AttrType::TimestampNs => "timestampNs",
        AttrType::Null => "null",
    }
}

fn encode_schema(schema: Schema) -> Value {
    json!({
        "attributes": schema.attributes.into_iter().map(|attribute| json!({
            "name": attribute.name,
            "types": attribute.types.into_iter().map(attr_type).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "contents": schema.contents,
        "mayIncludeShadowedFields": schema.may_include_shadowed_fields,
    })
}

fn encode_verification(report: turndb::store::StoreVerification) -> Value {
    json!({
        "scope": "current_manifest_revision",
        "state": "valid",
        "parts": report.parts,
        "partSections": report.part_sections,
        "records": report.records,
        "contentValues": report.content_values,
        "contentBytes": report.content_bytes.to_string(),
        "contentIdentities": report.content_identities,
    })
}

fn encode_space_usage(usage: turndb::store::StoreSpaceUsage) -> Value {
    fn amount(value: turndb::store::SpaceAmount) -> Value {
        json!({
            "members": value.members,
            "logicalBytes": value.logical_bytes.to_string(),
            "allocatedBytes": value.allocated_bytes.map(|bytes| bytes.to_string()),
        })
    }
    json!({
        "live": amount(usage.live),
        "retainedOnly": amount(usage.retained_only),
        "unclassified": amount(usage.unclassified),
        "total": amount(usage.total),
        "filesystemAvailableBytes": usage.filesystem_available_bytes.map(|bytes| bytes.to_string()),
    })
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| anyhow!("{field} must be a string"))
}

fn required_value_string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value.as_str().ok_or_else(|| anyhow!("{field} must be decimal text"))
}

fn optional_string(value: &Value, field: &str) -> Result<Option<String>> {
    value
        .get(field)
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| anyhow!("{field} must be a string"))
        })
        .transpose()
}

fn required_bool(value: &Value, field: &str) -> Result<bool> {
    value.get(field).and_then(Value::as_bool).ok_or_else(|| anyhow!("{field} must be a boolean"))
}

fn usize_value(value: &Value, field: &str) -> Result<usize> {
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| anyhow!("{field} must be a non-negative integer"))
}

fn string_array(value: Option<&Value>, field: &str) -> Result<Vec<String>> {
    value.map_or_else(
        || Ok(Vec::new()),
        |value| {
            value
                .as_array()
                .ok_or_else(|| anyhow!("{field} must be an array"))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| anyhow!("{field} values must be strings"))
                })
                .collect()
        },
    )
}

fn run_value(py: Python<'_>, actor: &Actor, operation: Operation) -> PyResult<PyObject> {
    let result = py.allow_threads(|| actor.call(operation)).map_err(python_error)?;
    value_to_py(py, result)
}

fn run_content<'py>(
    py: Python<'py>,
    actor: &Actor,
    id: String,
    name: String,
) -> PyResult<Option<Bound<'py, PyBytes>>> {
    let value = py
        .allow_threads(|| actor.call(Operation::ReadContent { id, name }))
        .map_err(python_error)?;
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(PyBytes::new_bound(
            py,
            &BASE64.decode(value).map_err(|error| TurnDbError::new_err(error.to_string()))?,
        ))),
        _ => Err(TurnDbError::new_err("invalid content response")),
    }
}

#[pyclass(name = "Snapshot")]
struct PySnapshot {
    actor: Actor,
}

#[pymethods]
impl PySnapshot {
    #[staticmethod]
    fn open(py: Python<'_>, path: String) -> PyResult<Self> {
        let actor = py
            .allow_threads(|| Actor::open_reader(Path::new(&path).to_path_buf()))
            .map_err(python_error)?;
        Ok(Self { actor })
    }

    fn scan(&self, py: Python<'_>, request: &Bound<'_, PyAny>) -> PyResult<PyObject> {
        let request = decode_scan(py_to_value(request)?)
            .map_err(|error| InvalidArgumentError::new_err(format!("{error:#}")))?;
        run_value(py, &self.actor, Operation::Scan(request))
    }

    fn explain_scan(&self, py: Python<'_>, request: &Bound<'_, PyAny>) -> PyResult<PyObject> {
        let request = decode_scan(py_to_value(request)?)
            .map_err(|error| InvalidArgumentError::new_err(format!("{error:#}")))?;
        run_value(py, &self.actor, Operation::Explain(request))
    }

    fn read_content<'py>(
        &self,
        py: Python<'py>,
        id: String,
        name: String,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        run_content(py, &self.actor, id, name)
    }

    fn schema(&self, py: Python<'_>) -> PyResult<PyObject> {
        run_value(py, &self.actor, Operation::Schema)
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| self.actor.close(false)).map_err(python_error)
    }
}

#[pyclass(name = "Store")]
struct PyStore {
    actor: Actor,
}

#[pymethods]
impl PyStore {
    #[staticmethod]
    #[pyo3(signature = (path, *, queue_capacity=DEFAULT_QUEUE_CAPACITY))]
    fn open(py: Python<'_>, path: String, queue_capacity: usize) -> PyResult<Self> {
        let actor = py
            .allow_threads(|| Actor::open_writer(PathBuf::from(path), queue_capacity))
            .map_err(python_error)?;
        Ok(Self { actor })
    }

    #[pyo3(signature = (operations, *, durable=false))]
    fn write(
        &self,
        py: Python<'_>,
        operations: &Bound<'_, PyAny>,
        durable: bool,
    ) -> PyResult<PyObject> {
        let operations = decode_write_operations(py_to_value(operations)?)
            .map_err(|error| InvalidArgumentError::new_err(format!("{error:#}")))?;
        run_value(py, &self.actor, Operation::Write { operations, durable })
    }

    fn sync(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| self.actor.call(Operation::Sync)).map(|_| ()).map_err(python_error)
    }

    fn flush(&self, py: Python<'_>) -> PyResult<bool> {
        let value = py.allow_threads(|| self.actor.call(Operation::Flush)).map_err(python_error)?;
        value.as_bool().ok_or_else(|| TurnDbError::new_err("invalid flush response"))
    }

    fn backup(&self, py: Python<'_>, path: String) -> PyResult<PyObject> {
        if path.is_empty() {
            return Err(InvalidArgumentError::new_err("backup path must not be empty"));
        }
        run_value(py, &self.actor, Operation::Backup(PathBuf::from(path)))
    }

    fn scan(&self, py: Python<'_>, request: &Bound<'_, PyAny>) -> PyResult<PyObject> {
        let request = decode_scan(py_to_value(request)?)
            .map_err(|error| InvalidArgumentError::new_err(format!("{error:#}")))?;
        run_value(py, &self.actor, Operation::Scan(request))
    }

    fn explain_scan(&self, py: Python<'_>, request: &Bound<'_, PyAny>) -> PyResult<PyObject> {
        let request = decode_scan(py_to_value(request)?)
            .map_err(|error| InvalidArgumentError::new_err(format!("{error:#}")))?;
        run_value(py, &self.actor, Operation::Explain(request))
    }

    fn read_content<'py>(
        &self,
        py: Python<'py>,
        id: String,
        name: String,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        run_content(py, &self.actor, id, name)
    }

    fn snapshot(&self, py: Python<'_>) -> PyResult<PySnapshot> {
        let reader = py.allow_threads(|| self.actor.snapshot()).map_err(python_error)?;
        let actor = Actor::from_reader(reader).map_err(python_error)?;
        Ok(PySnapshot { actor })
    }

    fn schema(&self, py: Python<'_>) -> PyResult<PyObject> {
        run_value(py, &self.actor, Operation::Schema)
    }

    fn verify(&self, py: Python<'_>) -> PyResult<PyObject> {
        run_value(py, &self.actor, Operation::Verify)
    }

    fn space_usage(&self, py: Python<'_>) -> PyResult<PyObject> {
        run_value(py, &self.actor, Operation::SpaceUsage)
    }

    #[pyo3(signature = (*, max_input_parts, max_input_rows, max_input_bytes))]
    fn compact_bounded(
        &self,
        py: Python<'_>,
        max_input_parts: usize,
        max_input_rows: u64,
        max_input_bytes: u64,
    ) -> PyResult<PyObject> {
        run_value(
            py,
            &self.actor,
            Operation::CompactBounded(CompactionBudget {
                max_input_parts,
                max_input_rows,
                max_input_bytes,
            }),
        )
    }

    fn refold(&self, py: Python<'_>) -> PyResult<PyObject> {
        run_value(py, &self.actor, Operation::Refold)
    }

    fn erase(&self, py: Python<'_>, ids: Vec<String>) -> PyResult<PyObject> {
        run_value(py, &self.actor, Operation::Erase(ids))
    }

    #[pyo3(signature = (*, durable=false))]
    fn close(&self, py: Python<'_>, durable: bool) -> PyResult<()> {
        py.allow_threads(|| self.actor.close(durable)).map_err(python_error)
    }
}

#[pyfunction]
fn capabilities(py: Python<'_>) -> PyResult<PyObject> {
    let compiled = turndb::capabilities::capabilities();
    let operations = vec![
        "openWriter",
        "openSnapshot",
        "compiledCapabilities",
        "write",
        "sync",
        "flush",
        "scan",
        "explainScan",
        "schema",
        "readContent",
        "snapshot",
        "verify",
        "spaceUsage",
        "compactBounded",
        "refold",
        "erase",
        "close",
        "backup",
    ];
    value_to_py(
        py,
        json!({
            "contractVersion": 2,
            "profile": "native",
            "operations": operations,
            "draftFormatEpoch": compiled.draft_format_epoch,
            "writerExclusion": "os_enforced",
            "positionedIo": compiled.positioned_io,
            "threads": true,
            "columnar": false,
            "sql": false,
            "arrowIpc": false,
            "reclamation": if compiled.in_place_deallocation { "content_punch_or_refold" } else { "refold_only" },
            "cancellation": { "scan": false, "lifecycle": false },
            "binding": "python",
            "actorQueueDefault": DEFAULT_QUEUE_CAPACITY,
            "actorQueueMaximum": MAX_QUEUE_CAPACITY,
        }),
    )
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyStore>()?;
    module.add_class::<PySnapshot>()?;
    module.add_function(wrap_pyfunction!(capabilities, module)?)?;
    module.add("TurnDbError", module.py().get_type_bound::<TurnDbError>())?;
    module.add("InvalidArgumentError", module.py().get_type_bound::<InvalidArgumentError>())?;
    module.add("NotFoundError", module.py().get_type_bound::<NotFoundError>())?;
    module.add("CorruptionError", module.py().get_type_bound::<CorruptionError>())?;
    module.add("CancelledError", module.py().get_type_bound::<CancelledError>())?;
    module.add("UnsupportedError", module.py().get_type_bound::<UnsupportedError>())?;
    module.add("BusyError", module.py().get_type_bound::<BusyError>())?;
    module.add("ClosedError", module.py().get_type_bound::<ClosedError>())?;
    Ok(())
}
