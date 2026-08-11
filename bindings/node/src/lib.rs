//! Native Node.js interface for TurnDB.
//!
//! The binding translates values and schedules work; the Rust core remains responsible for
//! atomicity, visibility, cursor validity, filtering, ordering, and content reconstruction.

#![deny(rustdoc::broken_intra_doc_links)]

mod actor;

use actor::{
    Actor, ActorFault, BoundedCompactResult, CompactResult, CompactionSpaceResult,
    FormatMigrationPreflightResult, FormatMigrationStepResult, OwnedContent, RefoldSpaceResult,
    VerifyResult, WriteOp, DEFAULT_QUEUE_CAPACITY, MAX_QUEUE_CAPACITY,
};
use napi::bindgen_prelude::{AbortSignal, BigInt, Buffer, PromiseRaw};
use napi::{Env, Error, Result, Status};
use napi_derive::napi;
use std::path::PathBuf;
#[cfg(feature = "sql")]
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use turndb::control::OperationControl;
use turndb::error::classify as classify_engine_error;
use turndb::fold::FoldCfg;
#[cfg(feature = "sql")]
use turndb::query::sql::{
    SqlBatch, SqlBudget, SqlOptions, SqlQuery, SqlValue, DEFAULT_AGGREGATE_MEMORY_BYTES,
    DEFAULT_MEMORY_BYTES,
};
use turndb::read_limits::ReadLimits;
use turndb::scan::{
    CancellationToken, Compare, ContentMode, ContentSelect, Direction, Predicate, ProjectedContent,
    ScanExplanation, ScanPage, ScanRequest, ScanRow, DEFAULT_MAX_RECONSTRUCTED_BYTES,
    DEFAULT_MAX_RESOLUTION_ENTRIES, MAX_RESOLUTION_ENTRIES,
};
use turndb::store::{CompactionBudget, ReadStore, Store, StoreOptions, WriteLimits};
use turndb::types::AttrValue;

fn failure(context: &str, error: impl std::fmt::Display) -> Error {
    coded_failure("INTERNAL", format!("{context}: {error:#}"))
}

fn coded_failure(code: &str, reason: impl std::fmt::Display) -> Error {
    Error::new(Status::GenericFailure, format!("[TURNDB_CODE:{code}] {reason}"))
}

fn engine_failure(context: &str, error: anyhow::Error) -> Error {
    let code = match error.chain().find_map(|cause| cause.downcast_ref::<ActorFault>()) {
        Some(ActorFault::Busy { .. }) => "BUSY",
        Some(ActorFault::Closed) => "CLOSED",
        Some(ActorFault::WorkerExited) => "INTERNAL",
        None => classify_engine_error(&error).code(),
    };
    coded_failure(code, format!("{context}: {error:#}"))
}

#[napi(object)]
pub struct NativeAttr {
    pub name: String,
    /// `string`, `int`, `float`, `bool`, `uint`, `binary`, `timestamp_ns`, or `null`.
    pub kind: String,
    pub string_value: Option<String>,
    pub int_value: Option<BigInt>,
    pub float_value: Option<f64>,
    pub bool_value: Option<bool>,
    pub uint_value: Option<BigInt>,
    pub binary_value: Option<Buffer>,
    pub timestamp_ns_value: Option<BigInt>,
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
    /// Immutable row occurrences plus memtable entries resolved before predicate evaluation.
    pub max_resolution_entries: Option<u32>,
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
pub struct NativeScanIoStats {
    pub part_sections_touched: BigInt,
    pub part_section_cache_hits: BigInt,
    pub part_section_cache_misses: BigInt,
    pub part_stored_bytes_read: BigInt,
    pub part_raw_bytes_decoded: BigInt,
    pub fold_blocks_touched: BigInt,
    pub fold_block_cache_hits: BigInt,
    pub fold_block_cache_misses: BigInt,
    pub fold_stored_bytes_read: BigInt,
    pub fold_raw_bytes_decoded: BigInt,
}

#[napi(object)]
pub struct NativeScanResolutionStats {
    pub physical_rows: BigInt,
    pub superseded_rows: BigInt,
    pub tombstones: BigInt,
    pub memtable_entries: BigInt,
    pub budget_exhausted: bool,
}

#[napi(object)]
pub struct NativeScanStats {
    pub duration_ns: BigInt,
    pub examined: u32,
    pub returned: u32,
    pub duplicate_attr_occurrences: u32,
    pub content_values_reconstructed: u32,
    pub reconstructed_bytes: BigInt,
    pub reconstruction_budget_exhausted: bool,
    pub io: NativeScanIoStats,
    pub resolution: NativeScanResolutionStats,
}

#[napi(object)]
pub struct NativeScanPage {
    pub rows: Vec<NativeScanRow>,
    pub next: Option<String>,
    pub stats: NativeScanStats,
}

#[napi(object)]
pub struct NativeScanContentPlan {
    pub name: String,
    pub mode: String,
}

#[napi(object)]
pub struct NativeScanPhysicalScope {
    pub immutable_parts_considered: BigInt,
    pub immutable_parts_with_rows: BigInt,
    pub immutable_rows_in_bounds: BigInt,
    pub memtable_entries_in_bounds: BigInt,
}

#[napi(object)]
pub struct NativeScanExplanation {
    pub direction: String,
    pub uses_cursor: bool,
    pub effective_from: Option<String>,
    pub effective_to: Option<String>,
    pub empty_range: bool,
    pub projected_attrs: Vec<String>,
    pub required_attrs: Vec<String>,
    pub predicate_only_attrs: Vec<String>,
    pub projected_contents: Vec<NativeScanContentPlan>,
    pub required_contents: Vec<String>,
    pub predicate_only_contents: Vec<String>,
    pub reconstructed_contents: Vec<String>,
    pub id_predicates: u32,
    pub attr_predicates: u32,
    pub content_predicates: u32,
    pub limit: u32,
    pub max_examined: u32,
    pub max_resolution_entries: u32,
    pub max_reconstructed_bytes: BigInt,
    pub physical: NativeScanPhysicalScope,
}

#[cfg(feature = "sql")]
#[napi(object)]
pub struct NativeSqlParam {
    /// `null`, `string`, `int`, `uint`, `float`, `bool`, `binary`, or `timestamp_ns`.
    pub kind: String,
    pub string_value: Option<String>,
    pub int_value: Option<BigInt>,
    pub float_value: Option<f64>,
    pub bool_value: Option<bool>,
    pub binary_value: Option<Buffer>,
    pub uint_value: Option<BigInt>,
    pub timestamp_ns_value: Option<BigInt>,
}

#[cfg(feature = "sql")]
#[napi(object, object_to_js = false)]
pub struct NativeSqlOptions {
    /// DataFusion execution memory. TurnDB caches and the returned IPC buffer are accounted apart.
    pub max_memory_bytes: Option<BigInt>,
    /// Milliseconds from submission, including actor queue time for writer-backed queries.
    pub timeout_ms: Option<u32>,
    pub signal: Option<AbortSignal>,
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
    pub planning_duration_ns: BigInt,
    pub execution_duration_ns: BigInt,
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
    pub positioned_io: bool,
    pub threads: bool,
    pub columnar: bool,
    pub sql: bool,
    pub portable_wasm: bool,
    pub native_node: bool,
    pub napi_version: u8,
    pub command_queue_capacity: u32,
    pub command_queue_capacity_max: u32,
    pub write_admission_limits: bool,
    pub read_admission_limits: bool,
    pub object_count_admission: bool,
    pub store_space_usage: bool,
    pub allocated_space_usage: bool,
    pub format_migration: bool,
    pub operation_metrics: bool,
    pub part_distribution: bool,
    pub content_liveness: bool,
    pub lifecycle_event_journal: bool,
    pub lifecycle_event_capacity: u32,
    pub query_timings: bool,
    pub sql_explain: bool,
    pub storage_runtime_options: bool,
    pub max_record_bytes_default: BigInt,
    pub max_batch_bytes_default: BigInt,
    pub max_batch_records_default: u32,
    pub max_identifier_bytes_default: u32,
    pub max_stored_frame_bytes_default: BigInt,
    pub max_decoded_frame_bytes_default: BigInt,
    pub max_directory_entries_default: BigInt,
    pub max_wal_frames_default: BigInt,
    pub max_fold_blocks_default: BigInt,
    pub immutable_snapshots: bool,
    pub lifecycle_operations: bool,
    pub backup_restore: bool,
    pub recovery_controls: bool,
    pub health_snapshots: bool,
    pub schema_discovery: bool,
    pub scan_explanation: bool,
    pub scan_cancellation: bool,
    pub lifecycle_cancellation: bool,
    pub bounded_compaction: bool,
    pub scan_reconstruction_budget: bool,
    pub scan_reconstructed_bytes_default: BigInt,
    pub scan_resolution_budget: bool,
    pub scan_resolution_entries_default: u32,
    pub scan_resolution_entries_max: u32,
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
        positioned_io: c.positioned_io,
        threads: c.threads,
        columnar: c.columnar,
        sql: c.sql,
        portable_wasm: c.portable_wasm,
        native_node: true,
        napi_version: 6,
        command_queue_capacity: DEFAULT_QUEUE_CAPACITY as u32,
        command_queue_capacity_max: MAX_QUEUE_CAPACITY as u32,
        write_admission_limits: c.write_admission_limits,
        read_admission_limits: c.read_admission_limits,
        object_count_admission: c.object_count_admission,
        store_space_usage: c.store_space_usage,
        allocated_space_usage: c.allocated_space_usage,
        format_migration: c.format_migration,
        operation_metrics: c.operation_metrics,
        part_distribution: c.part_distribution,
        content_liveness: c.content_liveness,
        lifecycle_event_journal: c.lifecycle_event_journal,
        lifecycle_event_capacity: turndb::observability::EVENT_JOURNAL_CAPACITY as u32,
        query_timings: c.query_timings,
        sql_explain: c.sql_explain,
        storage_runtime_options: true,
        max_record_bytes_default: BigInt::from(c.max_record_bytes_default),
        max_batch_bytes_default: BigInt::from(c.max_batch_bytes_default),
        max_batch_records_default: c.max_batch_records_default as u32,
        max_identifier_bytes_default: c.max_identifier_bytes_default as u32,
        max_stored_frame_bytes_default: BigInt::from(c.max_stored_frame_bytes_default),
        max_decoded_frame_bytes_default: BigInt::from(c.max_decoded_frame_bytes_default),
        max_directory_entries_default: BigInt::from(c.max_directory_entries_default),
        max_wal_frames_default: BigInt::from(c.max_wal_frames_default),
        max_fold_blocks_default: BigInt::from(c.max_fold_blocks_default),
        immutable_snapshots: true,
        lifecycle_operations: true,
        backup_restore: turndb::pack::ATOMIC_RESTORE,
        recovery_controls: true,
        health_snapshots: true,
        schema_discovery: true,
        scan_explanation: true,
        scan_cancellation: true,
        lifecycle_cancellation: true,
        bounded_compaction: true,
        scan_reconstruction_budget: true,
        scan_reconstructed_bytes_default: BigInt::from(DEFAULT_MAX_RECONSTRUCTED_BYTES),
        scan_resolution_budget: true,
        scan_resolution_entries_default: DEFAULT_MAX_RESOLUTION_ENTRIES as u32,
        scan_resolution_entries_max: MAX_RESOLUTION_ENTRIES as u32,
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
    /// Worst-case framed-WAL bytes admitted for one record. Defaults to 64 MiB.
    pub max_record_bytes: Option<BigInt>,
    /// Worst-case framed-WAL bytes admitted for one atomic batch. Defaults to 256 MiB.
    pub max_batch_bytes: Option<BigInt>,
    /// Ordered members admitted in one atomic batch. Defaults to 4,096.
    pub max_batch_records: Option<u32>,
    /// UTF-8 bytes admitted in an id, attribute name, or content name. Defaults to 4 KiB.
    pub max_identifier_bytes: Option<u32>,
    /// Stored bytes admitted for one WAL/TOC/section/fold frame. Defaults to 512 MiB.
    pub max_stored_frame_bytes: Option<BigInt>,
    /// Decoded bytes admitted for one TOC/section/fold frame. Defaults to 512 MiB.
    pub max_decoded_frame_bytes: Option<BigInt>,
    /// Entries admitted in one filesystem directory enumeration. Defaults to 100,000.
    pub max_directory_entries: Option<BigInt>,
    /// Physical frames admitted in one unflushed WAL. Defaults to 100,000.
    pub max_wal_frames: Option<BigInt>,
    /// Content blocks admitted in one fold generation. Defaults to 1,000,000.
    pub max_fold_blocks: Option<BigInt>,
    /// Raw bytes gathered per compressed fold block. Defaults to 4 MiB.
    pub block_target_bytes: Option<BigInt>,
    /// Decompressed fold-block cache budget. Defaults to 64 MiB.
    pub fold_cache_bytes: Option<BigInt>,
    /// Shared immutable-part section-cache budget. Defaults to 512 MiB.
    pub part_cache_bytes: Option<BigInt>,
    /// Fold segment roll threshold. Defaults to 1 GiB.
    pub segment_max_bytes: Option<BigInt>,
    /// Zstd write level in 1..=22. Defaults to 19.
    pub compression_level: Option<i32>,
    /// Fold compression workers; zero selects available parallelism.
    pub compression_threads: Option<u32>,
}

#[napi(object)]
pub struct NativeSnapshotOpenOptions {
    pub max_concurrent_sql_memory_bytes: Option<BigInt>,
    pub max_stored_frame_bytes: Option<BigInt>,
    pub max_decoded_frame_bytes: Option<BigInt>,
    pub max_directory_entries: Option<BigInt>,
    pub max_wal_frames: Option<BigInt>,
    pub max_fold_blocks: Option<BigInt>,
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

#[napi(object, object_to_js = false)]
pub struct NativeCompactionBudget {
    pub max_input_parts: u32,
    pub max_input_rows: BigInt,
    pub max_input_bytes: BigInt,
}

#[napi(object)]
pub struct NativeCompactionPlan {
    pub start_part: BigInt,
    pub input_parts: BigInt,
    pub input_rows: BigInt,
    pub input_bytes: BigInt,
    pub drops_tombstones: bool,
}

#[napi(object)]
pub struct NativeCompactionSpaceEstimate {
    pub plan: NativeCompactionPlan,
    pub input_sections: BigInt,
    pub input_raw_section_bytes: BigInt,
    pub estimated_stage_bytes: BigInt,
    pub estimate_is_hard_bound: bool,
    pub retained_input_bytes_after_commit: BigInt,
    pub filesystem_available_bytes: Option<BigInt>,
}

#[napi(object)]
pub struct NativeCompactionSpaceResult {
    pub flushed: bool,
    pub estimate: Option<NativeCompactionSpaceEstimate>,
}

#[napi(object)]
pub struct NativeRefoldSpaceEstimate {
    pub source_fold_logical_bytes: BigInt,
    pub source_part_bytes: BigInt,
    pub source_part_sections: BigInt,
    pub source_part_raw_section_bytes: BigInt,
    pub retained_only_bytes_before: BigInt,
    pub estimated_stage_bytes: BigInt,
    pub estimate_is_hard_bound: bool,
    pub filesystem_available_bytes: Option<BigInt>,
}

#[napi(object)]
pub struct NativeRefoldSpaceResult {
    pub flushed: bool,
    pub estimate: Option<NativeRefoldSpaceEstimate>,
}

#[napi(object)]
pub struct NativeFormatMigrationStatus {
    pub target_part_version: u8,
    pub live_parts: BigInt,
    pub current_parts: BigInt,
    pub legacy_parts: BigInt,
    pub legacy_rows: BigInt,
    pub legacy_bytes: BigInt,
    pub retained_legacy_parts: BigInt,
    pub retained_legacy_rows: BigInt,
    pub retained_legacy_bytes: BigInt,
}

#[napi(object)]
pub struct NativeFormatMigrationPlan {
    pub part_index: BigInt,
    pub source_part_version: u8,
    pub seq_lo: BigInt,
    pub seq_hi: BigInt,
    pub input_rows: BigInt,
    pub input_bytes: BigInt,
    pub input_sections: BigInt,
    pub input_raw_section_bytes: BigInt,
    pub estimated_stage_bytes: BigInt,
    pub estimate_is_hard_bound: bool,
    pub retained_input_bytes_after_commit: BigInt,
    pub filesystem_available_bytes: Option<BigInt>,
}

#[napi(object)]
pub struct NativeFormatMigrationPreflightResult {
    pub flushed: bool,
    pub status: NativeFormatMigrationStatus,
    pub estimate: Option<NativeFormatMigrationPlan>,
}

#[napi(object)]
pub struct NativeFormatMigrationStep {
    pub plan: NativeFormatMigrationPlan,
    pub output_bytes: BigInt,
    pub remaining_legacy_parts: BigInt,
    pub rewrite: NativeMergeStats,
}

#[napi(object)]
pub struct NativeFormatMigrationStepResult {
    pub flushed: bool,
    pub step: Option<NativeFormatMigrationStep>,
}

#[napi(object)]
pub struct NativeBoundedCompactResult {
    pub flushed: bool,
    pub parts_before: BigInt,
    pub parts_after: BigInt,
    pub plan: Option<NativeCompactionPlan>,
    pub output_bytes: Option<BigInt>,
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

#[napi(object, object_to_js = false)]
pub struct NativeRecoveryOptions {
    pub max_rollback_commits: Option<BigInt>,
    pub max_stored_frame_bytes: Option<BigInt>,
    pub max_decoded_frame_bytes: Option<BigInt>,
    pub max_directory_entries: Option<BigInt>,
    pub max_wal_frames: Option<BigInt>,
    pub max_fold_blocks: Option<BigInt>,
    /// Milliseconds from submission; worker-scheduling time is included.
    pub timeout_ms: Option<u32>,
    pub signal: Option<AbortSignal>,
}

#[napi(object, object_to_js = false)]
pub struct NativeRestoreOptions {
    pub max_stored_frame_bytes: Option<BigInt>,
    pub max_decoded_frame_bytes: Option<BigInt>,
    pub max_directory_entries: Option<BigInt>,
    pub max_wal_frames: Option<BigInt>,
    pub max_fold_blocks: Option<BigInt>,
    pub timeout_ms: Option<u32>,
    pub signal: Option<AbortSignal>,
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
    pub wal_frames: BigInt,
    pub fold_disk_bytes: BigInt,
    pub fold_segments: u32,
    pub fold_cache_hits: BigInt,
    pub fold_cache_misses: BigInt,
    pub fold_cache_bytes: BigInt,
    pub fold_cache_budget: BigInt,
    pub fold_block_target_bytes: BigInt,
    pub fold_segment_max_bytes: BigInt,
    pub fold_compression_level: i32,
    pub fold_compression_threads: BigInt,
    pub part_cache_bytes: BigInt,
    pub part_cache_budget: BigInt,
    pub max_stored_frame_bytes: BigInt,
    pub max_decoded_frame_bytes: BigInt,
    pub max_directory_entries: BigInt,
    pub max_wal_frames: BigInt,
    pub max_fold_blocks: BigInt,
    pub dedup_window_entries: BigInt,
    pub retained_commits: BigInt,
    pub punched_blocks: BigInt,
}

#[napi(object)]
pub struct NativeOperationMetrics {
    pub attempts: BigInt,
    pub succeeded: BigInt,
    pub failed: BigInt,
    pub cancelled: BigInt,
    pub total_duration_ns: BigInt,
    pub last_duration_ns: BigInt,
    pub max_duration_ns: BigInt,
}

#[napi(object)]
pub struct NativeStoreMetrics {
    pub open_recovery: NativeOperationMetrics,
    pub recovered_wal_frames: BigInt,
    pub sync: NativeOperationMetrics,
    pub flush: NativeOperationMetrics,
    pub compaction: NativeOperationMetrics,
    pub backup: NativeOperationMetrics,
    pub verification: NativeOperationMetrics,
    pub verification_corruption_failures: BigInt,
    pub punch: NativeOperationMetrics,
    pub refold: NativeOperationMetrics,
    pub format_migration: NativeOperationMetrics,
    pub folded_content: NativeFoldedContentMetrics,
}

#[napi(object)]
pub struct NativeFoldedContentMetrics {
    pub pieces: BigInt,
    pub dedup_hits: BigInt,
    pub logical_bytes: BigInt,
    pub novel_bytes: BigInt,
}

#[napi(object)]
pub struct NativePartDistribution {
    pub parts: BigInt,
    pub total_bytes: BigInt,
    pub min_bytes: BigInt,
    pub p50_bytes: BigInt,
    pub p95_bytes: BigInt,
    pub max_bytes: BigInt,
    pub total_rows: BigInt,
    pub min_rows: BigInt,
    pub p50_rows: BigInt,
    pub p95_rows: BigInt,
    pub max_rows: BigInt,
}

#[napi(object)]
pub struct NativeFoldBlockSpace {
    pub blocks: BigInt,
    pub raw_bytes: BigInt,
    pub stored_bytes: BigInt,
}

#[napi(object)]
pub struct NativeContentLiveness {
    pub live_pieces: BigInt,
    pub live_logical_bytes: BigInt,
    pub dead_logical_bytes: BigInt,
    pub stranded_dead_logical_bytes: BigInt,
    pub live_blocks: NativeFoldBlockSpace,
    pub reclaimable_blocks: NativeFoldBlockSpace,
}

#[napi(object)]
pub struct NativeLifecycleEvent {
    pub sequence: BigInt,
    pub operation: String,
    pub outcome: String,
    pub error_class: Option<String>,
    pub duration_ns: BigInt,
}

#[napi(object)]
pub struct NativeLifecycleEventBatch {
    pub events: Vec<NativeLifecycleEvent>,
    pub oldest_available_sequence: Option<BigInt>,
    pub latest_sequence: BigInt,
    pub dropped_events: BigInt,
    pub gap: bool,
}

#[napi(object)]
pub struct NativeSpaceAmount {
    pub files: BigInt,
    pub logical_bytes: BigInt,
    pub allocated_bytes: Option<BigInt>,
}

#[napi(object)]
pub struct NativeStoreSpaceUsage {
    pub live: NativeSpaceAmount,
    pub retained_only: NativeSpaceAmount,
    pub unclassified: NativeSpaceAmount,
    pub total: NativeSpaceAmount,
    pub filesystem_available_bytes: Option<BigInt>,
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
    deadline_at: Option<Instant>,
    cancelled: &'static str,
    deadline_reason: &'static str,
) -> &'static str {
    match (abort, deadline_at) {
        (Some(mut abort), Some(deadline_at)) => {
            tokio::select! {
                _ = &mut abort => cancelled,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline_at)) => deadline_reason,
            }
        }
        (Some(abort), None) => {
            let _ = abort.await;
            cancelled
        }
        (None, Some(at)) => {
            tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
            deadline_reason
        }
        (None, None) => std::future::pending().await,
    }
}

#[cfg(feature = "sql")]
async fn interruptible_sql_open<F>(
    work: F,
    abort: Option<tokio::sync::oneshot::Receiver<()>>,
    deadline: Option<Instant>,
) -> Result<NativeSqlQuery>
where
    F: std::future::Future<Output = Result<SqlQuery>>,
{
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(coded_failure("CANCELLED", "SQL query planning deadline exceeded"));
    }
    tokio::select! {
        query = work => query.map(NativeSqlQuery::new),
        reason = wait_sql_interrupt(
            abort,
            deadline,
            "SQL query planning was cancelled",
            "SQL query planning deadline exceeded",
        ) => Err(coded_failure("CANCELLED", reason)),
    }
}

#[cfg(feature = "sql")]
async fn pull_sql(
    state: Arc<SqlQueryState>,
    abort: Option<tokio::sync::oneshot::Receiver<()>>,
    deadline: Option<Instant>,
) -> Result<Option<NativeSqlBatch>> {
    let mut slot = state.slot.lock().await;
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
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
        reason = wait_sql_interrupt(
            abort,
            deadline,
            "SQL query pull was cancelled",
            "SQL query pull deadline exceeded",
        ) => SqlPull::Interrupted(reason),
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
            Err(engine_failure("pull TurnDB SQL query", error))
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
        let (abort, deadline) = decode_sql_next(options);
        let state = self.state.clone();
        let pulling = self.state.clone();
        match env.spawn_future(async move {
            let result = pull_sql(state, abort, deadline).await;
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
        let read_limits = decode_read_limits(
            options.as_ref().and_then(|value| value.max_stored_frame_bytes.clone()),
            options.as_ref().and_then(|value| value.max_decoded_frame_bytes.clone()),
            options.as_ref().and_then(|value| value.max_directory_entries.clone()),
            options.as_ref().and_then(|value| value.max_wal_frames.clone()),
            options.as_ref().and_then(|value| value.max_fold_blocks.clone()),
        )?;
        #[cfg(feature = "sql")]
        let budget = decode_sql_budget(
            options.as_ref().and_then(|value| value.max_concurrent_sql_memory_bytes.clone()),
        )?;
        let store = napi::tokio::task::spawn_blocking(move || {
            Store::open_read_with_limits(&PathBuf::from(path), FoldCfg::default(), read_limits)
        })
        .await
        .map_err(|error| failure("join TurnDB snapshot open", error))?
        .map_err(|error| engine_failure("open TurnDB snapshot", error))?;
        #[cfg(feature = "sql")]
        {
            Ok(NativeSnapshot::from_store(store, budget))
        }
        #[cfg(not(feature = "sql"))]
        {
            let _ = options;
            Ok(NativeSnapshot::from_store(store))
        }
    }

    /// Open a snapshot over a store held in ONE FILE — a sealed pack or a growable container.
    ///
    /// Which of the two it is comes from the file's magic, not its extension, and both answer
    /// reads identically: same manifest, same parts, same fold, same SQL. There is no writer role
    /// to take and no WAL to replay, so unlike a directory open this cannot contend with a writer.
    #[napi(factory)]
    pub async fn open_file(
        path: String,
        options: Option<NativeSnapshotOpenOptions>,
    ) -> Result<NativeSnapshot> {
        if path.is_empty() {
            return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
        }
        let read_limits = decode_read_limits(
            options.as_ref().and_then(|value| value.max_stored_frame_bytes.clone()),
            options.as_ref().and_then(|value| value.max_decoded_frame_bytes.clone()),
            options.as_ref().and_then(|value| value.max_directory_entries.clone()),
            options.as_ref().and_then(|value| value.max_wal_frames.clone()),
            options.as_ref().and_then(|value| value.max_fold_blocks.clone()),
        )?;
        #[cfg(feature = "sql")]
        let budget = decode_sql_budget(
            options.as_ref().and_then(|value| value.max_concurrent_sql_memory_bytes.clone()),
        )?;
        let store = napi::tokio::task::spawn_blocking(move || {
            turndb::store::open_read_file_with_limits(
                &PathBuf::from(path),
                FoldCfg::default(),
                read_limits,
            )
        })
        .await
        .map_err(|error| failure("join TurnDB single-file snapshot open", error))?
        .map_err(|error| engine_failure("open TurnDB single-file snapshot", error))?;
        #[cfg(feature = "sql")]
        {
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
        let read_limits = decode_read_limits(
            options.as_ref().and_then(|value| value.max_stored_frame_bytes.clone()),
            options.as_ref().and_then(|value| value.max_decoded_frame_bytes.clone()),
            options.as_ref().and_then(|value| value.max_directory_entries.clone()),
            options.as_ref().and_then(|value| value.max_wal_frames.clone()),
            options.as_ref().and_then(|value| value.max_fold_blocks.clone()),
        )?;
        #[cfg(feature = "sql")]
        let budget = decode_sql_budget(
            options.as_ref().and_then(|value| value.max_concurrent_sql_memory_bytes.clone()),
        )?;
        let store = napi::tokio::task::spawn_blocking(move || {
            Store::open_read_at_with_limits(
                &PathBuf::from(path),
                FoldCfg::default(),
                commit,
                read_limits,
            )
        })
        .await
        .map_err(|error| failure("join retained TurnDB snapshot open", error))?
        .map_err(|error| engine_failure("open retained TurnDB snapshot", error))?;
        #[cfg(feature = "sql")]
        {
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

    #[napi(getter)]
    pub fn max_stored_frame_bytes(&self) -> Result<BigInt> {
        Ok(BigInt::from(self.state.get()?.read_limits().max_stored_frame_bytes))
    }

    #[napi(getter)]
    pub fn max_decoded_frame_bytes(&self) -> Result<BigInt> {
        Ok(BigInt::from(self.state.get()?.read_limits().max_decoded_frame_bytes))
    }

    #[napi(getter)]
    pub fn max_directory_entries(&self) -> Result<BigInt> {
        Ok(BigInt::from(self.state.get()?.read_limits().max_directory_entries))
    }

    #[napi(getter)]
    pub fn max_wal_frames(&self) -> Result<BigInt> {
        Ok(BigInt::from(self.state.get()?.read_limits().max_wal_frames))
    }

    #[napi(getter)]
    pub fn max_fold_blocks(&self) -> Result<BigInt> {
        Ok(BigInt::from(self.state.get()?.read_limits().max_fold_blocks))
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
                .map_err(|error| engine_failure("scan TurnDB snapshot", error))
        })
    }

    /// Explain the prepared structured scan and exact pre-resolution physical scope.
    #[napi]
    pub fn explain_scan<'env>(
        &self,
        env: &'env Env,
        request: Option<NativeScanRequest>,
    ) -> Result<PromiseRaw<'env, NativeScanExplanation>> {
        let request = request.map(decode_scan).transpose();
        let store = self.state.get();
        env.spawn_future(async move {
            let request = request?.unwrap_or_default();
            let store = store?;
            napi::tokio::task::spawn_blocking(move || store.explain_scan(&request))
                .await
                .map_err(|error| failure("join TurnDB snapshot scan explanation", error))?
                .map(encode_scan_explanation)
                .map_err(|error| engine_failure("explain TurnDB snapshot scan", error))
        })
    }

    #[napi]
    pub async fn read_content(&self, id: String, name: String) -> Result<Option<Buffer>> {
        let store = self.state.get()?;
        napi::tokio::task::spawn_blocking(move || store.reconstruct_content(&id, &name))
            .await
            .map_err(|error| failure("join TurnDB snapshot content read", error))?
            .map(|bytes| bytes.map(Buffer::from))
            .map_err(|error| engine_failure("read TurnDB snapshot content", error))
    }

    /// Discover field names and scalar types from metadata without decoding values or content.
    #[napi]
    pub async fn schema(&self) -> Result<NativeSchema> {
        let store = self.state.get()?;
        napi::tokio::task::spawn_blocking(move || store.schema())
            .await
            .map_err(|error| failure("join TurnDB snapshot schema discovery", error))?
            .map(encode_schema)
            .map_err(|error| engine_failure("discover TurnDB snapshot schema", error))
    }

    #[napi]
    pub async fn close(&self) -> Result<()> {
        self.state.close()
    }
}

/// What one [`checkpoint_into_container`] moved.
#[napi(object)]
pub struct NativeCheckpointResult {
    /// Members the container holds after the checkpoint.
    pub members: u32,
    /// Bytes written into the container by this call.
    pub ingested_bytes: BigInt,
    /// Members already present byte-for-byte and therefore not rewritten.
    pub skipped_members: u32,
    /// The container's committed sequence after this call.
    pub commit_seq: BigInt,
    /// Bytes now superseded inside the container, reclaimable only by rewriting it.
    pub free_bytes: BigInt,
}

/// Checkpoint a store directory into a growable single file, creating it or growing one in place.
///
/// Incremental by construction: parts and rolled fold segments are immutable and uniquely named,
/// so a member already present under the same name and length is skipped rather than rewritten.
/// The source must be quiescent — settle it with `sync` and `flush` first — for the same reason a
/// backup refuses a store with a live WAL.
#[napi]
pub async fn checkpoint_into_container(
    directory_path: String,
    container_path: String,
) -> Result<NativeCheckpointResult> {
    if directory_path.is_empty() || container_path.is_empty() {
        return Err(Error::new(Status::InvalidArg, "store and container paths must not be empty"));
    }
    let stats = napi::tokio::task::spawn_blocking(move || {
        turndb::store::checkpoint_into_container(
            &PathBuf::from(directory_path),
            &PathBuf::from(container_path),
        )
    })
    .await
    .map_err(|error| failure("join TurnDB checkpoint", error))?
    .map_err(|error| engine_failure("checkpoint TurnDB store into a container", error))?;
    Ok(NativeCheckpointResult {
        members: stats.members as u32,
        ingested_bytes: BigInt::from(stats.ingested_bytes),
        skipped_members: stats.skipped_members as u32,
        commit_seq: BigInt::from(stats.commit_seq),
        free_bytes: BigInt::from(stats.free_bytes),
    })
}

/// Which single-file form a path holds: `"pack"` if sealed, `"container"` if it can still grow,
/// `null` for a directory or anything carrying neither magic.
///
/// Reading does not need this — [`NativeSnapshot::open_file`] dispatches on its own. It is here
/// for tooling that must know whether a file can be appended to before it plans to.
#[napi]
pub fn single_file_kind(path: String) -> Option<String> {
    match turndb::store::single_file_kind(&PathBuf::from(path)) {
        Some(turndb::store::SingleFileKind::Pack) => Some("pack".to_string()),
        Some(turndb::store::SingleFileKind::Container) => Some("container".to_string()),
        None => None,
    }
}

/// Retained commits currently available to [`NativeSnapshot::open_at`].
#[napi]
pub async fn retained_commits(path: String) -> Result<Vec<BigInt>> {
    if path.is_empty() {
        return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
    }
    let commits = napi::tokio::task::spawn_blocking(move || {
        turndb::store::retained_commits(&PathBuf::from(path))
    })
    .await
    .map_err(|error| failure("join retained TurnDB commit listing", error))?
    .map_err(|error| engine_failure("list retained TurnDB commits", error))?;
    Ok(commits.into_iter().map(BigInt::from).collect())
}

/// Validate and restore one immutable backup into a destination that must not exist.
#[napi]
pub fn restore_backup<'env>(
    env: &'env Env,
    backup_path: String,
    destination_path: String,
    options: Option<NativeRestoreOptions>,
) -> Result<PromiseRaw<'env, NativeBackupResult>> {
    if backup_path.is_empty() || destination_path.is_empty() {
        return Err(Error::new(
            Status::InvalidArg,
            "backup and destination paths must not be empty",
        ));
    }
    let (read_limits, control) = match options {
        Some(options) => (
            decode_read_limits(
                options.max_stored_frame_bytes,
                options.max_decoded_frame_bytes,
                options.max_directory_entries,
                options.max_wal_frames,
                options.max_fold_blocks,
            )?,
            decode_lifecycle(Some(NativeLifecycleOptions {
                timeout_ms: options.timeout_ms,
                signal: options.signal,
            })),
        ),
        None => (ReadLimits::default(), OperationControl::default()),
    };
    env.spawn_future(async move {
        napi::tokio::task::spawn_blocking(move || {
            turndb::pack::restore_with_limits_and_control(
                &PathBuf::from(backup_path),
                &PathBuf::from(destination_path),
                read_limits,
                &control,
            )
        })
        .await
        .map_err(|error| failure("join TurnDB backup restore", error))?
        .map(|stats| NativeBackupResult {
            files: BigInt::from(stats.files as u64),
            bytes: BigInt::from(stats.bytes),
            commit: BigInt::from(stats.commit),
        })
        .map_err(|error| engine_failure("restore TurnDB backup", error))
    })
}

/// Exclusively validate and promote a retained manifest over a damaged live commit point.
#[napi]
pub fn recover_manifest<'env>(
    env: &'env Env,
    path: String,
    options: Option<NativeRecoveryOptions>,
) -> Result<PromiseRaw<'env, NativeRecoveryResult>> {
    if path.is_empty() {
        return Err(Error::new(Status::InvalidArg, "store path must not be empty"));
    }
    let (max_rollback_commits, read_limits, control) = match options {
        Some(options) => (
            options
                .max_rollback_commits
                .map(|value| decode_u64(value, "maxRollbackCommits"))
                .transpose()?
                .unwrap_or(0),
            decode_read_limits(
                options.max_stored_frame_bytes,
                options.max_decoded_frame_bytes,
                options.max_directory_entries,
                options.max_wal_frames,
                options.max_fold_blocks,
            )?,
            decode_lifecycle(Some(NativeLifecycleOptions {
                timeout_ms: options.timeout_ms,
                signal: options.signal,
            })),
        ),
        None => (0, ReadLimits::default(), OperationControl::default()),
    };
    env.spawn_future(async move {
        napi::tokio::task::spawn_blocking(move || {
            turndb::store::recover_manifest_with_limits_and_control(
                &PathBuf::from(path),
                FoldCfg::default(),
                turndb::store::RecoveryOptions { max_rollback_commits },
                read_limits,
                &control,
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
        .map_err(|error| engine_failure("recover TurnDB manifest", error))
    })
}

/// A native writer handle. All operations are asynchronous and serialized by its Rust actor.
#[napi]
pub struct NativeStore {
    actor: Actor,
    /// Set when this handle was opened over a single file. The actor drives an ordinary store in
    /// the working directory beside it; this is what closing folds that work back into.
    container: Option<PathBuf>,
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
        let store_options = decode_store_options(options.as_ref())?;
        let actor = napi::tokio::task::spawn_blocking(move || {
            Actor::open_with_capacity_and_options(
                &PathBuf::from(path),
                capacity as usize,
                store_options,
            )
        })
        .await
        .map_err(|error| failure("join TurnDB open", error))?
        .map_err(|error| engine_failure("open TurnDB store", error))?;
        #[cfg(feature = "sql")]
        {
            Ok(NativeStore { actor, container: None, sql_budget })
        }
        #[cfg(not(feature = "sql"))]
        Ok(NativeStore { actor, container: None })
    }

    /// Open a writer over a store held in ONE FILE, creating the file if it does not exist.
    ///
    /// The engine's write path is directory-shaped — append semantics, fsync, and rename atomicity
    /// are properties a directory has and a byte range inside a file does not — so this drives an
    /// ordinary store in a working directory beside the file and folds it back in on
    /// [`NativeStore::close`]. After a clean close the file is the only artifact; after a crash the
    /// working directory remains and the next open resumes from it, because it holds writes the
    /// file was never told about.
    ///
    /// Every write method applies unchanged: it is the same engine either way.
    #[napi(factory)]
    pub async fn open_file(
        path: String,
        options: Option<NativeOpenOptions>,
    ) -> Result<NativeStore> {
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
        let store_options = decode_store_options(options.as_ref())?;
        let container = PathBuf::from(path);
        let opened = container.clone();
        let actor = napi::tokio::task::spawn_blocking(move || {
            // One implementation of the adopt-or-materialize decision, shared with the crate's
            // own ContainerStore. A second copy of it here is how the two get out of step.
            //
            // The whole `Prepared` goes to the actor, not just its path: sealed members stay in the
            // container, so the directory on its own is a store missing most of itself.
            let prepared = turndb::store::container_store::prepare(&opened)?;
            Actor::open_prepared(prepared, capacity as usize, store_options)
        })
        .await
        .map_err(|error| failure("join TurnDB single-file open", error))?
        .map_err(|error| engine_failure("open TurnDB single-file store", error))?;
        #[cfg(feature = "sql")]
        {
            Ok(NativeStore { actor, container: Some(container), sql_budget })
        }
        #[cfg(not(feature = "sql"))]
        Ok(NativeStore { actor, container: Some(container) })
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
            .map_err(|error| engine_failure("write TurnDB batch", error))
    }

    /// Make every previously accepted write crash-durable.
    #[napi]
    pub fn sync<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, ()>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor.sync(control).await.map_err(|error| engine_failure("sync TurnDB store", error))
        })
    }

    /// Seal the current memtable into an immutable part. Returns whether a part was written.
    #[napi]
    pub fn flush<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, bool>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor.flush(control).await.map_err(|error| engine_failure("flush TurnDB store", error))
        })
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
                .map_err(|error| engine_failure("scan TurnDB store", error))
        })
    }

    /// Explain the prepared structured scan and exact pre-resolution physical scope.
    #[napi]
    pub fn explain_scan<'env>(
        &self,
        env: &'env Env,
        request: Option<NativeScanRequest>,
    ) -> Result<PromiseRaw<'env, NativeScanExplanation>> {
        let request = request.map(decode_scan).transpose();
        let actor = self.actor.clone();
        env.spawn_future(async move {
            actor
                .explain_scan(request?.unwrap_or_default())
                .await
                .map(encode_scan_explanation)
                .map_err(|error| engine_failure("explain TurnDB store scan", error))
        })
    }

    /// Reconstruct one named content value without reading its siblings.
    #[napi]
    pub async fn read_content(&self, id: String, name: String) -> Result<Option<Buffer>> {
        self.actor
            .read_content(id, name)
            .await
            .map(|bytes| bytes.map(Buffer::from))
            .map_err(|error| engine_failure("read TurnDB content", error))
    }

    /// Publish all earlier accepted writes and return an immutable reader at that exact cut.
    #[napi]
    pub async fn snapshot(&self) -> Result<NativeSnapshot> {
        let store = self
            .actor
            .snapshot()
            .await
            .map_err(|error| engine_failure("create TurnDB snapshot", error))?;
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
                .map_err(|error| engine_failure("compact TurnDB store", error))
        })
    }

    /// Settle earlier writes, then publish one contiguous merge within exact physical-input bounds.
    #[napi]
    pub fn compact_bounded<'env>(
        &self,
        env: &'env Env,
        budget: NativeCompactionBudget,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeBoundedCompactResult>> {
        // Budget validation rejects through the promise like every other failure on this
        // Promise-typed surface — a `.then()` caller must not need a try/catch for one method.
        let budget = decode_compaction_budget(budget);
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .compact_bounded(budget?, control)
                .await
                .map(encode_bounded_compact)
                .map_err(|error| engine_failure("compact TurnDB store within budget", error))
        })
    }

    /// Settle earlier writes and estimate temporary space for the selected compaction plan.
    #[napi]
    pub fn estimate_compaction_space<'env>(
        &self,
        env: &'env Env,
        budget: NativeCompactionBudget,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeCompactionSpaceResult>> {
        let budget = decode_compaction_budget(budget);
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .estimate_compaction_space(budget?, control)
                .await
                .map(encode_compaction_space)
                .map_err(|error| engine_failure("estimate TurnDB compaction space", error))
        })
    }

    /// Inspect live immutable-part migration progress without changing store state.
    #[napi]
    pub fn format_migration_status<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeFormatMigrationStatus>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .format_migration_status(control)
                .await
                .map(encode_format_migration_status)
                .map_err(|error| engine_failure("read TurnDB format migration status", error))
        })
    }

    /// Settle earlier writes and preflight the next resumable format migration step.
    #[napi]
    pub fn estimate_format_migration_space<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeFormatMigrationPreflightResult>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .estimate_format_migration_space(control)
                .await
                .map(encode_format_migration_preflight)
                .map_err(|error| engine_failure("preflight TurnDB format migration", error))
        })
    }

    /// Settle earlier writes and atomically upgrade one legacy immutable part.
    #[napi]
    pub fn migrate_format_step<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeFormatMigrationStepResult>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .migrate_format_step(control)
                .await
                .map(encode_format_migration_step_result)
                .map_err(|error| engine_failure("migrate TurnDB format", error))
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
                .map_err(|error| engine_failure("verify TurnDB store", error))
        })
    }

    /// Settle earlier writes and publish a verified backup without replacing an existing path.
    #[napi]
    pub fn backup<'env>(
        &self,
        env: &'env Env,
        path: String,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeBackupResult>> {
        if path.is_empty() {
            return Err(Error::new(Status::InvalidArg, "backup path must not be empty"));
        }
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .backup(PathBuf::from(path), control)
                .await
                .map(|stats| NativeBackupResult {
                    files: BigInt::from(stats.files as u64),
                    bytes: BigInt::from(stats.bytes),
                    commit: BigInt::from(stats.commit),
                })
                .map_err(|error| engine_failure("backup TurnDB store", error))
        })
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
                .map_err(|error| engine_failure("erase TurnDB records", error))
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
                .map_err(|error| engine_failure("punch unreferenced TurnDB content", error))
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
                .map_err(|error| engine_failure("refold TurnDB store", error))
        })
    }

    /// Settle earlier writes and estimate duplicate-generation space for refold.
    #[napi]
    pub fn estimate_refold_space<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeRefoldSpaceResult>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .estimate_refold_space(control)
                .await
                .map(encode_refold_space)
                .map_err(|error| engine_failure("estimate TurnDB refold space", error))
        })
    }

    /// Return cheap operational counters without decoding records or content.
    #[napi]
    pub async fn health(&self) -> Result<NativeHealth> {
        self.actor
            .health()
            .await
            .map(encode_health)
            .map_err(|error| engine_failure("read TurnDB health", error))
    }

    /// Return monotonic operation outcomes and wall-time totals since this handle opened.
    #[napi]
    pub async fn metrics(&self) -> Result<NativeStoreMetrics> {
        self.actor
            .metrics()
            .await
            .map(encode_store_metrics)
            .map_err(|error| engine_failure("read TurnDB operation metrics", error))
    }

    /// Read bounded lifecycle outcomes after an independent sequence cursor.
    #[napi]
    pub async fn lifecycle_events(
        &self,
        after_sequence: Option<BigInt>,
        limit: Option<u32>,
    ) -> Result<NativeLifecycleEventBatch> {
        let after_sequence = after_sequence
            .map(|value| decode_u64(value, "afterSequence"))
            .transpose()?
            .unwrap_or(0);
        self.actor
            .lifecycle_events(after_sequence, limit.unwrap_or(100) as usize)
            .await
            .map(encode_lifecycle_events)
            .map_err(|error| engine_failure("read TurnDB lifecycle events", error))
    }

    /// Inspect exact live immutable-part file-size and physical-row distribution.
    #[napi]
    pub fn part_distribution<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativePartDistribution>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .part_distribution(control)
                .await
                .map(encode_part_distribution)
                .map_err(|error| engine_failure("measure TurnDB part distribution", error))
        })
    }

    /// Inspect exact live, dead, and block-reclaimable folded content.
    #[napi]
    pub fn content_liveness<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeContentLiveness>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .content_liveness(control)
                .await
                .map(encode_content_liveness)
                .map_err(|error| engine_failure("inspect TurnDB content liveness", error))
        })
    }

    /// Traverse and classify store files for maintenance-space preflight.
    #[napi]
    pub fn space_usage<'env>(
        &self,
        env: &'env Env,
        options: Option<NativeLifecycleOptions>,
    ) -> Result<PromiseRaw<'env, NativeStoreSpaceUsage>> {
        let actor = self.actor.clone();
        let control = decode_lifecycle(options);
        env.spawn_future(async move {
            actor
                .space_usage(control)
                .await
                .map(encode_space_usage)
                .map_err(|error| engine_failure("measure TurnDB store space", error))
        })
    }

    /// Discover the part field universe plus accepted writer-memtable fields.
    #[napi]
    pub async fn schema(&self) -> Result<NativeSchema> {
        self.actor
            .schema()
            .await
            .map(encode_schema)
            .map_err(|error| engine_failure("discover TurnDB schema", error))
    }

    /// Close the handle. Durability defaults to true; pass false only for an explicit no-sync close.
    #[napi]
    pub async fn close(&self, durable: Option<bool>) -> Result<()> {
        // A container close has to SEAL, not merely sync. `close(durable)` makes the WAL durable;
        // it does not empty it, and a checkpoint refuses a store whose WAL still holds writes no
        // part names — correctly, since those writes are not in anything the container would
        // ingest. Flushing first is what `ContainerStore::close` does for the same reason.
        if self.container.is_some() && durable.unwrap_or(true) {
            self.actor
                .flush(OperationControl::default())
                .await
                .map_err(|error| engine_failure("seal TurnDB writes before closing", error))?;
        }
        self.actor
            .close(durable.unwrap_or(true))
            .await
            .map_err(|error| engine_failure("close TurnDB store", error))?;
        // The actor is down and its writer lock released, so the working directory can be folded
        // in and removed. A non-durable close deliberately skips it: the caller asked not to
        // settle, and publishing unsettled state into the file would settle it anyway.
        if let (Some(container), true) = (self.container.clone(), durable.unwrap_or(true)) {
            napi::tokio::task::spawn_blocking(move || {
                turndb::store::container_store::settle(&container)
            })
            .await
            .map_err(|error| failure("join TurnDB single-file close", error))?
            .map_err(|error| {
                engine_failure("fold the working directory into its container", error)
            })?;
        }
        Ok(())
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
    pub fn query_sql<'env>(
        &self,
        env: &'env Env,
        sql: String,
        params: Option<Vec<NativeSqlParam>>,
        options: Option<NativeSqlOptions>,
    ) -> Result<PromiseRaw<'env, NativeSqlQuery>> {
        let decoded = decode_sql(sql.as_str(), params, options);
        let store = self.state.get().map(|store| store.as_ref().clone());
        let budget = self.state.sql_budget.clone();
        env.spawn_future(async move {
            let store = store?;
            let decoded = decoded?;
            let DecodedSql { params, options, abort, deadline } = decoded;
            interruptible_sql_open(
                async move {
                    SqlQuery::open_with_budget(store, &sql, params, options, &budget)
                        .await
                        .map_err(|error| engine_failure("open TurnDB snapshot SQL query", error))
                },
                abort,
                deadline,
            )
            .await
        })
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
    pub fn query_sql<'env>(
        &self,
        env: &'env Env,
        sql: String,
        params: Option<Vec<NativeSqlParam>>,
        options: Option<NativeSqlOptions>,
    ) -> Result<PromiseRaw<'env, NativeSqlQuery>> {
        let decoded = decode_sql(sql.as_str(), params, options);
        let actor = self.actor.clone();
        let budget = self.sql_budget.clone();
        env.spawn_future(async move {
            let decoded = decoded?;
            let DecodedSql { params, options, abort, deadline } = decoded;
            interruptible_sql_open(
                async move {
                    let store = actor
                        .snapshot()
                        .await
                        .map_err(|error| engine_failure("publish TurnDB SQL snapshot", error))?;
                    SqlQuery::open_with_budget(store, &sql, params, options, &budget)
                        .await
                        .map_err(|error| engine_failure("open TurnDB SQL query", error))
                },
                abort,
                deadline,
            )
            .await
        })
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
    let NativeAttr {
        name,
        kind,
        string_value,
        int_value,
        float_value,
        bool_value,
        uint_value,
        binary_value,
        timestamp_ns_value,
    } = attr;
    let supplied = u8::from(string_value.is_some())
        + u8::from(int_value.is_some())
        + u8::from(float_value.is_some())
        + u8::from(bool_value.is_some())
        + u8::from(uint_value.is_some())
        + u8::from(binary_value.is_some())
        + u8::from(timestamp_ns_value.is_some());
    if (kind == "null" && supplied != 0) || (kind != "null" && supplied != 1) {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "attribute {name:?} must carry exactly one typed value, except null carries none"
            ),
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
        "uint" => AttrValue::UInt(decode_u64(
            uint_value.ok_or_else(|| missing("uintValue"))?,
            "uintValue",
        )?),
        "binary" => AttrValue::Bytes(binary_value.ok_or_else(|| missing("binaryValue"))?.to_vec()),
        "timestamp_ns" => {
            let value = timestamp_ns_value.ok_or_else(|| missing("timestampNsValue"))?;
            let (value, lossless) = value.get_i64();
            if !lossless {
                return Err(Error::new(
                    Status::InvalidArg,
                    "timestampNsValue is outside the signed i64 range",
                ));
            }
            AttrValue::TimestampNs(value)
        }
        "null" => AttrValue::Null,
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
    if let Some(max_resolution_entries) = input.max_resolution_entries {
        if max_resolution_entries == 0 || max_resolution_entries as usize > MAX_RESOLUTION_ENTRIES {
            return Err(Error::new(
                Status::InvalidArg,
                format!("maxResolutionEntries must be in 1..={MAX_RESOLUTION_ENTRIES}"),
            ));
        }
        request.max_resolution_entries = max_resolution_entries as usize;
    }
    if let Some(max_reconstructed_bytes) = max_reconstructed_bytes {
        request.max_reconstructed_bytes = max_reconstructed_bytes;
    }
    Ok(request)
}

#[cfg(feature = "sql")]
struct DecodedSql {
    params: Vec<SqlValue>,
    options: SqlOptions,
    abort: Option<tokio::sync::oneshot::Receiver<()>>,
    deadline: Option<Instant>,
}

#[cfg(feature = "sql")]
fn decode_sql(
    sql: &str,
    params: Option<Vec<NativeSqlParam>>,
    options: Option<NativeSqlOptions>,
) -> Result<DecodedSql> {
    if sql.trim().is_empty() {
        return Err(Error::new(Status::InvalidArg, "SQL query must not be empty"));
    }
    let params =
        params.unwrap_or_default().into_iter().map(decode_sql_param).collect::<Result<Vec<_>>>()?;
    let (max_memory_bytes, timeout_ms, signal) = match options {
        Some(options) => (options.max_memory_bytes, options.timeout_ms, options.signal),
        None => (None, None, None),
    };
    let max_memory_bytes = max_memory_bytes
        .map(|value| decode_u64(value, "maxMemoryBytes"))
        .transpose()?
        .unwrap_or(DEFAULT_MEMORY_BYTES as u64);
    let max_memory_bytes = usize::try_from(max_memory_bytes).map_err(|_| {
        Error::new(Status::InvalidArg, "maxMemoryBytes exceeds this platform's address space")
    })?;
    if max_memory_bytes == 0 {
        return Err(Error::new(Status::InvalidArg, "maxMemoryBytes must be greater than zero"));
    }
    let (abort, deadline) = decode_sql_interrupt(timeout_ms, signal);
    Ok(DecodedSql { params, options: SqlOptions { max_memory_bytes }, abort, deadline })
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
        "uint" => SqlValue::UInt(decode_u64(
            param
                .uint_value
                .ok_or_else(|| Error::new(Status::InvalidArg, "SQL uint needs uintValue"))?,
            "SQL uintValue",
        )?),
        "timestamp_ns" => {
            let value = param.timestamp_ns_value.ok_or_else(|| {
                Error::new(Status::InvalidArg, "SQL timestamp_ns needs timestampNsValue")
            })?;
            let (value, lossless) = value.get_i64();
            if !lossless {
                return Err(Error::new(
                    Status::InvalidArg,
                    "SQL timestampNsValue is outside the signed i64 range",
                ));
            }
            SqlValue::TimestampNs(value)
        }
        other => {
            return Err(Error::new(
                Status::InvalidArg,
                format!(
                    "unknown SQL parameter kind {other:?}; expected null, string, int, uint, float, bool, binary, or timestamp_ns"
                ),
            ))
        }
    })
}

#[cfg(feature = "sql")]
fn decode_sql_next(
    options: Option<NativeSqlNextOptions>,
) -> (Option<tokio::sync::oneshot::Receiver<()>>, Option<Instant>) {
    let Some(options) = options else { return (None, None) };
    decode_sql_interrupt(options.timeout_ms, options.signal)
}

#[cfg(feature = "sql")]
fn decode_sql_interrupt(
    timeout_ms: Option<u32>,
    signal: Option<AbortSignal>,
) -> (Option<tokio::sync::oneshot::Receiver<()>>, Option<Instant>) {
    let abort = signal.map(|signal| {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(sender)));
        signal.on_abort(move || {
            if let Some(sender) = sender.lock().ok().and_then(|mut sender| sender.take()) {
                let _ = sender.send(());
            }
        });
        receiver
    });
    let deadline =
        timeout_ms.map(|millis| Instant::now() + Duration::from_millis(u64::from(millis)));
    (abort, deadline)
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

fn decode_write_limits(options: Option<&NativeOpenOptions>) -> Result<WriteLimits> {
    let defaults = WriteLimits::default();
    let limits = WriteLimits {
        max_record_bytes: match options.and_then(|options| options.max_record_bytes.clone()) {
            Some(value) => decode_u64(value, "maxRecordBytes")?,
            None => defaults.max_record_bytes,
        },
        max_batch_bytes: match options.and_then(|options| options.max_batch_bytes.clone()) {
            Some(value) => decode_u64(value, "maxBatchBytes")?,
            None => defaults.max_batch_bytes,
        },
        max_batch_records: options
            .and_then(|options| options.max_batch_records)
            .map(|value| value as usize)
            .unwrap_or(defaults.max_batch_records),
        max_identifier_bytes: options
            .and_then(|options| options.max_identifier_bytes)
            .map(|value| value as usize)
            .unwrap_or(defaults.max_identifier_bytes),
    };
    limits.validate().map_err(|error| Error::new(Status::InvalidArg, error.to_string()))
}

fn decode_read_limits(
    max_stored_frame_bytes: Option<BigInt>,
    max_decoded_frame_bytes: Option<BigInt>,
    max_directory_entries: Option<BigInt>,
    max_wal_frames: Option<BigInt>,
    max_fold_blocks: Option<BigInt>,
) -> Result<ReadLimits> {
    let defaults = ReadLimits::default();
    let limits = ReadLimits {
        max_stored_frame_bytes: match max_stored_frame_bytes {
            Some(value) => decode_u64(value, "maxStoredFrameBytes")?,
            None => defaults.max_stored_frame_bytes,
        },
        max_decoded_frame_bytes: match max_decoded_frame_bytes {
            Some(value) => decode_u64(value, "maxDecodedFrameBytes")?,
            None => defaults.max_decoded_frame_bytes,
        },
        max_directory_entries: match max_directory_entries {
            Some(value) => decode_u64(value, "maxDirectoryEntries")?,
            None => defaults.max_directory_entries,
        },
        max_wal_frames: match max_wal_frames {
            Some(value) => decode_u64(value, "maxWalFrames")?,
            None => defaults.max_wal_frames,
        },
        max_fold_blocks: match max_fold_blocks {
            Some(value) => decode_u64(value, "maxFoldBlocks")?,
            None => defaults.max_fold_blocks,
        },
    };
    limits.validate().map_err(|error| Error::new(Status::InvalidArg, error.to_string()))
}

fn decode_store_options(options: Option<&NativeOpenOptions>) -> Result<StoreOptions> {
    let defaults = StoreOptions::default();
    let decode_usize = |value: BigInt, name: &str| -> Result<usize> {
        usize::try_from(decode_u64(value, name)?).map_err(|_| {
            Error::new(Status::InvalidArg, format!("{name} exceeds this platform's address space"))
        })
    };
    let block_target = match options.and_then(|options| options.block_target_bytes.clone()) {
        Some(value) => decode_usize(value, "blockTargetBytes")?,
        None => defaults.fold.block_target,
    };
    if block_target == 0 || block_target as u64 > turndb::fold::BLOCK_TARGET_MAX {
        return Err(Error::new(
            Status::InvalidArg,
            format!("blockTargetBytes must be between 1 and {}", turndb::fold::BLOCK_TARGET_MAX),
        ));
    }
    let fold_cache_bytes = match options.and_then(|options| options.fold_cache_bytes.clone()) {
        Some(value) => decode_usize(value, "foldCacheBytes")?,
        None => defaults.fold.cache_bytes,
    };
    if fold_cache_bytes == 0 {
        return Err(Error::new(Status::InvalidArg, "foldCacheBytes must be greater than zero"));
    }
    let part_cache_bytes = match options.and_then(|options| options.part_cache_bytes.clone()) {
        Some(value) => decode_usize(value, "partCacheBytes")?,
        None => defaults.part_cache_bytes,
    };
    if part_cache_bytes < turndb::part::cache::BUDGET_MIN {
        return Err(Error::new(
            Status::InvalidArg,
            format!("partCacheBytes must be at least {}", turndb::part::cache::BUDGET_MIN),
        ));
    }
    let segment_max = match options.and_then(|options| options.segment_max_bytes.clone()) {
        Some(value) => decode_u64(value, "segmentMaxBytes")?,
        None => u64::from(defaults.fold.seg_max),
    };
    if segment_max == 0 || segment_max >= turndb::fold::SEGMENT_MAX_LIMIT {
        return Err(Error::new(
            Status::InvalidArg,
            format!(
                "segmentMaxBytes must be between 1 and {}",
                turndb::fold::SEGMENT_MAX_LIMIT - 1
            ),
        ));
    }
    let segment_max = u32::try_from(segment_max).map_err(|_| {
        Error::new(Status::InvalidArg, "segmentMaxBytes must be smaller than 4 GiB")
    })?;
    let level =
        options.and_then(|options| options.compression_level).unwrap_or(defaults.fold.level);
    if !(1..=22).contains(&level) {
        return Err(Error::new(Status::InvalidArg, "compressionLevel must be between 1 and 22"));
    }
    let compress_threads = options
        .and_then(|options| options.compression_threads)
        .map(|value| value as usize)
        .unwrap_or(defaults.fold.compress_threads);
    Ok(StoreOptions {
        fold: FoldCfg {
            seg_max: segment_max,
            cache_bytes: fold_cache_bytes,
            block_target,
            level,
            compress_threads,
        },
        write_limits: decode_write_limits(options)?,
        read_limits: decode_read_limits(
            options.and_then(|value| value.max_stored_frame_bytes.clone()),
            options.and_then(|value| value.max_decoded_frame_bytes.clone()),
            options.and_then(|value| value.max_directory_entries.clone()),
            options.and_then(|value| value.max_wal_frames.clone()),
            options.and_then(|value| value.max_fold_blocks.clone()),
        )?,
        part_cache_bytes,
    })
}

fn decode_compaction_budget(input: NativeCompactionBudget) -> Result<CompactionBudget> {
    let budget = CompactionBudget {
        max_input_parts: usize::try_from(input.max_input_parts).map_err(|_| {
            Error::new(Status::InvalidArg, "maxInputParts exceeds this platform's address space")
        })?,
        max_input_rows: decode_u64(input.max_input_rows, "maxInputRows")?,
        max_input_bytes: decode_u64(input.max_input_bytes, "maxInputBytes")?,
    };
    budget.validate().map_err(|error| Error::new(Status::InvalidArg, error.to_string()))?;
    Ok(budget)
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
        uint_value: None,
        binary_value: None,
        timestamp_ns_value: None,
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
        AttrValue::UInt(value) => {
            attr.kind = "uint".into();
            attr.uint_value = Some(BigInt::from(value));
        }
        AttrValue::Bytes(value) => {
            attr.kind = "binary".into();
            attr.binary_value = Some(Buffer::from(value));
        }
        AttrValue::TimestampNs(value) => {
            attr.kind = "timestamp_ns".into();
            attr.timestamp_ns_value = Some(BigInt::from(value));
        }
        AttrValue::Null => attr.kind = "null".into(),
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
            duration_ns: BigInt::from(page.stats.duration_ns),
            examined: page.stats.examined as u32,
            returned: page.stats.returned as u32,
            duplicate_attr_occurrences: page.stats.duplicate_attr_occurrences as u32,
            content_values_reconstructed: page.stats.content_values_reconstructed as u32,
            reconstructed_bytes: BigInt::from(page.stats.reconstructed_bytes),
            reconstruction_budget_exhausted: page.stats.reconstruction_budget_exhausted,
            io: NativeScanIoStats {
                part_sections_touched: BigInt::from(page.stats.io.part_sections_touched as u64),
                part_section_cache_hits: BigInt::from(page.stats.io.part_section_cache_hits),
                part_section_cache_misses: BigInt::from(page.stats.io.part_section_cache_misses),
                part_stored_bytes_read: BigInt::from(page.stats.io.part_stored_bytes_read),
                part_raw_bytes_decoded: BigInt::from(page.stats.io.part_raw_bytes_decoded),
                fold_blocks_touched: BigInt::from(page.stats.io.fold_blocks_touched as u64),
                fold_block_cache_hits: BigInt::from(page.stats.io.fold_block_cache_hits),
                fold_block_cache_misses: BigInt::from(page.stats.io.fold_block_cache_misses),
                fold_stored_bytes_read: BigInt::from(page.stats.io.fold_stored_bytes_read),
                fold_raw_bytes_decoded: BigInt::from(page.stats.io.fold_raw_bytes_decoded),
            },
            resolution: NativeScanResolutionStats {
                physical_rows: BigInt::from(page.stats.resolution.physical_rows as u64),
                superseded_rows: BigInt::from(page.stats.resolution.superseded_rows as u64),
                tombstones: BigInt::from(page.stats.resolution.tombstones as u64),
                memtable_entries: BigInt::from(page.stats.resolution.memtable_entries as u64),
                budget_exhausted: page.stats.resolution.budget_exhausted,
            },
        },
    }
}

fn encode_scan_explanation(explanation: ScanExplanation) -> NativeScanExplanation {
    NativeScanExplanation {
        direction: match explanation.direction {
            Direction::Forward => "forward",
            Direction::Reverse => "reverse",
        }
        .into(),
        uses_cursor: explanation.uses_cursor,
        effective_from: explanation.effective_from,
        effective_to: explanation.effective_to,
        empty_range: explanation.empty_range,
        projected_attrs: explanation.projected_attrs,
        required_attrs: explanation.required_attrs,
        predicate_only_attrs: explanation.predicate_only_attrs,
        projected_contents: explanation
            .projected_contents
            .into_iter()
            .map(|content| NativeScanContentPlan {
                name: content.name,
                mode: match content.mode {
                    ContentMode::Metadata => "metadata",
                    ContentMode::Bytes => "bytes",
                }
                .into(),
            })
            .collect(),
        required_contents: explanation.required_contents,
        predicate_only_contents: explanation.predicate_only_contents,
        reconstructed_contents: explanation.reconstructed_contents,
        id_predicates: explanation.id_predicates as u32,
        attr_predicates: explanation.attr_predicates as u32,
        content_predicates: explanation.content_predicates as u32,
        limit: explanation.limit as u32,
        max_examined: explanation.max_examined as u32,
        max_resolution_entries: explanation.max_resolution_entries as u32,
        max_reconstructed_bytes: BigInt::from(explanation.max_reconstructed_bytes),
        physical: NativeScanPhysicalScope {
            immutable_parts_considered: BigInt::from(
                explanation.physical.immutable_parts_considered as u64,
            ),
            immutable_parts_with_rows: BigInt::from(
                explanation.physical.immutable_parts_with_rows as u64,
            ),
            immutable_rows_in_bounds: BigInt::from(
                explanation.physical.immutable_rows_in_bounds as u64,
            ),
            memtable_entries_in_bounds: BigInt::from(
                explanation.physical.memtable_entries_in_bounds as u64,
            ),
        },
    }
}

#[cfg(feature = "sql")]
fn encode_sql_stats(stats: turndb::query::ScanStats) -> NativeSqlStats {
    NativeSqlStats {
        planning_duration_ns: BigInt::from(stats.planning_duration_ns),
        execution_duration_ns: BigInt::from(stats.execution_duration_ns),
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

fn encode_compaction_plan(plan: turndb::store::CompactionPlan) -> NativeCompactionPlan {
    NativeCompactionPlan {
        start_part: BigInt::from(plan.start_part as u64),
        input_parts: BigInt::from(plan.input_parts as u64),
        input_rows: BigInt::from(plan.input_rows),
        input_bytes: BigInt::from(plan.input_bytes),
        drops_tombstones: plan.drops_tombstones,
    }
}

fn encode_compaction_space(result: CompactionSpaceResult) -> NativeCompactionSpaceResult {
    NativeCompactionSpaceResult {
        flushed: result.flushed,
        estimate: result.estimate.map(|estimate| NativeCompactionSpaceEstimate {
            plan: encode_compaction_plan(estimate.plan),
            input_sections: BigInt::from(estimate.input_sections as u64),
            input_raw_section_bytes: BigInt::from(estimate.input_raw_section_bytes),
            estimated_stage_bytes: BigInt::from(estimate.estimated_stage_bytes),
            estimate_is_hard_bound: estimate.estimate_is_hard_bound,
            retained_input_bytes_after_commit: BigInt::from(
                estimate.retained_input_bytes_after_commit,
            ),
            filesystem_available_bytes: estimate.filesystem_available_bytes.map(BigInt::from),
        }),
    }
}

fn encode_format_migration_status(
    status: turndb::store::FormatMigrationStatus,
) -> NativeFormatMigrationStatus {
    NativeFormatMigrationStatus {
        target_part_version: status.target_part_version,
        live_parts: BigInt::from(status.live_parts as u64),
        current_parts: BigInt::from(status.current_parts as u64),
        legacy_parts: BigInt::from(status.legacy_parts as u64),
        legacy_rows: BigInt::from(status.legacy_rows),
        legacy_bytes: BigInt::from(status.legacy_bytes),
        retained_legacy_parts: BigInt::from(status.retained_legacy_parts as u64),
        retained_legacy_rows: BigInt::from(status.retained_legacy_rows),
        retained_legacy_bytes: BigInt::from(status.retained_legacy_bytes),
    }
}

fn encode_format_migration_plan(
    plan: turndb::store::FormatMigrationPlan,
) -> NativeFormatMigrationPlan {
    NativeFormatMigrationPlan {
        part_index: BigInt::from(plan.part_index as u64),
        source_part_version: plan.source_part_version,
        seq_lo: BigInt::from(plan.seq_lo),
        seq_hi: BigInt::from(plan.seq_hi),
        input_rows: BigInt::from(plan.input_rows),
        input_bytes: BigInt::from(plan.input_bytes),
        input_sections: BigInt::from(plan.input_sections as u64),
        input_raw_section_bytes: BigInt::from(plan.input_raw_section_bytes),
        estimated_stage_bytes: BigInt::from(plan.estimated_stage_bytes),
        estimate_is_hard_bound: plan.estimate_is_hard_bound,
        retained_input_bytes_after_commit: BigInt::from(plan.retained_input_bytes_after_commit),
        filesystem_available_bytes: plan.filesystem_available_bytes.map(BigInt::from),
    }
}

fn encode_format_migration_preflight(
    result: FormatMigrationPreflightResult,
) -> NativeFormatMigrationPreflightResult {
    NativeFormatMigrationPreflightResult {
        flushed: result.flushed,
        status: encode_format_migration_status(result.status),
        estimate: result.estimate.map(encode_format_migration_plan),
    }
}

fn encode_format_migration_step_result(
    result: FormatMigrationStepResult,
) -> NativeFormatMigrationStepResult {
    NativeFormatMigrationStepResult {
        flushed: result.flushed,
        step: result.step.map(|step| NativeFormatMigrationStep {
            plan: encode_format_migration_plan(step.plan),
            output_bytes: BigInt::from(step.output_bytes),
            remaining_legacy_parts: BigInt::from(step.remaining_legacy_parts as u64),
            rewrite: encode_merge(step.rewrite),
        }),
    }
}

fn encode_bounded_compact(result: BoundedCompactResult) -> NativeBoundedCompactResult {
    let (plan, output_bytes, merge) = match result.compaction {
        Some(compaction) => (
            Some(encode_compaction_plan(compaction.plan)),
            Some(BigInt::from(compaction.output_bytes)),
            Some(encode_merge(compaction.merge)),
        ),
        None => (None, None, None),
    };
    NativeBoundedCompactResult {
        flushed: result.flushed,
        parts_before: BigInt::from(result.parts_before as u64),
        parts_after: BigInt::from(result.parts_after as u64),
        plan,
        output_bytes,
        merge,
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

fn encode_refold_space(result: RefoldSpaceResult) -> NativeRefoldSpaceResult {
    NativeRefoldSpaceResult {
        flushed: result.flushed,
        estimate: result.estimate.map(|estimate| NativeRefoldSpaceEstimate {
            source_fold_logical_bytes: BigInt::from(estimate.source_fold_logical_bytes),
            source_part_bytes: BigInt::from(estimate.source_part_bytes),
            source_part_sections: BigInt::from(estimate.source_part_sections as u64),
            source_part_raw_section_bytes: BigInt::from(estimate.source_part_raw_section_bytes),
            retained_only_bytes_before: BigInt::from(estimate.retained_only_bytes_before),
            estimated_stage_bytes: BigInt::from(estimate.estimated_stage_bytes),
            estimate_is_hard_bound: estimate.estimate_is_hard_bound,
            filesystem_available_bytes: estimate.filesystem_available_bytes.map(BigInt::from),
        }),
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
        wal_frames: BigInt::from(health.wal_frames),
        fold_disk_bytes: BigInt::from(health.fold_disk_bytes),
        fold_segments: health.fold_segments,
        fold_cache_hits: BigInt::from(health.fold_cache_hits),
        fold_cache_misses: BigInt::from(health.fold_cache_misses),
        fold_cache_bytes: BigInt::from(health.fold_cache_bytes as u64),
        fold_cache_budget: BigInt::from(health.fold_cache_budget as u64),
        fold_block_target_bytes: BigInt::from(health.fold_block_target_bytes as u64),
        fold_segment_max_bytes: BigInt::from(u64::from(health.fold_segment_max_bytes)),
        fold_compression_level: health.fold_compression_level,
        fold_compression_threads: BigInt::from(health.fold_compression_threads as u64),
        part_cache_bytes: BigInt::from(health.part_cache_bytes as u64),
        part_cache_budget: BigInt::from(health.part_cache_budget as u64),
        max_stored_frame_bytes: BigInt::from(health.max_stored_frame_bytes),
        max_decoded_frame_bytes: BigInt::from(health.max_decoded_frame_bytes),
        max_directory_entries: BigInt::from(health.max_directory_entries),
        max_wal_frames: BigInt::from(health.max_wal_frames),
        max_fold_blocks: BigInt::from(health.max_fold_blocks),
        dedup_window_entries: BigInt::from(health.dedup_window_entries as u64),
        retained_commits: BigInt::from(health.retained_commits as u64),
        punched_blocks: BigInt::from(health.punched_blocks),
    }
}

fn encode_operation_metrics(
    metrics: turndb::observability::OperationMetrics,
) -> NativeOperationMetrics {
    NativeOperationMetrics {
        attempts: BigInt::from(metrics.attempts),
        succeeded: BigInt::from(metrics.succeeded),
        failed: BigInt::from(metrics.failed),
        cancelled: BigInt::from(metrics.cancelled),
        total_duration_ns: BigInt::from(metrics.total_duration_ns),
        last_duration_ns: BigInt::from(metrics.last_duration_ns),
        max_duration_ns: BigInt::from(metrics.max_duration_ns),
    }
}

fn encode_store_metrics(metrics: turndb::observability::StoreMetrics) -> NativeStoreMetrics {
    NativeStoreMetrics {
        open_recovery: encode_operation_metrics(metrics.open_recovery),
        recovered_wal_frames: BigInt::from(metrics.recovered_wal_frames),
        sync: encode_operation_metrics(metrics.sync),
        flush: encode_operation_metrics(metrics.flush),
        compaction: encode_operation_metrics(metrics.compaction),
        backup: encode_operation_metrics(metrics.backup),
        verification: encode_operation_metrics(metrics.verification),
        verification_corruption_failures: BigInt::from(metrics.verification_corruption_failures),
        punch: encode_operation_metrics(metrics.punch),
        refold: encode_operation_metrics(metrics.refold),
        format_migration: encode_operation_metrics(metrics.format_migration),
        folded_content: NativeFoldedContentMetrics {
            pieces: BigInt::from(metrics.folded_content.pieces),
            dedup_hits: BigInt::from(metrics.folded_content.dedup_hits),
            logical_bytes: BigInt::from(metrics.folded_content.logical_bytes),
            novel_bytes: BigInt::from(metrics.folded_content.novel_bytes),
        },
    }
}

fn encode_part_distribution(
    distribution: turndb::observability::PartDistribution,
) -> NativePartDistribution {
    NativePartDistribution {
        parts: BigInt::from(distribution.parts as u64),
        total_bytes: BigInt::from(distribution.total_bytes),
        min_bytes: BigInt::from(distribution.min_bytes),
        p50_bytes: BigInt::from(distribution.p50_bytes),
        p95_bytes: BigInt::from(distribution.p95_bytes),
        max_bytes: BigInt::from(distribution.max_bytes),
        total_rows: BigInt::from(distribution.total_rows),
        min_rows: BigInt::from(distribution.min_rows),
        p50_rows: BigInt::from(distribution.p50_rows),
        p95_rows: BigInt::from(distribution.p95_rows),
        max_rows: BigInt::from(distribution.max_rows),
    }
}

fn encode_content_liveness(
    liveness: turndb::observability::ContentLiveness,
) -> NativeContentLiveness {
    let encode_space = |space: turndb::observability::FoldBlockSpace| NativeFoldBlockSpace {
        blocks: BigInt::from(space.blocks),
        raw_bytes: BigInt::from(space.raw_bytes),
        stored_bytes: BigInt::from(space.stored_bytes),
    };
    NativeContentLiveness {
        live_pieces: BigInt::from(liveness.live_pieces),
        live_logical_bytes: BigInt::from(liveness.live_logical_bytes),
        dead_logical_bytes: BigInt::from(liveness.dead_logical_bytes),
        stranded_dead_logical_bytes: BigInt::from(liveness.stranded_dead_logical_bytes),
        live_blocks: encode_space(liveness.live_blocks),
        reclaimable_blocks: encode_space(liveness.reclaimable_blocks),
    }
}

fn encode_lifecycle_events(
    batch: turndb::observability::LifecycleEventBatch,
) -> NativeLifecycleEventBatch {
    NativeLifecycleEventBatch {
        events: batch
            .events
            .into_iter()
            .map(|event| NativeLifecycleEvent {
                sequence: BigInt::from(event.sequence),
                operation: event.operation.name().to_string(),
                outcome: event.outcome.name().to_string(),
                error_class: event.error_class.map(|class| class.code().to_string()),
                duration_ns: BigInt::from(event.duration_ns),
            })
            .collect(),
        oldest_available_sequence: batch.oldest_available_sequence.map(BigInt::from),
        latest_sequence: BigInt::from(batch.latest_sequence),
        dropped_events: BigInt::from(batch.dropped_events),
        gap: batch.gap,
    }
}

fn encode_space_amount(amount: turndb::store::SpaceAmount) -> NativeSpaceAmount {
    NativeSpaceAmount {
        files: BigInt::from(amount.files as u64),
        logical_bytes: BigInt::from(amount.logical_bytes),
        allocated_bytes: amount.allocated_bytes.map(BigInt::from),
    }
}

fn encode_space_usage(usage: turndb::store::StoreSpaceUsage) -> NativeStoreSpaceUsage {
    NativeStoreSpaceUsage {
        live: encode_space_amount(usage.live),
        retained_only: encode_space_amount(usage.retained_only),
        unclassified: encode_space_amount(usage.unclassified),
        total: encode_space_amount(usage.total),
        filesystem_available_bytes: usage.filesystem_available_bytes.map(BigInt::from),
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
                            turndb::schema::AttrType::UInt => "uint",
                            turndb::schema::AttrType::Binary => "binary",
                            turndb::schema::AttrType::TimestampNs => "timestamp_ns",
                            turndb::schema::AttrType::Null => "null",
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

#[cfg(all(test, feature = "sql"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropNotice(Arc<AtomicBool>);

    impl Drop for DropNotice {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn a_planning_deadline_drops_the_unfinished_future() {
        let dropped = Arc::new(AtomicBool::new(false));
        let notice = DropNotice(dropped.clone());
        let work = async move {
            let _notice = notice;
            std::future::pending::<Result<SqlQuery>>().await
        };
        let runtime = napi::tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(interruptible_sql_open(
            work,
            None,
            Some(Instant::now() + Duration::from_millis(1)),
        ));
        let error = match result {
            Ok(_) => panic!("a pending planning future beat its deadline"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("planning deadline exceeded"));
        assert!(dropped.load(Ordering::Acquire), "the losing planning future remained alive");
    }
}
