//! DataFusion adapter — a thin shell over [`super::Lens`].
//!
//! Deliberately thin. Everything that decides how bytes become columns lives in the lens; this module
//! only teaches DataFusion how to ask. If SQL were removed tomorrow the scan layer would be untouched,
//! which is the seam a storage engine wants against a query engine it does not own.
//!
//! One partition per part, so parts scan in parallel and a merged part simply becomes one bigger
//! partition.

use super::{Cmp, Lens, Pred, ScanStats, F_BODY};
use crate::fold::Fold;
use crate::part::Part;
use crate::store::ReadStore;
use crate::types::AttrValue;
use anyhow::Result;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::{Session, TableProvider};
use datafusion::common::Result as DfResult;
use datafusion::datasource::TableType;
use datafusion::logical_expr::{Operator, TableProviderFilterPushDown};
use datafusion::scalar::ScalarValue;
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

    /// Every filter is INEXACT: the scan may return extra rows and the engine re-applies the
    /// predicate above it. That asymmetry is deliberate — it lets the scan skip aggressively without
    /// owning correctness, so an unhandled type or a malformed value costs work, never an answer.
    fn supports_filters_pushdown(
        &self,
        filters: &[&datafusion::prelude::Expr],
    ) -> DfResult<Vec<TableProviderFilterPushDown>> {
        Ok(filters
            .iter()
            .map(|f| match to_pred(f, &self.lens) {
                Some(_) => TableProviderFilterPushDown::Inexact,
                None => TableProviderFilterPushDown::Unsupported,
            })
            .collect())
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[datafusion::prelude::Expr],
        _limit: Option<usize>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        let full: Vec<usize> = (0..self.lens.schema().fields().len()).collect();
        let proj = projection.cloned().unwrap_or(full);
        let preds: Vec<Pred> = filters.iter().filter_map(|f| to_pred(f, &self.lens)).collect();
        Ok(Arc::new(TurndbExec::try_new(
            self.parts.clone(),
            self.fold.clone(),
            self.lens.clone(),
            proj,
            preds,
            self.stats.clone(),
        )?))
    }
}

/// `column <op> literal`, in either order. Anything else declines, and the engine keeps it.
fn to_pred(e: &datafusion::prelude::Expr, lens: &Lens) -> Option<Pred> {
    use datafusion::prelude::Expr;
    let Expr::BinaryExpr(b) = e else { return None };
    let (name, op, lit) = match (&*b.left, &*b.right) {
        (Expr::Column(c), Expr::Literal(v, _)) => (&c.name, b.op, v),
        // reversed: `5 < n` is `n > 5`
        (Expr::Literal(v, _), Expr::Column(c)) => (&c.name, flip(b.op)?, v),
        _ => return None,
    };
    let field = lens.schema().index_of(name).ok()?;
    let op = match op {
        Operator::Eq => Cmp::Eq,
        Operator::NotEq => Cmp::Ne,
        Operator::Lt => Cmp::Lt,
        Operator::LtEq => Cmp::LtEq,
        Operator::Gt => Cmp::Gt,
        Operator::GtEq => Cmp::GtEq,
        _ => return None,
    };
    let val = scalar_to_attr(lit)?;
    Some(Pred { field, op, val })
}

/// A literal, unwrapped to the value the columns actually hold.
///
/// The dictionary case is the one that matters: a string column surfaces as `Dictionary(Int32, Utf8)`,
/// so the planner coerces the literal to match and the comparison arrives wrapped. Missing that meant
/// every string predicate silently declined to push down — the tests still passed, because Inexact
/// pushdown failing simply means the engine does the work instead.
fn scalar_to_attr(v: &ScalarValue) -> Option<AttrValue> {
    Some(match v {
        ScalarValue::Dictionary(_, inner) => return scalar_to_attr(inner),
        ScalarValue::Utf8(Some(s)) | ScalarValue::LargeUtf8(Some(s)) | ScalarValue::Utf8View(Some(s)) => {
            AttrValue::Str(s.clone())
        }
        ScalarValue::Int64(Some(i)) => AttrValue::Int(*i),
        ScalarValue::Int32(Some(i)) => AttrValue::Int(*i as i64),
        ScalarValue::Int16(Some(i)) => AttrValue::Int(*i as i64),
        ScalarValue::Int8(Some(i)) => AttrValue::Int(*i as i64),
        ScalarValue::UInt32(Some(i)) => AttrValue::Int(*i as i64),
        ScalarValue::Float64(Some(f)) => AttrValue::Float(*f),
        ScalarValue::Float32(Some(f)) => AttrValue::Float(*f as f64),
        ScalarValue::Boolean(Some(b)) => AttrValue::Bool(*b),
        _ => return None,
    })
}

fn flip(op: Operator) -> Option<Operator> {
    Some(match op {
        Operator::Eq => Operator::Eq,
        Operator::NotEq => Operator::NotEq,
        Operator::Lt => Operator::Gt,
        Operator::LtEq => Operator::GtEq,
        Operator::Gt => Operator::Lt,
        Operator::GtEq => Operator::LtEq,
        _ => return None,
    })
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
    preds: Vec<Pred>,
    stats: Arc<Mutex<ScanStats>>,
    schema: SchemaRef,
    props: Arc<PlanProperties>,
    /// A LIMIT pushed down by the planner. Bounds WORK, which streaming alone does not.
    fetch: Option<usize>,
}

impl TurndbExec {
    fn try_new(
        parts: Vec<Arc<Part>>,
        fold: Arc<Fold>,
        lens: Arc<Lens>,
        projection: Vec<usize>,
        preds: Vec<Pred>,
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
        Ok(TurndbExec { parts, fold, lens, projection, preds, stats, schema, props: Arc::new(props), fetch: None })
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

    // NOT supports_limit_pushdown: that means "a limit may be pushed THROUGH this node to its INPUT".
    // A leaf has no input, so claiming it makes the planner drop the limit rather than apply it —
    // which returned 300 rows for `LIMIT 5`. A leaf implements with_fetch/fetch and nothing else.
    fn fetch(&self) -> Option<usize> {
        self.fetch
    }

    /// Each partition takes the FULL limit, not a share of it. Partitions are combined above, so a
    /// per-partition share could under-deliver when one part is short; over-fetching is bounded and
    /// correct, and still turns O(parts x batch) into O(parts x limit).
    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        Some(Arc::new(TurndbExec {
            parts: self.parts.clone(),
            fold: self.fold.clone(),
            lens: self.lens.clone(),
            projection: self.projection.clone(),
            preds: self.preds.clone(),
            stats: self.stats.clone(),
            schema: self.schema.clone(),
            props: self.props.clone(),
            fetch: limit,
        }))
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
            .scan(part, fold, &self.projection, &self.preds)
            .map_err(|e| DataFusionError::External(e.into()))?
            .with_fetch(self.fetch);

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
