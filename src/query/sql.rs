//! Bounded, read-only SQL execution over an immutable TurnDB reader.
//!
//! This module is the embedding seam rather than a binding-specific convenience. SQL text and typed
//! values enter here; TurnDB configures DataFusion, registers the storage-backed table, enforces a
//! read-only plan, and emits independently decodable Arrow IPC batches. Bindings never translate a
//! dynamic query result row by row or inherit DataFusion's internal Rust types.

use super::table::TurndbTable;
use super::ScanStats;
use crate::store::ReadStore;
use anyhow::{bail, Context, Result};
use arrow::datatypes::SchemaRef;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use datafusion::error::DataFusionError;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::physical_plan::SendableRecordBatchStream;
use datafusion::prelude::{SQLOptions, SessionConfig, SessionContext};
use datafusion::scalar::ScalarValue;
use futures::StreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The stable table name exposed inside every isolated query session.
pub const TABLE_NAME: &str = "records";

/// A per-query DataFusion execution-memory ceiling. Output IPC bytes and TurnDB's bounded caches are
/// outside this pool and remain separately accountable.
pub const DEFAULT_MEMORY_BYTES: usize = 256 << 20;
/// Default aggregate reservation ceiling used by full-featured embedders.
pub const DEFAULT_AGGREGATE_MEMORY_BYTES: usize = 1 << 30;

/// A conservative shared governor for concurrent SQL execution. Each live query reserves its
/// configured execution ceiling, so the sum of possible DataFusion pool use cannot exceed `limit`.
#[derive(Clone)]
pub struct SqlBudget {
    inner: Arc<SqlBudgetInner>,
}

struct SqlBudgetInner {
    limit: usize,
    reserved: AtomicUsize,
}

#[derive(Debug)]
pub struct SqlBudgetExhausted {
    pub requested: usize,
    pub available: usize,
    pub limit: usize,
}

impl std::fmt::Display for SqlBudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SQL query requests {} execution-memory bytes but only {} of the shared {}-byte budget are available",
            self.requested, self.available, self.limit
        )
    }
}

impl std::error::Error for SqlBudgetExhausted {}

struct SqlReservation {
    budget: SqlBudget,
    bytes: usize,
}

impl Drop for SqlReservation {
    fn drop(&mut self) {
        self.budget.inner.reserved.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl SqlBudget {
    pub fn new(limit: usize) -> Result<SqlBudget> {
        if limit == 0 {
            bail!("aggregate SQL memory limit must be greater than zero");
        }
        Ok(SqlBudget { inner: Arc::new(SqlBudgetInner { limit, reserved: AtomicUsize::new(0) }) })
    }

    pub fn limit(&self) -> usize {
        self.inner.limit
    }

    pub fn reserved(&self) -> usize {
        self.inner.reserved.load(Ordering::Acquire)
    }

    fn reserve(&self, bytes: usize) -> Result<SqlReservation> {
        let mut current = self.reserved();
        loop {
            let available = self.inner.limit.saturating_sub(current);
            if bytes > available {
                return Err(SqlBudgetExhausted {
                    requested: bytes,
                    available,
                    limit: self.inner.limit,
                }
                .into());
            }
            match self.inner.reserved.compare_exchange_weak(
                current,
                current + bytes,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(SqlReservation { budget: self.clone(), bytes }),
                Err(observed) => current = observed,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlErrorClass {
    InvalidArgument,
    ResourceExhausted,
    Unsupported,
    Io,
    Internal,
}

/// Classify DataFusion's typed error tree without matching its display text. Errors originating in
/// TurnDB's still-untyped storage core remain Internal until the engine taxonomy can prove otherwise.
pub fn classify_error(error: &anyhow::Error) -> SqlErrorClass {
    if error.chain().any(|cause| cause.downcast_ref::<SqlBudgetExhausted>().is_some()) {
        return SqlErrorClass::ResourceExhausted;
    }
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<DataFusionError>())
        .map(classify_datafusion)
        .unwrap_or(SqlErrorClass::Internal)
}

fn classify_datafusion(error: &DataFusionError) -> SqlErrorClass {
    match error {
        DataFusionError::Context(_, source) => classify_datafusion(source),
        DataFusionError::External(source) => source
            .downcast_ref::<DataFusionError>()
            .map(classify_datafusion)
            .unwrap_or(SqlErrorClass::Internal),
        DataFusionError::SQL(..)
        | DataFusionError::Plan(_)
        | DataFusionError::SchemaError(..)
        | DataFusionError::Execution(_)
        | DataFusionError::Configuration(_) => SqlErrorClass::InvalidArgument,
        DataFusionError::ResourcesExhausted(_) => SqlErrorClass::ResourceExhausted,
        DataFusionError::NotImplemented(_) => SqlErrorClass::Unsupported,
        DataFusionError::IoError(_) => SqlErrorClass::Io,
        _ => SqlErrorClass::Internal,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlOptions {
    pub max_memory_bytes: usize,
}

impl Default for SqlOptions {
    fn default() -> Self {
        SqlOptions { max_memory_bytes: DEFAULT_MEMORY_BYTES }
    }
}

/// A positional SQL value. Callers use `$1`, `$2`, ... placeholders rather than interpolating text.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlValue {
    Null,
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Binary(Vec<u8>),
}

impl From<SqlValue> for ScalarValue {
    fn from(value: SqlValue) -> ScalarValue {
        match value {
            SqlValue::Null => ScalarValue::Null,
            SqlValue::String(value) => ScalarValue::Utf8(Some(value)),
            SqlValue::Int(value) => ScalarValue::Int64(Some(value)),
            SqlValue::Float(value) => ScalarValue::Float64(Some(value)),
            SqlValue::Bool(value) => ScalarValue::Boolean(Some(value)),
            SqlValue::Binary(value) => ScalarValue::Binary(Some(value)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlBatch {
    /// One complete Arrow IPC stream containing the query schema and exactly one record batch.
    pub ipc: Vec<u8>,
    pub rows: usize,
}

/// A pull-based query. Dropping it drops DataFusion's execution stream and cancels unfinished work.
pub struct SqlQuery {
    stream: Option<SendableRecordBatchStream>,
    schema_ipc: Vec<u8>,
    table: std::sync::Arc<TurndbTable>,
    finished: bool,
    reservation: Option<SqlReservation>,
}

impl SqlQuery {
    /// Plan a single read-only query over `store` and create its bounded execution stream.
    pub async fn open(
        store: ReadStore,
        sql: &str,
        params: Vec<SqlValue>,
        options: SqlOptions,
    ) -> Result<SqlQuery> {
        Self::open_inner(store, sql, params, options, None).await
    }

    /// Plan against a shared aggregate reservation budget.
    pub async fn open_with_budget(
        store: ReadStore,
        sql: &str,
        params: Vec<SqlValue>,
        options: SqlOptions,
        budget: &SqlBudget,
    ) -> Result<SqlQuery> {
        Self::open_inner(store, sql, params, options, Some(budget)).await
    }

    async fn open_inner(
        store: ReadStore,
        sql: &str,
        params: Vec<SqlValue>,
        options: SqlOptions,
        budget: Option<&SqlBudget>,
    ) -> Result<SqlQuery> {
        if sql.trim().is_empty() {
            bail!("SQL query must not be empty");
        }
        if options.max_memory_bytes == 0 {
            bail!("SQL max_memory_bytes must be greater than zero");
        }

        let reservation =
            budget.map(|budget| budget.reserve(options.max_memory_bytes)).transpose()?;

        // A separate RuntimeEnv makes the option an actual per-query ceiling rather than a hint on
        // whichever global context happened to exist. DataFusion documents that not every allocation
        // participates in the pool, so the API calls this execution memory, not total RSS.
        let runtime = RuntimeEnvBuilder::new()
            .with_memory_limit(options.max_memory_bytes, 1.0)
            .build_arc()
            .context("build bounded SQL runtime")?;
        let ctx = SessionContext::new_with_config_rt(SessionConfig::new(), runtime);
        let table =
            TurndbTable::register(store, &ctx, TABLE_NAME).context("register TurnDB SQL table")?;
        let read_only = SQLOptions::new()
            .with_allow_ddl(false)
            .with_allow_dml(false)
            .with_allow_statements(false);
        let mut frame =
            ctx.sql_with_options(sql, read_only).await.context("plan read-only TurnDB SQL")?;
        frame = frame
            .with_param_values(params.into_iter().map(ScalarValue::from).collect::<Vec<_>>())
            .context("bind TurnDB SQL parameters")?;
        let stream = frame.execute_stream().await.context("start TurnDB SQL execution")?;
        let schema_ipc = encode_schema(&stream.schema()).context("encode SQL result schema")?;
        Ok(SqlQuery { stream: Some(stream), schema_ipc, table, finished: false, reservation })
    }

    /// A complete zero-batch Arrow IPC stream carrying the result schema, available before pulling.
    pub fn schema_ipc(&self) -> &[u8] {
        &self.schema_ipc
    }

    /// Pull and encode one record batch. `None` is stable after the stream finishes.
    pub async fn next(&mut self) -> Result<Option<SqlBatch>> {
        if self.finished {
            return Ok(None);
        }
        let batch = match self
            .stream
            .as_mut()
            .expect("unfinished SQL query retains its stream")
            .next()
            .await
            .transpose()
            .context("execute TurnDB SQL batch")
        {
            Ok(Some(batch)) => batch,
            Ok(None) => {
                self.finished = true;
                self.stream = None;
                self.reservation = None;
                return Ok(None);
            }
            Err(error) => {
                self.finished = true;
                self.stream = None;
                self.reservation = None;
                return Err(error);
            }
        };
        let rows = batch.num_rows();
        let ipc = encode_batch(&batch).context("encode TurnDB SQL batch")?;
        Ok(Some(SqlBatch { ipc, rows }))
    }

    pub fn stats(&self) -> ScanStats {
        self.table.stats()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

fn encode_schema(schema: &SchemaRef) -> Result<Vec<u8>> {
    let mut ipc = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut ipc, schema)?;
        writer.finish()?;
    }
    Ok(ipc)
}

fn encode_batch(batch: &RecordBatch) -> Result<Vec<u8>> {
    let mut ipc = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut ipc, &batch.schema())?;
        writer.write(batch)?;
        writer.finish()?;
    }
    Ok(ipc)
}
