//! Native Node.js interface for TurnDB.
//!
//! The binding translates values and schedules work; the Rust core remains responsible for
//! atomicity, visibility, cursor validity, filtering, ordering, and content reconstruction.

#![deny(rustdoc::broken_intra_doc_links)]

mod actor;

use actor::{
    Actor, CompactResult, OwnedContent, VerifyResult, WriteOp, DEFAULT_QUEUE_CAPACITY,
    MAX_QUEUE_CAPACITY,
};
use napi::bindgen_prelude::{AbortSignal, BigInt, Buffer, PromiseRaw};
use napi::{Env, Error, Result, Status};
use napi_derive::napi;
use std::path::PathBuf;
#[cfg(feature = "sql")]
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use turndb::control::{OperationControl, OperationInterrupted};
use turndb::fold::FoldCfg;
#[cfg(feature = "sql")]
use turndb::query::sql::{
    classify_error as classify_sql_error, SqlBatch, SqlBudget, SqlErrorClass, SqlOptions, SqlQuery,
    SqlValue, DEFAULT_AGGREGATE_MEMORY_BYTES, DEFAULT_MEMORY_BYTES,
};
use turndb::scan::{
    CancellationToken, Compare, ContentMode, ContentSelect, Direction, Predicate, ProjectedContent,
    ScanInterrupted, ScanPage, ScanRequest, ScanRow, DEFAULT_MAX_RECONSTRUCTED_BYTES,
};
use turndb::store::{ReadStore, Store};
use turndb::types::AttrValue;

fn failure(context: &str, error: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, format!("{context}: {error:#}"))
}

fn coded_failure(code: &str, reason: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, format!("[TURNDB_CODE:{code}] {reason}"))
}

fn scan_failure(context: &str, error: anyhow::Error) -> Error {
    if error.chain().any(|cause| cause.downcast_ref::<ScanInterrupted>().is_some()) {
        coded_failure("CANCELLED", format!("{context}: {error:#}"))
    } else {
        failure(context, error)
    }
}

fn lifecycle_failure(context: &str, error: anyhow::Error) -> Error {
    if error.chain().any(|cause| cause.downcast_ref::<OperationInterrupted>().is_some()) {
        coded_failure("CANCELLED", format!("{context}: {error:#}"))
    } else {
        failure(context, error)
    }
}

fn backup_failure(context: &str, error: anyhow::Error) -> Error {
    let code = if let Some(error) = error.downcast_ref::<turndb::pack::BackupError>() {
        match error {
            turndb::pack::BackupError::DestinationExists(_) => "INVALID_ARGUMENT",
            turndb::pack::BackupError::InvalidBackup { .. } => "CORRUPTION",
            turndb::pack::BackupError::Unsupported(_) => "UNSUPPORTED",
        }
    } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
        match error.kind() {
            std::io::ErrorKind::NotFound => "NOT_FOUND",
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::InvalidInput => {
                "INVALID_ARGUMENT"
            }
            std::io::ErrorKind::Unsupported => "UNSUPPORTED",
            _ => "IO",
        }
    } else {
        "INTERNAL"
    };
    coded_failure(code, format!("{context}: {error:#}"))
}

fn recovery_failure(context: &str, error: anyhow::Error) -> Error {
    let code = if error
        .chain()
        .any(|cause| cause.downcast_ref::<turndb::fold::WriterLocked>().is_some())
    {
        "CONTENTION"
    } else if let Some(error) = error.downcast_ref::<turndb::store::RecoveryError>() {
        match error {
            turndb::store::RecoveryError::Healthy(_)
            | turndb::store::RecoveryError::RollbackLimit { .. } => "INVALID_ARGUMENT",
            turndb::store::RecoveryError::NoUsableCandidate { .. } => "CORRUPTION",
        }
    } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
        if error.kind() == std::io::ErrorKind::NotFound {
            "NOT_FOUND"
        } else {
            "IO"
        }
    } else {
        "INTERNAL"
    };
    coded_failure(code, format!("{context}: {error:#}"))
}

#[cfg(feature = "sql")]
fn sql_failure(context: &str, error: anyhow::Error) -> Error {
    let code = match classify_sql_error(&error) {
        SqlErrorClass::InvalidArgument => "INVALID_ARGUMENT",
        SqlErrorClass::ResourceExhausted => "RESOURCE_EXHAUSTED",
        SqlErrorClass::Unsupported => "UNSUPPORTED",
        SqlErrorClass::Io => "IO",
        SqlErrorClass::Internal => "INTERNAL",
    };
    coded_failure(code, format!("{context}: {error:#}"))
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

#[napi(object, object_to_js = false)]
pub struct NativeScanRequest {
    pub from: Option<String>,
    pub to: Option<String>,
    /// `forward` or `reverse`.
    pub direction: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub max_examined: Option<u32>,
    /// Whole-page content reconstruction ceiling. Rows are never truncated.
    pub max_reconstructed_bytes: Option<BigInt>,
    /// Milliseconds from submission; zero is an immediate, deterministic deadline.
    pub timeout_ms: Option<u32>,
    pub signal: Option<AbortSignal>,
    pub attrs: Option<Vec<String>>,
    pub contents: Option<Vec<NativeContentSelect>>,
    pub predicates: Option<Vec<NativePredicate>>,
}

#[napi(object, object_to_js = false)]
pub struct NativeLifecycleOptions {
    /// Milliseconds from submission; actor-queue time is included.
    pub timeout_ms: Option<u32>,
    pub signal: Option<AbortSignal>,
}

#[napi(object)]
pub struct NativeProjectedContent {
    pub name: String,
    pub present: bool,
    pub len: Option<BigInt>,
    pub pieces: Option<u32>,
    pub identity: Option<String>,
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
    pub reconstruction_budget_exhausted: bool,
}

#[napi(object)]
pub struct NativeScanPage {
    pub rows: Vec<NativeScanRow>,
    pub next: Option<String>,
    pub stats: NativeScanStats,
}

#[cfg(feature = "sql")]
#[napi(object)]
pub struct NativeSqlParam {
    /// `null`, `string`, `int`, `float`, `bool`, or `binary`.
    pub kind: String,
    pub string_value: Option<String>,
    pub int_value: Option<BigInt>,
    pub float_value: Option<f64>,
    pub bool_value: Option<bool>,
    pub binary_value: Option<Buffer>,
}

#[cfg(feature = "sql")]
#[napi(object, object_to_js = false)]
pub struct NativeSqlOptions {
    /// DataFusion execution memory. TurnDB caches and the returned IPC buffer are accounted apart.
    pub max_memory_bytes: Option<BigInt>,
}

#[cfg(feature = "sql")]
#[napi(object, object_to_js = false)]
pub struct NativeSqlNextOptions {
    pub timeout_ms: Option<u32>,
    pub signal: Option<AbortSignal>,
}

#[cfg(feature = "sql")]
#[napi(object)]
pub struct NativeSqlStats {
    pub rows: BigInt,
    pub batches: BigInt,
    pub columns_decoded: BigInt,
    pub fold_reads: BigInt,
    pub rows_filtered: BigInt,
    pub rows_hidden: BigInt,
    pub batches_skipped: BigInt,
    pub shadowed_occurrences: BigInt,
}

#[cfg(feature = "sql")]
#[napi(object)]
pub struct NativeSqlBatch {
    /// A complete, independently decodable Arrow IPC stream containing exactly one record batch.
    pub ipc: Buffer,
    pub rows: u32,
    pub stats: NativeSqlStats,
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
    pub command_queue_capacity_max: u32,
    pub immutable_snapshots: bool,
    pub lifecycle_operations: bool,
    pub backup_restore: bool,
    pub recovery_controls: bool,
    pub health_snapshots: bool,
    pub schema_discovery: bool,
    pub scan_cancellation: bool,
    pub lifecycle_cancellation: bool,
    pub scan_reconstruction_budget: bool,
    pub scan_reconstructed_bytes_default: BigInt,
    pub arrow_ipc: bool,
    pub parameterized_sql: bool,
    pub sql_memory_bytes_default: Option<BigInt>,
    pub sql_aggregate_memory_bytes_default: Option<BigInt>,
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
        command_queue_capacity: DEFAULT_QUEUE_CAPACITY as u32,
        command_queue_capacity_max: MAX_QUEUE_CAPACITY as u32,
        immutable_snapshots: true,
        lifecycle_operations: true,
        backup_restore: turndb::pack::ATOMIC_RESTORE,
        recovery_controls: true,
        health_snapshots: true,
        schema_discovery: true,
        scan_cancellation: true,
        lifecycle_cancellation: true,
        scan_reconstruction_budget: true,
        scan_reconstructed_bytes_default: BigInt::from(DEFAULT_MAX_RECONSTRUCTED_BYTES),
        arrow_ipc: cfg!(feature = "sql"),
        parameterized_sql: cfg!(feature = "sql"),
        sql_memory_bytes_default: cfg!(feature = "sql").then(|| {
            #[cfg(feature = "sql")]
            {
                BigInt::from(DEFAULT_MEMORY_BYTES as u64)
            }
            #[cfg(not(feature = "sql"))]
            unreachable!()
        }),
        sql_aggregate_memory_bytes_default: cfg!(feature = "sql").then(|| {
            #[cfg(feature = "sql")]
            {
                BigInt::from(DEFAULT_AGGREGATE_MEMORY_BYTES as u64)
            }
            #[cfg(not(feature = "sql"))]
            unreachable!()
        }),
    }
}

#[napi(object)]
pub struct NativeOpenOptions {
    /// Accepted commands waiting behind the one currently executing. Defaults to 64.
    pub command_queue_capacity: Option<u32>,
    /// Sum of the execution-memory ceilings reserved by live SQL queries. Defaults to 1 GiB.
    pub max_concurrent_sql_memory_bytes: Option<BigInt>,
}

#[napi(object)]
pub struct NativeSnapshotOpenOptions {
    pub max_concurrent_sql_memory_bytes: Option<BigInt>,
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
pub struct NativeBackupResult {
    pub files: BigInt,
    pub bytes: BigInt,
    pub commit: BigInt,
}

#[napi(object)]
pub struct NativeRecoveryOptions {
    pub max_rollback_commits: Option<BigInt>,
}

#[napi(object)]
pub struct NativeRecoveryResult {
    pub commit: BigInt,
    pub rollback_commits: BigInt,
    pub records: BigInt,
    pub content_values: BigInt,
    pub parts: BigInt,
    pub part_sections: BigInt,
    pub fold_segments: u32,
    pub fold_blocks: BigInt,
    pub fold_bytes: BigInt,
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
    #[cfg(feature = "sql")]
    sql_budget: SqlBudget,
}

#[cfg(feature = "sql")]
struct SqlSlot {
    query: Option<SqlQuery>,
    stats: turndb::query::ScanStats,
}

#[cfg(feature = "sql")]
struct SqlQueryState {
    slot: tokio::sync::Mutex<SqlSlot>,
    schema_ipc: Vec<u8>,
    pulling: AtomicBool,
}

/// A pull-based read-only SQL result. Each pull retains at most one Arrow batch in the binding.
#[cfg(feature = "sql")]
#[napi]
pub struct NativeSqlQuery {
    state: Arc<SqlQueryState>,
}

#[cfg(feature = "sql")]
impl NativeSqlQuery {
    fn new(query: SqlQuery) -> NativeSqlQuery {
        let schema_ipc = query.schema_ipc().to_vec();
        NativeSqlQuery {
            state: Arc::new(SqlQueryState {
                slot: tokio::sync::Mutex::new(SqlSlot {
                    query: Some(query),
                    stats: turndb::query::ScanStats::default(),
                }),
                schema_ipc,
                pulling: AtomicBool::new(false),
            }),
        }
    }
}

#[cfg(feature = "sql")]
enum SqlPull {
    Batch(anyhow::Result<Option<SqlBatch>>),
    Interrupted(&'static str),
}

#[cfg(feature = "sql")]
async fn wait_sql_interrupt(
    abort: Option<tokio::sync::oneshot::Receiver<()>>,
    timeout: Option<Duration>,
) -> &'static str {
    match (abort, timeout) {
        (Some(mut abort), Some(timeout)) => {
            tokio::select! {
                _ = &mut abort => "SQL query pull was cancelled",
                _ = tokio::time::sleep(timeout) => "SQL query pull deadline exceeded",
            }
        }
        (Some(abort), None) => {
            let _ = abort.await;
            "SQL query pull was cancelled"
        }
        (None, Some(timeout)) => {
            tokio::time::sleep(timeout).await;
            "SQL query pull deadline exceeded"
        }
        (None, None) => std::future::pending().await,
    }
}

#[cfg(feature = "sql")]
async fn pull_sql(
    state: Arc<SqlQueryState>,
    abort: Option<tokio::sync::oneshot::Receiver<()>>,
    timeout: Option<Duration>,
) -> Result<Option<NativeSqlBatch>> {
    let mut slot = state.slot.lock().await;
    if timeout.is_some_and(|timeout| timeout.is_zero()) {
        if let Some(query) = slot.query.take() {
            slot.stats = query.stats();
        }
        return Err(coded_failure("CANCELLED", "SQL query pull deadline exceeded"));
    }
    let Some(query) = slot.query.as_mut() else {
        return Ok(None);
    };
    let result = tokio::select! {
        result = query.next() => SqlPull::Batch(result),
        reason = wait_sql_interrupt(abort, timeout) => SqlPull::Interrupted(reason),
    };
    match result {
        SqlPull::Batch(Ok(Some(batch))) => {
            slot.stats =
                slot.query.as_ref().expect("query remains while a batch is returned").stats();
            Ok(Some(NativeSqlBatch {
                rows: u32::try_from(batch.rows)
                    .map_err(|_| coded_failure("INTERNAL", "SQL batch row count exceeds u32"))?,
                ipc: Buffer::from(batch.ipc),
                stats: encode_sql_stats(slot.stats),
            }))
        }
        SqlPull::Batch(Ok(None)) => {
            slot.stats = slot.query.as_ref().expect("query remains at end of stream").stats();
            slot.query = None;
            Ok(None)
        }
        SqlPull::Batch(Err(error)) => {
            slot.query = None;
            Err(sql_failure("pull TurnDB SQL query", error))
        }
        SqlPull::Interrupted(reason) => {
            slot.stats = slot.query.as_ref().expect("query remains until interruption").stats();
            slot.query = None;
            Err(coded_failure("CANCELLED", reason))
        }
    }
}

#[cfg(feature = "sql")]
#[napi]
impl NativeSqlQuery {
    /// A complete zero-batch Arrow IPC stream carrying the result schema.
    #[napi(getter)]
    pub fn schema_ipc(&self) -> Buffer {
        Buffer::from(self.state.schema_ipc.clone())
    }

    /// Pull one independently decodable Arrow IPC record batch. `null` remains stable at EOF.
    #[napi]
    pub fn next<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeSqlNextOptions>,
    ) -> Result<PromiseRaw<'env, Option<NativeSqlBatch>>> {
        if self
            .state
            .pulling
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_err()
        {
            return Err(coded_failure("BUSY", "a SQL query pull is already in progress"));
        }
        let (abort, timeout) = decode_sql_next(options);
        let state = self.state.clone();
        let pulling = self.state.clone();
        match env.spawn_future(async move {
            let result = pull_sql(state, abort, timeout).await;
            pulling.pulling.store(false, AtomicOrdering::Release);
            result
        }) {
            Ok(promise) => Ok(promise),
            Err(error) => {
                self.state.pulling.store(false, AtomicOrdering::Release);
                Err(error)
            }
        }
    }

    #[napi]
    pub async fn stats(&self) -> NativeSqlStats {
        encode_sql_stats(self.state.slot.lock().await.stats)
    }

    /// Drop the execution stream. DataFusion treats dropping an unfinished stream as cancellation.
    #[napi]
    pub async fn close(&self) {
        let mut slot = self.state.slot.lock().await;
        if let Some(query) = slot.query.take() {
            slot.stats = query.stats();
        }
    }
}

impl SnapshotState {
    #[cfg(feature = "sql")]
    fn new(store: ReadStore, sql_budget: SqlBudget) -> SnapshotState {
        let commit = store.manifest().commit;
        SnapshotState {
            store: Mutex::new(Some(Arc::new(store))),
            commit,
            #[cfg(feature = "sql")]
            sql_budget,
        }
    }

    #[cfg(not(feature = "sql"))]
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
    #[cfg(feature = "sql")]
    fn from_store(store: ReadStore, sql_budget: SqlBudget) -> NativeSnapshot {
        NativeSnapshot { state: Arc::new(SnapshotState::new(store, sql_budget)) }
    }

    #[cfg(not(feature = "sql"))]
    fn from_store(store: ReadStore) -> NativeSnapshot {
        NativeSnapshot { state: Arc::new(SnapshotState::new(store)) }
    }
}

#[napi]
impl NativeSnapshot {
    /// Open the currently published manifest without taking the writer lock or replaying its WAL.
    #[napi(factory)]
    pub async fn open(
        path: String,
        options: Option<NativeSnapshotOpenOptions>,
    ) -> Result<NativeSnapshot> {
        if path.is_empty() {
            return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
        }
        let store = napi::tokio::task::spawn_blocking(move || {
            Store::open_read(&PathBuf::from(path), FoldCfg::default())
        })
        .await
        .map_err(|error| failure("join TurnDB snapshot open", error))?
        .map_err(|error| failure("open TurnDB snapshot", error))?;
        #[cfg(feature = "sql")]
        {
            let budget = decode_sql_budget(
                options.and_then(|options| options.max_concurrent_sql_memory_bytes),
            )?;
            Ok(NativeSnapshot::from_store(store, budget))
        }
        #[cfg(not(feature = "sql"))]
        {
            let _ = options;
            Ok(NativeSnapshot::from_store(store))
        }
    }

    /// Open one retained manifest commit. Retention is bounded and erasure can purge history.
    #[napi(factory)]
    pub async fn open_at(
        path: String,
        commit: BigInt,
        options: Option<NativeSnapshotOpenOptions>,
    ) -> Result<NativeSnapshot> {
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
        #[cfg(feature = "sql")]
        {
            let budget = decode_sql_budget(
                options.and_then(|options| options.max_concurrent_sql_memory_bytes),
            )?;
            Ok(NativeSnapshot::from_store(store, budget))
        }
        #[cfg(not(feature = "sql"))]
        {
            let _ = options;
            Ok(NativeSnapshot::from_store(store))
        }
    }

    #[napi(getter)]
    pub fn commit(&self) -> BigInt {
        BigInt::from(self.state.commit)
    }

    #[napi]
    pub fn scan<'env>(
        &self,
        env: &'env Env,
        request: Option<NativeScanRequest>,
    ) -> Result<PromiseRaw<'env, NativeScanPage>> {
        let request = request.map(decode_scan).transpose();
        let store = self.state.get();
        env.spawn_future(async move {
            let request = request?.unwrap_or_default();
            let store = store?;
            napi::tokio::task::spawn_blocking(move || store.scan(&request))
                .await
                .map_err(|error| failure("join TurnDB snapshot scan", error))?
                .map(encode_page)
                .map_err(|error| scan_failure("scan TurnDB snapshot", error))
        })
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

/// Validate and restore one immutable backup into a destination that must not exist.
#[napi]
pub async fn restore_backup(
    backup_path: String,
    destination_path: String,
) -> Result<NativeBackupResult> {
    if backup_path.is_empty() || destination_path.is_empty() {
        return Err(Error::new(
            Status::InvalidArg,
            "backup and destination paths must not be empty",
        ));
    }
    napi::tokio::task::spawn_blocking(move || {
        turndb::pack::restore(&PathBuf::from(backup_path), &PathBuf::from(destination_path))
    })
    .await
    .map_err(|error| failure("join TurnDB backup restore", error))?
    .map(|stats| NativeBackupResult {
        files: BigInt::from(stats.files as u64),
        bytes: BigInt::from(stats.bytes),
        commit: BigInt::from(stats.commit),
    })
    .map_err(|error| backup_failure("restore TurnDB backup", error))
}

/// Exclusively validate and promote a retained manifest over a damaged live commit point.
#[napi]
pub async fn recover_manifest(
    path: String,
    options: Option<NativeRecoveryOptions>,
) -> Result<NativeRecoveryResult> {
    if path.is_empty() {
        return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
    }
    let max_rollback_commits = options
        .and_then(|options| options.max_rollback_commits)
        .map(|value| decode_u64(value, "maxRollbackCommits"))
        .transpose()?
        .unwrap_or(0);
    napi::tokio::task::spawn_blocking(move || {
        turndb::store::recover_manifest(
            &PathBuf::from(path),
            FoldCfg::default(),
            turndb::store::RecoveryOptions { max_rollback_commits },
        )
    })
    .await
    .map_err(|error| failure("join TurnDB manifest recovery", error))?
    .map(|report| NativeRecoveryResult {
        commit: BigInt::from(report.commit),
        rollback_commits: BigInt::from(report.rollback_commits),
        records: BigInt::from(report.records as u64),
        content_values: BigInt::from(report.content_values as u64),
        parts: BigInt::from(report.parts as u64),
        part_sections: BigInt::from(report.part_sections as u64),
        fold_segments: report.fold_segments,
        fold_blocks: BigInt::from(report.fold_blocks as u64),
        fold_bytes: BigInt::from(report.fold_bytes),
    })
    .map_err(|error| recovery_failure("recover TurnDB manifest", error))
}

/// A native writer handle. All operations are asynchronous and serialized by its Rust actor.
#[napi]
pub struct NativeStore {
    actor: Actor,
    #[cfg(feature = "sql")]
    sql_budget: SqlBudget,
}

#[napi]
impl NativeStore {
    /// Open a writer. Resolves only after recovery and writer-lock acquisition complete.
    #[napi(factory)]
    pub async fn open(path: String, options: Option<NativeOpenOptions>) -> Result<NativeStore> {
        if path.is_empty() {
            return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
        }
        let capacity = options
            .as_ref()
            .and_then(|options| options.command_queue_capacity)
            .unwrap_or(DEFAULT_QUEUE_CAPACITY as u32);
        if !(1..=MAX_QUEUE_CAPACITY as u32).contains(&capacity) {
            return Err(Error::new(
                Status::InvalidArg,
                format!(
                    "commandQueueCapacity must be between 1 and {MAX_QUEUE_CAPACITY}, got {capacity}"
                ),
            ));
        }
        #[cfg(feature = "sql")]
        let sql_budget = decode_sql_budget(
            options.as_ref().and_then(|options| options.max_concurrent_sql_memory_bytes.clone()),
        )?;
        let actor = napi::tokio::task::spawn_blocking(move || {
            Actor::open_with_capacity(&PathBuf::from(path), capacity as usize)
        })
        .await
        .map_err(|error| failure("join TurnDB open", error))?
        .map_err(|error| failure("open TurnDB store", error))?;
        #[cfg(feature = "sql")]
        {
            Ok(NativeStore { actor, sql_budget })
        }
        #[cfg(not(feature = "sql"))]
        Ok(NativeStore { actor })
    }

    /// The bounded backlog configured for this handle.
    #[napi(getter)]
    pub fn command_queue_capacity(&self) -> u32 {
        self.actor.queue_capacity() as u32
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
    pub fn scan<'env>(
        &self,
        env: &'env Env,
        request: Option<NativeScanRequest>,
    ) -> Result<PromiseRaw<'env, NativeScanPage>> {
        let request = request.map(decode_scan).transpose();
        let actor = self.actor.clone();
        env.spawn_future(async move {
            actor
                .scan(request?.unwrap_or_default())
                .await
                .map(encode_page)
                .map_err(|error| scan_failure("scan TurnDB store", error))
        })
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
        let store = self
            .actor
            .snapshot()
            .await
            .map_err(|error| failure("create TurnDB snapshot", error))?;
        #[cfg(feature = "sql")]
        return Ok(NativeSnapshot::from_store(store, self.sql_budget.clone()));
        #[cfg(not(feature = "sql"))]
        Ok(NativeSnapshot::from_store(store))
    }

    /// Settle earlier writes and compact. `full=true` merges every live part; false uses policy.
    #[napi]
    pub fn compact<'env>(
        &self,
        env: &'env Env,
        full: Option<bool>,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeCompactResult>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .compact(full.unwrap_or(false), control)
                .await
                .map(encode_compact)
                .map_err(|error| lifecycle_failure("compact TurnDB store", error))
        })
    }

    /// Settle earlier writes, then verify manifest pins, every part section, and every fold frame.
    #[napi]
    pub fn verify<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeVerifyResult>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .verify(control)
                .await
                .map(encode_verify)
                .map_err(|error| lifecycle_failure("verify TurnDB store", error))
        })
    }

    /// Settle earlier writes and publish a verified backup without replacing an existing path.
    #[napi]
    pub async fn backup(&self, path: String) -> Result<NativeBackupResult> {
        if path.is_empty() {
            return Err(Error::new(Status::InvalidArg, "backup path must not be empty"));
        }
        self.actor
            .backup(PathBuf::from(path))
            .await
            .map(|stats| NativeBackupResult {
                files: BigInt::from(stats.files as u64),
                bytes: BigInt::from(stats.bytes),
                commit: BigInt::from(stats.commit),
            })
            .map_err(|error| backup_failure("backup TurnDB store", error))
    }

    /// Physically erase ids from this store, including retained history. External copies are out of scope.
    #[napi]
    pub fn erase<'env>(
        &self,
        env: &'env Env,
        ids: Vec<String>,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeEraseResult>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .erase(ids, control)
                .await
                .map(|stats| NativeEraseResult {
                    requested: BigInt::from(stats.requested as u64),
                    tombstoned: BigInt::from(stats.tombstoned as u64),
                    absent: BigInt::from(stats.absent as u64),
                    refold: stats.refold.map(encode_refold),
                })
                .map_err(|error| lifecycle_failure("erase TurnDB records", error))
        })
    }

    /// Reclaim unreachable fold blocks in place where the platform supports punching.
    #[napi]
    pub fn punch<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativePunchResult>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .punch(control)
                .await
                .map(|stats| NativePunchResult {
                    blocks_examined: BigInt::from(stats.blocks_examined as u64),
                    blocks_punched: BigInt::from(stats.blocks_punched as u64),
                })
                .map_err(|error| lifecycle_failure("punch unreferenced TurnDB content", error))
        })
    }

    /// Rewrite all live content into a new fold generation and purge retained history.
    #[napi]
    pub fn refold<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeRefoldResult>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .refold(control)
                .await
                .map(encode_refold)
                .map_err(|error| lifecycle_failure("refold TurnDB store", error))
        })
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

#[cfg(feature = "sql")]
#[napi]
impl NativeSnapshot {
    #[napi(getter)]
    pub fn max_concurrent_sql_memory_bytes(&self) -> BigInt {
        BigInt::from(self.state.sql_budget.limit() as u64)
    }

    #[napi(getter)]
    pub fn reserved_sql_memory_bytes(&self) -> BigInt {
        BigInt::from(self.state.sql_budget.reserved() as u64)
    }

    /// Execute bounded, read-only SQL over this immutable snapshot and return a pull-based IPC stream.
    #[napi]
    pub async fn query_sql(
        &self,
        sql: String,
        params: Option<Vec<NativeSqlParam>>,
        options: Option<NativeSqlOptions>,
    ) -> Result<NativeSqlQuery> {
        let (params, options) = decode_sql(sql.as_str(), params, options)?;
        let store = self.state.get()?.as_ref().clone();
        SqlQuery::open_with_budget(store, &sql, params, options, &self.state.sql_budget)
            .await
            .map(NativeSqlQuery::new)
            .map_err(|error| sql_failure("open TurnDB snapshot SQL query", error))
    }
}

#[cfg(feature = "sql")]
#[napi]
impl NativeStore {
    #[napi(getter)]
    pub fn max_concurrent_sql_memory_bytes(&self) -> BigInt {
        BigInt::from(self.sql_budget.limit() as u64)
    }

    #[napi(getter)]
    pub fn reserved_sql_memory_bytes(&self) -> BigInt {
        BigInt::from(self.sql_budget.reserved() as u64)
    }

    /// Publish earlier writes as an exact immutable cut, then execute read-only SQL over that cut.
    #[napi]
    pub async fn query_sql(
        &self,
        sql: String,
        params: Option<Vec<NativeSqlParam>>,
        options: Option<NativeSqlOptions>,
    ) -> Result<NativeSqlQuery> {
        let (params, options) = decode_sql(sql.as_str(), params, options)?;
        let store = self
            .actor
            .snapshot()
            .await
            .map_err(|error| failure("publish TurnDB SQL snapshot", error))?;
        SqlQuery::open_with_budget(store, &sql, params, options, &self.sql_budget)
            .await
            .map(NativeSqlQuery::new)
            .map_err(|error| sql_failure("open TurnDB SQL query", error))
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

fn decode_lifecycle(options: Option<NativeLifecycleOptions>) -> OperationControl {
    let Some(options) = options else {
        return OperationControl::default();
    };
    let cancellation = options.signal.map(|signal| {
        let token = CancellationToken::new();
        let cancelled = token.clone();
        signal.on_abort(move || cancelled.cancel());
        token
    });
    let deadline =
        options.timeout_ms.map(|millis| Instant::now() + Duration::from_millis(u64::from(millis)));
    OperationControl { deadline, cancellation }
}

fn decode_scan(input: NativeScanRequest) -> Result<ScanRequest> {
    let max_reconstructed_bytes = input
        .max_reconstructed_bytes
        .map(|value| {
            let value = decode_u64(value, "maxReconstructedBytes")?;
            if value == 0 {
                return Err(Error::new(
                    Status::InvalidArg,
                    "maxReconstructedBytes must be greater than zero",
                ));
            }
            Ok(value)
        })
        .transpose()?;
    let cancellation = input.signal.map(|signal| {
        let token = CancellationToken::new();
        let cancelled = token.clone();
        signal.on_abort(move || cancelled.cancel());
        token
    });
    let deadline =
        input.timeout_ms.map(|millis| Instant::now() + Duration::from_millis(u64::from(millis)));
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
        deadline,
        cancellation,
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
    if let Some(max_reconstructed_bytes) = max_reconstructed_bytes {
        request.max_reconstructed_bytes = max_reconstructed_bytes;
    }
    Ok(request)
}

#[cfg(feature = "sql")]
fn decode_sql(
    sql: &str,
    params: Option<Vec<NativeSqlParam>>,
    options: Option<NativeSqlOptions>,
) -> Result<(Vec<SqlValue>, SqlOptions)> {
    if sql.trim().is_empty() {
        return Err(Error::new(Status::InvalidArg, "SQL query must not be empty"));
    }
    let params =
        params.unwrap_or_default().into_iter().map(decode_sql_param).collect::<Result<Vec<_>>>()?;
    let max_memory_bytes = options
        .and_then(|options| options.max_memory_bytes)
        .map(|value| decode_u64(value, "maxMemoryBytes"))
        .transpose()?
        .unwrap_or(DEFAULT_MEMORY_BYTES as u64);
    let max_memory_bytes = usize::try_from(max_memory_bytes).map_err(|_| {
        Error::new(Status::InvalidArg, "maxMemoryBytes exceeds this platform's address space")
    })?;
    if max_memory_bytes == 0 {
        return Err(Error::new(Status::InvalidArg, "maxMemoryBytes must be greater than zero"));
    }
    Ok((params, SqlOptions { max_memory_bytes }))
}

#[cfg(feature = "sql")]
fn decode_sql_param(param: NativeSqlParam) -> Result<SqlValue> {
    Ok(match param.kind.as_str() {
        "null" => SqlValue::Null,
        "string" => SqlValue::String(param.string_value.ok_or_else(|| {
            Error::new(Status::InvalidArg, "SQL string parameter needs stringValue")
        })?),
        "int" => {
            let value = param.int_value.ok_or_else(|| {
                Error::new(Status::InvalidArg, "SQL int parameter needs intValue")
            })?;
            let (value, lossless) = value.get_i64();
            if !lossless {
                return Err(Error::new(
                    Status::InvalidArg,
                    "SQL int parameter is outside the signed i64 range",
                ));
            }
            SqlValue::Int(value)
        }
        "float" => SqlValue::Float(param.float_value.ok_or_else(|| {
            Error::new(Status::InvalidArg, "SQL float parameter needs floatValue")
        })?),
        "bool" => SqlValue::Bool(param.bool_value.ok_or_else(|| {
            Error::new(Status::InvalidArg, "SQL bool parameter needs boolValue")
        })?),
        "binary" => SqlValue::Binary(
            param
                .binary_value
                .ok_or_else(|| {
                    Error::new(Status::InvalidArg, "SQL binary parameter needs binaryValue")
                })?
                .to_vec(),
        ),
        other => {
            return Err(Error::new(
                Status::InvalidArg,
                format!(
                    "unknown SQL parameter kind {other:?}; expected null, string, int, float, bool, or binary"
                ),
            ))
        }
    })
}

#[cfg(feature = "sql")]
fn decode_sql_next(
    options: Option<NativeSqlNextOptions>,
) -> (Option<tokio::sync::oneshot::Receiver<()>>, Option<Duration>) {
    let Some(options) = options else { return (None, None) };
    let abort = options.signal.map(|signal| {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(sender)));
        signal.on_abort(move || {
            if let Some(sender) = sender.lock().ok().and_then(|mut sender| sender.take()) {
                let _ = sender.send(());
            }
        });
        receiver
    });
    let timeout = options.timeout_ms.map(|millis| Duration::from_millis(u64::from(millis)));
    (abort, timeout)
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

#[cfg(feature = "sql")]
fn decode_sql_budget(value: Option<BigInt>) -> Result<SqlBudget> {
    let value = value
        .map(|value| decode_u64(value, "maxConcurrentSqlMemoryBytes"))
        .transpose()?
        .unwrap_or(DEFAULT_AGGREGATE_MEMORY_BYTES as u64);
    let value = usize::try_from(value).map_err(|_| {
        Error::new(
            Status::InvalidArg,
            "maxConcurrentSqlMemoryBytes exceeds this platform's address space",
        )
    })?;
    SqlBudget::new(value).map_err(|error| Error::new(Status::InvalidArg, error.to_string()))
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
        identity: content.identity.map(|identity| identity.to_hex()),
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
            reconstruction_budget_exhausted: page.stats.reconstruction_budget_exhausted,
        },
    }
}

#[cfg(feature = "sql")]
fn encode_sql_stats(stats: turndb::query::ScanStats) -> NativeSqlStats {
    NativeSqlStats {
        rows: BigInt::from(stats.rows as u64),
        batches: BigInt::from(stats.batches as u64),
        columns_decoded: BigInt::from(stats.columns_decoded as u64),
        fold_reads: BigInt::from(stats.fold_reads as u64),
        rows_filtered: BigInt::from(stats.rows_filtered as u64),
        rows_hidden: BigInt::from(stats.rows_hidden as u64),
        batches_skipped: BigInt::from(stats.batches_skipped as u64),
        shadowed_occurrences: BigInt::from(stats.shadowed_occurrences as u64),
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
