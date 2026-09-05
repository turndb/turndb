//! The concurrency seam between Node and the embedded engine.
//!
//! A `Store` is deliberately single-owner. Node may issue concurrent Promises, so each native
//! handle gives the store one dedicated Rust thread and submits bounded commands to it. This keeps
//! filesystem, compression, and sync work off the JavaScript event loop without introducing a
//! JavaScript mutex or duplicating storage semantics in the wrapper.

use anyhow::{anyhow, Context, Result};
use napi::tokio::sync::oneshot;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use turndb::carve::Carve;
use turndb::control::OperationControl;
use turndb::scan::{ScanExplanation, ScanPage, ScanRequest};
use turndb::store::{
    Batch, BoundedCompaction, CompactionBudget, ContentPunchStats, ContentSpans, ErasureStats,
    ReadStore, Store, StoreOptions,
};
use turndb::types::AttrValue;

pub(crate) struct CompactResult {
    pub flushed: bool,
    pub parts_before: usize,
    pub parts_after: usize,
    pub merge: Option<turndb::part::merge::MergeStats>,
}

pub(crate) struct BoundedCompactResult {
    pub flushed: bool,
    pub parts_before: usize,
    pub parts_after: usize,
    pub compaction: Option<BoundedCompaction>,
}

pub(crate) struct CompactionSpaceResult {
    pub flushed: bool,
    pub estimate: Option<turndb::store::CompactionSpaceEstimate>,
}

pub(crate) struct RefoldSpaceResult {
    pub flushed: bool,
    pub estimate: Option<turndb::store::RefoldSpaceEstimate>,
}

pub(crate) type VerifyResult = turndb::store::StoreVerification;

pub(crate) const DEFAULT_QUEUE_CAPACITY: usize = 64;
pub(crate) const MAX_QUEUE_CAPACITY: usize = 65_536;
const OPEN: u8 = 0;
const CLOSING_OR_CLOSED: u8 = 1;

#[derive(Debug)]
pub(crate) enum ActorFault {
    Busy { capacity: usize },
    Closed,
    WorkerExited,
}

impl std::fmt::Display for ActorFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActorFault::Busy { capacity } => write!(
                f,
                "store command queue is full (capacity {capacity}); retry after pending operations settle"
            ),
            ActorFault::Closed => write!(f, "store is closed"),
            ActorFault::WorkerExited => write!(f, "store worker has exited"),
        }
    }
}

impl std::error::Error for ActorFault {}

#[derive(Debug)]
pub(crate) struct OwnedContent {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum WriteOp {
    Put { id: String, contents: Vec<OwnedContent>, attrs: Vec<(String, AttrValue)> },
    Delete { id: String },
}

enum Command {
    Write {
        ops: Vec<WriteOp>,
        durable: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Sync {
        control: OperationControl,
        reply: oneshot::Sender<Result<()>>,
    },
    Flush {
        control: OperationControl,
        reply: oneshot::Sender<Result<bool>>,
    },
    Scan {
        request: ScanRequest,
        reply: oneshot::Sender<Result<ScanPage>>,
    },
    ExplainScan {
        request: ScanRequest,
        reply: oneshot::Sender<Result<ScanExplanation>>,
    },
    ReadContent {
        id: String,
        name: String,
        reply: oneshot::Sender<Result<Option<Vec<u8>>>>,
    },
    Snapshot {
        reply: oneshot::Sender<Result<ReadStore>>,
    },
    Compact {
        full: bool,
        control: OperationControl,
        reply: oneshot::Sender<Result<CompactResult>>,
    },
    CompactBounded {
        budget: CompactionBudget,
        control: OperationControl,
        reply: oneshot::Sender<Result<BoundedCompactResult>>,
    },
    EstimateCompactionSpace {
        budget: CompactionBudget,
        control: OperationControl,
        reply: oneshot::Sender<Result<CompactionSpaceResult>>,
    },
    Verify {
        control: OperationControl,
        reply: oneshot::Sender<Result<VerifyResult>>,
    },
    Backup {
        path: std::path::PathBuf,
        control: OperationControl,
        reply: oneshot::Sender<Result<turndb::backup::BackupStats>>,
    },
    Erase {
        ids: Vec<String>,
        control: OperationControl,
        reply: oneshot::Sender<Result<ErasureStats>>,
    },
    ContentPunch {
        control: OperationControl,
        reply: oneshot::Sender<Result<ContentPunchStats>>,
    },
    Refold {
        control: OperationControl,
        reply: oneshot::Sender<Result<turndb::store::refold::RefoldStats>>,
    },
    EstimateRefoldSpace {
        control: OperationControl,
        reply: oneshot::Sender<Result<RefoldSpaceResult>>,
    },
    Health {
        reply: oneshot::Sender<Result<turndb::store::StoreHealth>>,
    },
    Metrics {
        reply: oneshot::Sender<Result<turndb::observability::StoreMetrics>>,
    },
    LifecycleEvents {
        after_sequence: u64,
        limit: usize,
        reply: oneshot::Sender<Result<turndb::observability::LifecycleEventBatch>>,
    },
    PartDistribution {
        control: OperationControl,
        reply: oneshot::Sender<Result<turndb::observability::PartDistribution>>,
    },
    ContentLiveness {
        control: OperationControl,
        reply: oneshot::Sender<Result<turndb::observability::ContentLiveness>>,
    },
    SpaceUsage {
        control: OperationControl,
        reply: oneshot::Sender<Result<turndb::store::StoreSpaceUsage>>,
    },
    Schema {
        reply: oneshot::Sender<Result<turndb::schema::Schema>>,
    },
    Close {
        durable: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    #[cfg(test)]
    Hold {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    },
}

struct Inner {
    tx: mpsc::SyncSender<Command>,
    state: AtomicU8,
    capacity: usize,
}

#[derive(Clone)]
pub(crate) struct Actor {
    inner: Arc<Inner>,
}

impl Actor {
    #[cfg(test)]
    pub fn open_with_capacity(path: &Path, capacity: usize) -> Result<Actor> {
        Self::open_with_capacity_and_options(path, capacity, StoreOptions::default())
    }

    /// Open the single-file store and give it its dedicated thread. The path names a `.turndb`
    /// file, created if absent; the bridge's prepared-working-directory machinery has no seat
    /// here any more — the store IS the file.
    pub fn open_with_capacity_and_options(
        path: &Path,
        capacity: usize,
        options: StoreOptions,
    ) -> Result<Actor> {
        if !(1..=MAX_QUEUE_CAPACITY).contains(&capacity) {
            return Err(anyhow!(
                "command queue capacity must be between 1 and {MAX_QUEUE_CAPACITY}, got {capacity}"
            ));
        }
        let (tx, rx) = mpsc::sync_channel(capacity);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let path = path.to_path_buf();
        std::thread::Builder::new()
            .name("turndb-store".into())
            .spawn(move || match Store::open_file_with_options(&path, options) {
                Ok(store) => {
                    let _ = ready_tx.send(Ok(()));
                    run(store, &path, rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .context("spawn TurnDB store thread")?;
        match ready_rx.recv().context("TurnDB store thread exited during open")? {
            Ok(()) => {
                Ok(Actor { inner: Arc::new(Inner { tx, state: AtomicU8::new(OPEN), capacity }) })
            }
            Err(error) => Err(error).context("open TurnDB store"),
        }
    }

    fn submit<R: Send + 'static>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<R>>) -> Command,
    ) -> Result<oneshot::Receiver<Result<R>>> {
        if self.inner.state.load(Ordering::Acquire) != OPEN {
            return Err(ActorFault::Closed.into());
        }
        let (reply, receive) = oneshot::channel();
        self.inner.tx.try_send(make(reply)).map_err(|error| -> anyhow::Error {
            match error {
                mpsc::TrySendError::Full(_) => {
                    ActorFault::Busy { capacity: self.inner.capacity }.into()
                }
                mpsc::TrySendError::Disconnected(_) => ActorFault::WorkerExited.into(),
            }
        })?;
        Ok(receive)
    }

    pub fn queue_capacity(&self) -> usize {
        self.inner.capacity
    }

    async fn receive<R>(&self, receive: oneshot::Receiver<Result<R>>) -> Result<R> {
        match receive.await {
            Ok(result) => result,
            // The sender was dropped without a reply. `submit` checks state before enqueueing, but
            // it cannot check-and-enqueue atomically: a command can land behind Close in the queue
            // and be dropped when the worker returns. That command failed because the store
            // CLOSED, and must say so — "thread exited" is for a worker that actually died, which
            // is the only way the sender drops while the state still reads OPEN.
            Err(_) if self.inner.state.load(Ordering::Acquire) != OPEN => {
                Err(ActorFault::Closed.into())
            }
            Err(error) => Err(error).context("TurnDB store thread exited before replying"),
        }
    }

    pub async fn write(&self, ops: Vec<WriteOp>, durable: bool) -> Result<()> {
        self.receive(self.submit(|reply| Command::Write { ops, durable, reply })?).await
    }

    pub async fn sync(&self, control: OperationControl) -> Result<()> {
        self.receive(self.submit(|reply| Command::Sync { control, reply })?).await
    }

    pub async fn flush(&self, control: OperationControl) -> Result<bool> {
        self.receive(self.submit(|reply| Command::Flush { control, reply })?).await
    }

    pub async fn scan(&self, request: ScanRequest) -> Result<ScanPage> {
        self.receive(self.submit(|reply| Command::Scan { request, reply })?).await
    }

    pub async fn explain_scan(&self, request: ScanRequest) -> Result<ScanExplanation> {
        self.receive(self.submit(|reply| Command::ExplainScan { request, reply })?).await
    }

    pub async fn read_content(&self, id: String, name: String) -> Result<Option<Vec<u8>>> {
        self.receive(self.submit(|reply| Command::ReadContent { id, name, reply })?).await
    }

    pub async fn snapshot(&self) -> Result<ReadStore> {
        self.receive(self.submit(|reply| Command::Snapshot { reply })?).await
    }

    pub async fn compact(&self, full: bool, control: OperationControl) -> Result<CompactResult> {
        self.receive(self.submit(|reply| Command::Compact { full, control, reply })?).await
    }

    pub async fn compact_bounded(
        &self,
        budget: CompactionBudget,
        control: OperationControl,
    ) -> Result<BoundedCompactResult> {
        self.receive(self.submit(|reply| Command::CompactBounded { budget, control, reply })?).await
    }

    pub async fn estimate_compaction_space(
        &self,
        budget: CompactionBudget,
        control: OperationControl,
    ) -> Result<CompactionSpaceResult> {
        self.receive(self.submit(|reply| Command::EstimateCompactionSpace {
            budget,
            control,
            reply,
        })?)
        .await
    }

    pub async fn verify(&self, control: OperationControl) -> Result<VerifyResult> {
        self.receive(self.submit(|reply| Command::Verify { control, reply })?).await
    }

    pub async fn backup(
        &self,
        path: std::path::PathBuf,
        control: OperationControl,
    ) -> Result<turndb::backup::BackupStats> {
        self.receive(self.submit(|reply| Command::Backup { path, control, reply })?).await
    }

    pub async fn erase(&self, ids: Vec<String>, control: OperationControl) -> Result<ErasureStats> {
        self.receive(self.submit(|reply| Command::Erase { ids, control, reply })?).await
    }

    pub async fn content_punch(&self, control: OperationControl) -> Result<ContentPunchStats> {
        self.receive(self.submit(|reply| Command::ContentPunch { control, reply })?).await
    }

    pub async fn refold(
        &self,
        control: OperationControl,
    ) -> Result<turndb::store::refold::RefoldStats> {
        self.receive(self.submit(|reply| Command::Refold { control, reply })?).await
    }

    pub async fn estimate_refold_space(
        &self,
        control: OperationControl,
    ) -> Result<RefoldSpaceResult> {
        self.receive(self.submit(|reply| Command::EstimateRefoldSpace { control, reply })?).await
    }

    pub async fn health(&self) -> Result<turndb::store::StoreHealth> {
        self.receive(self.submit(|reply| Command::Health { reply })?).await
    }

    pub async fn metrics(&self) -> Result<turndb::observability::StoreMetrics> {
        self.receive(self.submit(|reply| Command::Metrics { reply })?).await
    }

    pub async fn lifecycle_events(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<turndb::observability::LifecycleEventBatch> {
        self.receive(self.submit(|reply| Command::LifecycleEvents {
            after_sequence,
            limit,
            reply,
        })?)
        .await
    }

    pub async fn part_distribution(
        &self,
        control: OperationControl,
    ) -> Result<turndb::observability::PartDistribution> {
        self.receive(self.submit(|reply| Command::PartDistribution { control, reply })?).await
    }

    pub async fn content_liveness(
        &self,
        control: OperationControl,
    ) -> Result<turndb::observability::ContentLiveness> {
        self.receive(self.submit(|reply| Command::ContentLiveness { control, reply })?).await
    }

    pub async fn space_usage(
        &self,
        control: OperationControl,
    ) -> Result<turndb::store::StoreSpaceUsage> {
        self.receive(self.submit(|reply| Command::SpaceUsage { control, reply })?).await
    }

    pub async fn schema(&self) -> Result<turndb::schema::Schema> {
        self.receive(self.submit(|reply| Command::Schema { reply })?).await
    }

    pub async fn close(&self, durable: bool) -> Result<()> {
        if self
            .inner
            .state
            .compare_exchange(OPEN, CLOSING_OR_CLOSED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ActorFault::Closed.into());
        }
        let (reply, receive) = oneshot::channel();
        let tx = self.inner.tx.clone();
        // Close must not become impossible merely because the bounded queue is momentarily full.
        // The blocking send runs on napi-rs's Tokio pool, never on the JavaScript event loop.
        let disconnected = napi::tokio::task::spawn_blocking(move || {
            tx.send(Command::Close { durable, reply }).is_err()
        })
        .await
        .context("join TurnDB close submission")?;
        if disconnected {
            return Err(ActorFault::WorkerExited.into());
        }
        // NOT the state-aware `receive`: this handle just moved the state off OPEN itself, so a
        // dropped reply here can only mean the worker died before processing Close — which is
        // "worker exited", never "you called an already-closed store".
        receive.await.context("TurnDB store thread exited before replying")?
    }
}

fn run(mut store: Store, path: &Path, rx: mpsc::Receiver<Command>) {
    while let Ok(command) = rx.recv() {
        match command {
            Command::Write { ops, durable, reply } => {
                let result = apply(&mut store, ops, durable);
                let _ = reply.send(result);
            }
            Command::Sync { control, reply } => {
                let _ = reply.send(store.sync_with_control(&control));
            }
            Command::Flush { control, reply } => {
                let _ = reply.send(store.flush_with_control(&control).map(|part| part.is_some()));
            }
            Command::Scan { request, reply } => {
                let _ = reply.send(store.scan(&request));
            }
            Command::ExplainScan { request, reply } => {
                let _ = reply.send(store.explain_scan(&request));
            }
            Command::ReadContent { id, name, reply } => {
                let _ = reply.send(store.reconstruct_content(&id, &name));
            }
            Command::Snapshot { reply } => {
                // A reader never replays the writer's WAL. Flush is therefore the only honest way
                // to include every earlier accepted write in an immutable view, and actor
                // serialization makes this an exact cut rather than a race around `open_read`.
                let read_limits = store.read_limits();
                // The snapshot inherits the writer's fold configuration as well as its read
                // limits: a custom cache or block policy must not silently revert to defaults in
                // readers derived from this handle.
                let fold_cfg = store.fold_cfg();
                let result = store
                    .flush()
                    .and_then(|_| {
                        turndb::store::open_read_container_with_limits(path, fold_cfg, read_limits)
                    })
                    .context("publish immutable reader snapshot");
                let _ = reply.send(result);
            }
            Command::Compact { full, control, reply } => {
                let result = compact(&mut store, full, &control);
                let _ = reply.send(result);
            }
            Command::CompactBounded { budget, control, reply } => {
                let result = compact_bounded(&mut store, budget, &control);
                let _ = reply.send(result);
            }
            Command::EstimateCompactionSpace { budget, control, reply } => {
                let result = estimate_compaction_space(&mut store, budget, &control);
                let _ = reply.send(result);
            }
            Command::Verify { control, reply } => {
                let result = verify(&mut store, &control);
                let _ = reply.send(result);
            }
            Command::Backup { path, control, reply } => {
                // Actor order fixes the backup boundary: earlier writes are synchronized and
                // published by `Store::backup`, while later commands wait for artifact installation.
                let _ = reply.send(store.backup_with_control(&path, &control));
            }
            Command::Erase { ids, control, reply } => {
                let _ = reply.send(store.erase_ids_with_control(&ids, &control));
            }
            Command::ContentPunch { control, reply } => {
                let result = control
                    .check("content punching")
                    .map_err(anyhow::Error::from)
                    .and_then(|_| publish_pending(&mut store, &control))
                    .and_then(|_| store.punch_unreferenced_with_control(&control));
                let _ = reply.send(result);
            }
            Command::Refold { control, reply } => {
                let result = control
                    .check("content refold")
                    .map_err(anyhow::Error::from)
                    .and_then(|_| publish_pending(&mut store, &control))
                    .and_then(|_| store.refold_with_control(&control));
                let _ = reply.send(result);
            }
            Command::EstimateRefoldSpace { control, reply } => {
                let result = estimate_refold_space(&mut store, &control);
                let _ = reply.send(result);
            }
            Command::Health { reply } => {
                let _ = reply.send(Ok(store.health()));
            }
            Command::Metrics { reply } => {
                let _ = reply.send(Ok(store.metrics()));
            }
            Command::LifecycleEvents { after_sequence, limit, reply } => {
                let _ = reply.send(Ok(store.lifecycle_events_after(after_sequence, limit)));
            }
            Command::PartDistribution { control, reply } => {
                let _ = reply.send(store.part_distribution_with_control(&control));
            }
            Command::ContentLiveness { control, reply } => {
                let _ = reply.send(store.content_liveness_with_control(&control));
            }
            Command::SpaceUsage { control, reply } => {
                let _ = reply.send(store.space_usage_with_control(&control));
            }
            Command::Schema { reply } => {
                let _ = reply.send(store.schema());
            }
            Command::Close { durable, reply } => {
                // A durable close settles the store to exactly one file: acknowledged writes
                // synced, the memtable flushed, the emptied WAL sidecar removed. A non-durable
                // close deliberately leaves the sidecar — the caller asked not to settle, and
                // the next open replays it exactly as a crash would.
                // Close acknowledgement includes releasing the OS writer lock. Sending first and
                // dropping on function exit leaves a real race for immediate reopen/WAL replay.
                let result = if durable {
                    store.sync().and_then(|_| store.close())
                } else {
                    drop(store);
                    Ok(())
                };
                let _ = reply.send(result);
                return;
            }
            #[cfg(test)]
            Command::Hold { entered, release } => {
                let _ = entered.send(());
                let _ = release.recv();
            }
        }
    }
}

fn apply(store: &mut Store, ops: Vec<WriteOp>, durable: bool) -> Result<()> {
    let carve = Carve::default();
    let mut batch = Batch::new();
    for op in &ops {
        match op {
            WriteOp::Put { id, contents, attrs } => {
                let contents: Vec<_> = contents
                    .iter()
                    .map(|content| ContentSpans::carve(&content.name, &content.bytes, &carve))
                    .collect();
                batch.put_record(id, &contents, attrs.clone())?;
            }
            WriteOp::Delete { id } => batch.delete(id),
        }
    }
    store.apply(batch)?;
    if durable {
        store.sync()?;
    }
    Ok(())
}

fn publish_pending(store: &mut Store, control: &OperationControl) -> Result<bool> {
    store.sync_with_control(control)?;
    Ok(store.flush_with_control(control)?.is_some())
}

fn compact(store: &mut Store, full: bool, control: &OperationControl) -> Result<CompactResult> {
    control.check("part compaction")?;
    let flushed = publish_pending(store, control)?;
    control.check("part compaction")?;
    let parts_before = store.part_count();
    let merge = if full {
        store.merge_range_with_control(0, parts_before, control)?
    } else {
        store.auto_compact_with_control(control)?
    };
    Ok(CompactResult { flushed, parts_before, parts_after: store.part_count(), merge })
}

fn compact_bounded(
    store: &mut Store,
    budget: CompactionBudget,
    control: &OperationControl,
) -> Result<BoundedCompactResult> {
    control.check("bounded compaction")?;
    let flushed = publish_pending(store, control)?;
    control.check("bounded compaction")?;
    let parts_before = store.part_count();
    let compaction = store.compact_bounded_with_control(budget, control)?;
    Ok(BoundedCompactResult { flushed, parts_before, parts_after: store.part_count(), compaction })
}

fn estimate_compaction_space(
    store: &mut Store,
    budget: CompactionBudget,
    control: &OperationControl,
) -> Result<CompactionSpaceResult> {
    control.check("compaction space preflight")?;
    let flushed = publish_pending(store, control)?;
    control.check("compaction space preflight")?;
    let estimate = store.estimate_compaction_space_with_control(budget, control)?;
    Ok(CompactionSpaceResult { flushed, estimate })
}

fn estimate_refold_space(
    store: &mut Store,
    control: &OperationControl,
) -> Result<RefoldSpaceResult> {
    control.check("refold space preflight")?;
    let flushed = publish_pending(store, control)?;
    control.check("refold space preflight")?;
    let estimate = store.estimate_refold_space_with_control(control)?;
    Ok(RefoldSpaceResult { flushed, estimate })
}

fn verify(store: &mut Store, control: &OperationControl) -> Result<VerifyResult> {
    // Synchronization and publication make the report cover every operation accepted before this
    // command, and actor serialization prevents a new manifest from racing the chain walk.
    control.check("store verification")?;
    publish_pending(store, control)?;
    control.check("store verification")?;
    store.verify_with_control(control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use turndb::scan::{ContentMode, ContentSelect};

    fn temp() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "turndb-node-actor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn actor_preserves_order_duplicates_named_content_and_deletes() {
        let runtime = napi::tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let dir = temp();
            let actor = Actor::open_with_capacity(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
            actor
                .write(
                    vec![WriteOp::Put {
                        id: "trace/1".into(),
                        contents: vec![
                            OwnedContent { name: "input".into(), bytes: b"same".to_vec() },
                            OwnedContent { name: "output".into(), bytes: b"same".to_vec() },
                        ],
                        attrs: vec![
                            ("tag".into(), AttrValue::Str("a".into())),
                            ("tag".into(), AttrValue::Str("b".into())),
                            ("wide".into(), AttrValue::Int(i64::MIN)),
                        ],
                    }],
                    true,
                )
                .await
                .unwrap();
            let page = actor
                .scan(ScanRequest {
                    attrs: vec!["tag".into(), "wide".into()],
                    contents: vec![ContentSelect {
                        name: "output".into(),
                        mode: ContentMode::Bytes,
                    }],
                    ..ScanRequest::default()
                })
                .await
                .unwrap();
            assert_eq!(page.rows.len(), 1);
            assert_eq!(page.rows[0].attrs.len(), 3);
            assert_eq!(page.rows[0].contents[0].bytes.as_deref(), Some(b"same".as_slice()));

            actor.write(vec![WriteOp::Delete { id: "trace/1".into() }], true).await.unwrap();
            assert!(actor.scan(ScanRequest::default()).await.unwrap().rows.is_empty());
            actor.close(true).await.unwrap();
            std::fs::remove_dir_all(dir).unwrap();
        });
    }

    #[test]
    fn bounded_queue_refuses_the_first_command_beyond_capacity() {
        let dir = temp();
        let capacity = 2;
        let actor = Actor::open_with_capacity(&dir, capacity).unwrap();
        assert_eq!(actor.queue_capacity(), capacity);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        actor.inner.tx.send(Command::Hold { entered: entered_tx, release: release_rx }).unwrap();
        entered_rx.recv().unwrap();

        let mut queued = Vec::new();
        for _ in 0..capacity {
            queued.push(
                actor
                    .submit(|reply| Command::Sync { control: OperationControl::default(), reply })
                    .unwrap(),
            );
        }
        let error = actor
            .submit(|reply| Command::Sync { control: OperationControl::default(), reply })
            .unwrap_err();
        assert!(error.downcast_ref::<ActorFault>().is_some_and(|fault| {
            matches!(fault, ActorFault::Busy { capacity: 2 })
                && fault.to_string().contains("capacity 2")
        }));

        release_tx.send(()).unwrap();
        drop(queued);
        napi::tokio::runtime::Runtime::new().unwrap().block_on(actor.close(false)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `submit` checks state and enqueues non-atomically, so a command can pass the check while
    /// OPEN and still land BEHIND a concurrent close's Close command; the worker exits at Close
    /// and drops it unprocessed. That command failed because the store closed, and its promise
    /// must say CLOSED — not report an internal worker failure for an orderly shutdown. This
    /// constructs the exact losing interleaving: Close enqueued first, the straggler admitted
    /// while the state still reads OPEN, then the state flipped as close() would have.
    #[test]
    fn a_command_racing_a_close_is_refused_as_closed_not_internal() {
        let dir = temp();
        let actor = Actor::open_with_capacity(&dir, 4).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        actor.inner.tx.send(Command::Hold { entered: entered_tx, release: release_rx }).unwrap();
        entered_rx.recv().unwrap();

        let (close_reply, close_receive) = oneshot::channel();
        actor.inner.tx.send(Command::Close { durable: false, reply: close_reply }).unwrap();
        let straggler = actor
            .submit(|reply| Command::Sync { control: OperationControl::default(), reply })
            .expect("the state still reads OPEN, so admission must succeed");
        actor.inner.state.store(CLOSING_OR_CLOSED, Ordering::Release);

        release_tx.send(()).unwrap();
        let runtime = napi::tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(close_receive).unwrap().unwrap();
        let error = runtime.block_on(actor.receive(straggler)).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ActorFault>(), Some(ActorFault::Closed)),
            "a command dropped by an orderly close must classify as CLOSED, got: {error:#}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn queue_capacity_is_bounded_and_defaults_compatibly() {
        let dir = temp();
        let actor = Actor::open_with_capacity(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
        assert_eq!(actor.queue_capacity(), DEFAULT_QUEUE_CAPACITY);
        napi::tokio::runtime::Runtime::new().unwrap().block_on(actor.close(false)).unwrap();
        assert!(Actor::open_with_capacity(&dir, 0)
            .err()
            .unwrap()
            .to_string()
            .contains("between 1"));
        assert!(Actor::open_with_capacity(&dir, MAX_QUEUE_CAPACITY + 1)
            .err()
            .unwrap()
            .to_string()
            .contains(&MAX_QUEUE_CAPACITY.to_string()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_scan_deadline_includes_time_waiting_in_the_actor_queue() {
        let dir = temp();
        let actor = Actor::open_with_capacity(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        actor.inner.tx.send(Command::Hold { entered: entered_tx, release: release_rx }).unwrap();
        entered_rx.recv().unwrap();

        let runtime = napi::tokio::runtime::Runtime::new().unwrap();
        let scan_actor = actor.clone();
        let pending = runtime.spawn(async move {
            scan_actor
                .scan(ScanRequest {
                    deadline: Some(
                        std::time::Instant::now() + std::time::Duration::from_millis(10),
                    ),
                    ..ScanRequest::default()
                })
                .await
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        release_tx.send(()).unwrap();
        let error = runtime.block_on(pending).unwrap().unwrap_err();
        assert!(error.downcast_ref::<turndb::scan::ScanInterrupted>().is_some_and(|error| {
            error.reason == turndb::scan::ScanInterruptionReason::DeadlineExceeded
        }));
        runtime.block_on(actor.close(false)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_lifecycle_deadline_includes_time_waiting_in_the_actor_queue() {
        let dir = temp();
        let actor = Actor::open_with_capacity(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        actor.inner.tx.send(Command::Hold { entered: entered_tx, release: release_rx }).unwrap();
        entered_rx.recv().unwrap();

        let runtime = napi::tokio::runtime::Runtime::new().unwrap();
        let verify_actor = actor.clone();
        let pending = runtime.spawn(async move {
            verify_actor
                .verify(OperationControl {
                    deadline: Some(
                        std::time::Instant::now() + std::time::Duration::from_millis(10),
                    ),
                    cancellation: None,
                })
                .await
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        release_tx.send(()).unwrap();
        let error = match runtime.block_on(pending).unwrap() {
            Ok(_) => panic!("expired lifecycle work must refuse before settling the store"),
            Err(error) => error,
        };
        assert!(error.downcast_ref::<turndb::control::OperationInterrupted>().is_some_and(
            |error| { error.reason == turndb::control::InterruptionReason::DeadlineExceeded }
        ));
        runtime.block_on(actor.close(false)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_backup_deadline_includes_actor_queue_time_and_publishes_nothing() {
        let dir = temp();
        let artifact = dir.with_extension("cancelled-backup");
        let actor = Actor::open_with_capacity(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        actor.inner.tx.send(Command::Hold { entered: entered_tx, release: release_rx }).unwrap();
        entered_rx.recv().unwrap();

        let runtime = napi::tokio::runtime::Runtime::new().unwrap();
        let backup_actor = actor.clone();
        let output = artifact.clone();
        let pending = runtime.spawn(async move {
            backup_actor
                .backup(
                    output,
                    OperationControl {
                        deadline: Some(
                            std::time::Instant::now() + std::time::Duration::from_millis(10),
                        ),
                        cancellation: None,
                    },
                )
                .await
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        release_tx.send(()).unwrap();
        let error = runtime.block_on(pending).unwrap().unwrap_err();
        assert!(error.downcast_ref::<turndb::control::OperationInterrupted>().is_some_and(
            |error| { error.reason == turndb::control::InterruptionReason::DeadlineExceeded }
        ));
        assert!(!artifact.exists());
        runtime.block_on(actor.close(false)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_flush_deadline_includes_actor_queue_time_and_keeps_the_memtable() {
        let dir = temp();
        let actor = Actor::open_with_capacity(&dir, DEFAULT_QUEUE_CAPACITY).unwrap();
        let runtime = napi::tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(actor.write(
                vec![WriteOp::Put {
                    id: "pending".into(),
                    contents: Vec::new(),
                    attrs: Vec::new(),
                }],
                false,
            ))
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        actor.inner.tx.send(Command::Hold { entered: entered_tx, release: release_rx }).unwrap();
        entered_rx.recv().unwrap();

        let flush_actor = actor.clone();
        let pending = runtime.spawn(async move {
            flush_actor
                .flush(OperationControl {
                    deadline: Some(
                        std::time::Instant::now() + std::time::Duration::from_millis(10),
                    ),
                    cancellation: None,
                })
                .await
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        release_tx.send(()).unwrap();
        let error = runtime.block_on(pending).unwrap().unwrap_err();
        assert!(error.downcast_ref::<turndb::control::OperationInterrupted>().is_some_and(
            |error| { error.reason == turndb::control::InterruptionReason::DeadlineExceeded }
        ));
        assert_eq!(runtime.block_on(actor.health()).unwrap().memtable_entries, 1);
        runtime.block_on(actor.close(false)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
