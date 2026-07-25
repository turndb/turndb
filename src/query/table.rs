//! DataFusion adapter — a thin shell over [`super::Lens`].
//!
//! Deliberately thin. Everything that decides how bytes become columns lives in the lens; this module
//! only teaches DataFusion how to ask. If SQL were removed tomorrow the scan layer would be untouched,
//! which is the seam a storage engine wants against a query engine it does not own.
//!
//! One partition per part, so parts scan in parallel and a merged part simply becomes one bigger
//! partition.

use super::{Lens, ScanStats, F_BODY};
use crate::fold::Fold;
use crate::part::Part;
use crate::store::ReadStore;
use anyhow::Result;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::{Session, TableProvider};
use datafusion::common::Result as DfResult;
use datafusion::datasource::TableType;
use datafusion::error::DataFusionError;
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::memory::MemoryStream;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
};
use futures::stream;
use datafusion::prelude::SessionContext;
use std::fmt;
use std::sync::{Arc, Mutex};

/// A turndb store presented to DataFusion as one table.
pub struct TurndbTable {
    parts: Vec<Arc<Part>>,
    fold: Arc<Fold>,
    lens: Arc<Lens>,
    /// Accumulated across every scan, so a test can assert what a query actually touched.
    stats: Arc<Mutex<ScanStats>>,
}

impl fmt::Debug for TurndbTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TurndbTable({} parts)", self.parts.len())
    }
}

impl TurndbTable {
    pub fn new(parts: Vec<Arc<Part>>, fold: Arc<Fold>) -> Result<TurndbTable> {
        let lens = Arc::new(Lens::new(&parts)?);
        Ok(TurndbTable { parts, fold, lens, stats: Arc::new(Mutex::new(ScanStats::default())) })
    }

    /// Register a read-only store as table `name` in a fresh session.
    pub fn context(store: ReadStore, name: &str) -> Result<(SessionContext, Arc<TurndbTable>)> {
        let (fold, parts) = store.into_parts();
        let t = Arc::new(TurndbTable::new(parts, fold)?);
        let ctx = SessionContext::new();
        ctx.register_table(name, t.clone())?;
        Ok((ctx, t))
    }

    /// What every scan through this table has touched so far.
    pub fn stats(&self) -> ScanStats {
        *self.stats.lock().unwrap()
    }

    pub fn reset_stats(&self) {
        *self.stats.lock().unwrap() = ScanStats::default();
    }
}

#[async_trait::async_trait]
impl TableProvider for TurndbTable {
    fn schema(&self) -> SchemaRef {
        self.lens.schema()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[datafusion::prelude::Expr],
        _limit: Option<usize>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        let full: Vec<usize> = (0..self.lens.schema().fields().len()).collect();
        let proj = projection.cloned().unwrap_or(full);
        Ok(Arc::new(TurndbExec::try_new(
            self.parts.clone(),
            self.fold.clone(),
            self.lens.clone(),
            proj,
            self.stats.clone(),
        )?))
    }
}

/// One partition per part, streamed.
///
/// Batches are produced on demand as the consumer pulls them, so peak residency is one batch —
/// `BATCH_BYTES` of content — regardless of how large the part is. Materialising a partition instead
/// would mean `SELECT id, body` over a 19 GiB corpus building 19 GiB of Arrow in one partition.
struct TurndbExec {
    parts: Vec<Arc<Part>>,
    fold: Arc<Fold>,
    lens: Arc<Lens>,
    projection: Vec<usize>,
    stats: Arc<Mutex<ScanStats>>,
    schema: SchemaRef,
    props: Arc<PlanProperties>,
}

impl TurndbExec {
    fn try_new(
        parts: Vec<Arc<Part>>,
        fold: Arc<Fold>,
        lens: Arc<Lens>,
        projection: Vec<usize>,
        stats: Arc<Mutex<ScanStats>>,
    ) -> DfResult<TurndbExec> {
        let full = lens.schema();
        let fields: Vec<_> = projection.iter().map(|&i| full.field(i).clone()).collect();
        let schema: SchemaRef = Arc::new(datafusion::arrow::datatypes::Schema::new(fields));
        let props = PlanProperties::new(
            EquivalenceProperties::new(schema.clone()),
            Partitioning::UnknownPartitioning(parts.len().max(1)),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Ok(TurndbExec { parts, fold, lens, projection, stats, schema, props: Arc::new(props) })
    }
}

impl fmt::Debug for TurndbExec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TurndbExec({} parts)", self.parts.len())
    }
}

impl DisplayAs for TurndbExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cols: Vec<&str> = self.schema.fields().iter().map(|x| x.name().as_str()).collect();
        write!(f, "TurndbExec: parts={}, projection=[{}]", self.parts.len(), cols.join(", "))
    }
}

impl ExecutionPlan for TurndbExec {
    fn name(&self) -> &str {
        "TurndbExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.props
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(self: Arc<Self>, _c: Vec<Arc<dyn ExecutionPlan>>) -> DfResult<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(&self, partition: usize, _ctx: Arc<TaskContext>) -> DfResult<SendableRecordBatchStream> {
        let Some(part) = self.parts.get(partition) else {
            return Ok(Box::pin(MemoryStream::try_new(vec![], self.schema.clone(), None)?));
        };
        // The fold is handed over only when `body` is in the projection, so an attribute-only query
        // cannot reach content even by mistake.
        let wants_body = self.schema.fields().iter().any(|f| f.name() == F_BODY);
        let fold = wants_body.then(|| &self.fold);

        let scan = self
            .lens
            .scan(part, fold, &self.projection)
            .map_err(|e| DataFusionError::External(e.into()))?;

        // `unfold` drives the scan one batch at a time. Decoding runs inline on the polling thread —
        // it is CPU work over already-cached sections rather than blocking I/O, and DataFusion spreads
        // partitions across its own worker threads, so a part is the unit of parallelism.
        // Progress is published per batch as a DELTA off the scan's own counters, so a query that
        // stops early — LIMIT, an error, a dropped stream — reports what it genuinely touched rather
        // than what a full scan would have.
        let s = stream::unfold(
            Some((scan, self.stats.clone(), ScanStats::default())),
            move |st| async move {
                let (mut scan, shared, prev) = st?;
                match scan.next_batch() {
                    Ok(Some(b)) => {
                        let now = scan.stats();
                        shared.lock().unwrap().add(now.since(prev));
                        Some((Ok(b), Some((scan, shared, now))))
                    }
                    Ok(None) => {
                        shared.lock().unwrap().add(scan.stats().since(prev));
                        None
                    }
                    Err(e) => Some((Err(DataFusionError::External(e.into())), None)),
                }
            },
        );
        Ok(Box::pin(RecordBatchStreamAdapter::new(self.schema.clone(), s)))
    }
}
