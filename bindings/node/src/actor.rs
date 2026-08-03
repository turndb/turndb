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
use turndb::error::{classify, ErrorClass, IntegrityError};
use turndb::fold::FoldCfg;
use turndb::scan::{ScanExplanation, ScanPage, ScanRequest};
use turndb::store::{
    Batch, BoundedCompaction, ChainReport, CompactionBudget, ContentSpans, ErasureStats,
    PunchStats, ReadStore, Store, WriteLimits,
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

pub(crate) struct VerifyResult {
    pub chain: ChainReport,
    pub fold: turndb::fold::FoldScrub,
    pub parts: usize,
    pub part_sections: usize,
}

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
        reply: oneshot::Sender<Result<()>>,
    },
    Flush {
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
    Verify {
        control: OperationControl,
        reply: oneshot::Sender<Result<VerifyResult>>,
    },
    Backup {
        path: std::path::PathBuf,
        control: OperationControl,
        reply: oneshot::Sender<Result<turndb::pack::BackupStats>>,
    },
    Erase {
        ids: Vec<String>,
        control: OperationControl,
        reply: oneshot::Sender<Result<ErasureStats>>,
    },
    Punch {
        control: OperationControl,
        reply: oneshot::Sender<Result<PunchStats>>,
    },
    Refold {
        control: OperationControl,
        reply: oneshot::Sender<Result<turndb::store::refold::RefoldStats>>,
    },
    Health {
        reply: oneshot::Sender<Result<turndb::store::StoreHealth>>,
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
        Self::open_with_capacity_and_limits(path, capacity, WriteLimits::default())
    }

    pub fn open_with_capacity_and_limits(
        path: &Path,
        capacity: usize,
        write_limits: WriteLimits,
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
            .spawn(move || match Store::open_with_limits(&path, FoldCfg::default(), write_limits) {
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

    async fn receive<R>(receive: oneshot::Receiver<Result<R>>) -> Result<R> {
        receive.await.context("TurnDB store thread exited before replying")?
    }

    pub async fn write(&self, ops: Vec<WriteOp>, durable: bool) -> Result<()> {
        Self::receive(self.submit(|reply| Command::Write { ops, durable, reply })?).await
    }

    pub async fn sync(&self) -> Result<()> {
        Self::receive(self.submit(|reply| Command::Sync { reply })?).await
    }

    pub async fn flush(&self) -> Result<bool> {
        Self::receive(self.submit(|reply| Command::Flush { reply })?).await
    }

    pub async fn scan(&self, request: ScanRequest) -> Result<ScanPage> {
        Self::receive(self.submit(|reply| Command::Scan { request, reply })?).await
    }

    pub async fn explain_scan(&self, request: ScanRequest) -> Result<ScanExplanation> {
        Self::receive(self.submit(|reply| Command::ExplainScan { request, reply })?).await
    }

    pub async fn read_content(&self, id: String, name: String) -> Result<Option<Vec<u8>>> {
        Self::receive(self.submit(|reply| Command::ReadContent { id, name, reply })?).await
    }

    pub async fn snapshot(&self) -> Result<ReadStore> {
        Self::receive(self.submit(|reply| Command::Snapshot { reply })?).await
    }

    pub async fn compact(&self, full: bool, control: OperationControl) -> Result<CompactResult> {
        Self::receive(self.submit(|reply| Command::Compact { full, control, reply })?).await
    }

    pub async fn compact_bounded(
        &self,
        budget: CompactionBudget,
        control: OperationControl,
    ) -> Result<BoundedCompactResult> {
        Self::receive(self.submit(|reply| Command::CompactBounded { budget, control, reply })?)
            .await
    }

    pub async fn verify(&self, control: OperationControl) -> Result<VerifyResult> {
        Self::receive(self.submit(|reply| Command::Verify { control, reply })?).await
    }

    pub async fn backup(
        &self,
        path: std::path::PathBuf,
        control: OperationControl,
    ) -> Result<turndb::pack::BackupStats> {
        Self::receive(self.submit(|reply| Command::Backup { path, control, reply })?).await
    }

    pub async fn erase(&self, ids: Vec<String>, control: OperationControl) -> Result<ErasureStats> {
        Self::receive(self.submit(|reply| Command::Erase { ids, control, reply })?).await
    }

    pub async fn punch(&self, control: OperationControl) -> Result<PunchStats> {
        Self::receive(self.submit(|reply| Command::Punch { control, reply })?).await
    }

    pub async fn refold(
        &self,
        control: OperationControl,
    ) -> Result<turndb::store::refold::RefoldStats> {
        Self::receive(self.submit(|reply| Command::Refold { control, reply })?).await
    }

    pub async fn health(&self) -> Result<turndb::store::StoreHealth> {
        Self::receive(self.submit(|reply| Command::Health { reply })?).await
    }

    pub async fn schema(&self) -> Result<turndb::schema::Schema> {
        Self::receive(self.submit(|reply| Command::Schema { reply })?).await
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
        Self::receive(receive).await
    }
}

fn run(mut store: Store, path: &Path, rx: mpsc::Receiver<Command>) {
    while let Ok(command) = rx.recv() {
        match command {
            Command::Write { ops, durable, reply } => {
                let result = apply(&mut store, ops, durable);
                let _ = reply.send(result);
            }
            Command::Sync { reply } => {
                let _ = reply.send(store.sync());
            }
            Command::Flush { reply } => {
                let _ = reply.send(store.flush().map(|part| part.is_some()));
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
                let result = store
                    .flush()
                    .and_then(|_| Store::open_read(path, FoldCfg::default()))
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
            Command::Verify { control, reply } => {
                let result = verify(&mut store, path, &control);
                let _ = reply.send(result);
            }
            Command::Backup { path, control, reply } => {
                // Actor order fixes the backup cut: earlier writes are settled by `Store::backup`,
                // while later commands wait until the verified artifact has been published.
                let _ = reply.send(store.backup_with_control(&path, &control));
            }
            Command::Erase { ids, control, reply } => {
                let _ = reply.send(store.erase_ids_with_control(&ids, &control));
            }
            Command::Punch { control, reply } => {
                let result = control
                    .check("content punching")
                    .map_err(anyhow::Error::from)
                    .and_then(|_| settle(&mut store))
                    .and_then(|_| store.punch_unreferenced_with_control(&control));
                let _ = reply.send(result);
            }
            Command::Refold { control, reply } => {
                let result = control
                    .check("content refold")
                    .map_err(anyhow::Error::from)
                    .and_then(|_| settle(&mut store))
                    .and_then(|_| store.refold_with_control(&control));
                let _ = reply.send(result);
            }
            Command::Health { reply } => {
                let _ = reply.send(Ok(store.health()));
            }
            Command::Schema { reply } => {
                let _ = reply.send(store.schema());
            }
            Command::Close { durable, reply } => {
                let result = if durable { store.sync() } else { Ok(()) };
                // Close acknowledgement includes releasing the OS writer lock. Sending first and
                // dropping on function exit leaves a real race for immediate reopen/recovery.
                drop(store);
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

fn settle(store: &mut Store) -> Result<bool> {
    store.sync()?;
    Ok(store.flush()?.is_some())
}

fn compact(store: &mut Store, full: bool, control: &OperationControl) -> Result<CompactResult> {
    control.check("part compaction")?;
    let flushed = settle(store)?;
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
    let flushed = settle(store)?;
    control.check("bounded compaction")?;
    let parts_before = store.part_count();
    let compaction = store.compact_bounded_with_control(budget, control)?;
    Ok(BoundedCompactResult { flushed, parts_before, parts_after: store.part_count(), compaction })
}

fn verify(store: &mut Store, path: &Path, control: &OperationControl) -> Result<VerifyResult> {
    // Settling makes the report cover every operation accepted before this command, and actor
    // serialization prevents a new manifest from racing the chain walk.
    control.check("store verification")?;
    settle(store)?;
    control.check("store verification")?;
    let chain = integrity(
        "verify retained manifest chain",
        turndb::store::verify_chain_with_control(path, control),
    )?;
    let fold = integrity("verify fold frames", store.fold().scrub_with_control(control))?;
    let mut part_sections = 0usize;
    for part in store.parts() {
        control.check("store verification")?;
        part_sections += integrity(
            "verify immutable part sections",
            part.verify_sections_with_control(control),
        )?;
    }
    Ok(VerifyResult { chain, fold, parts: store.part_count(), part_sections })
}

fn integrity<T>(context: &'static str, result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if classify(&error) == ErrorClass::Internal {
            IntegrityError::new(context, error).into()
        } else {
            error
        }
    })
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
            queued.push(actor.submit(|reply| Command::Sync { reply }).unwrap());
        }
        let error = actor.submit(|reply| Command::Sync { reply }).unwrap_err();
        assert!(error.downcast_ref::<ActorFault>().is_some_and(|fault| {
            matches!(fault, ActorFault::Busy { capacity: 2 })
                && fault.to_string().contains("capacity 2")
        }));

        release_tx.send(()).unwrap();
        drop(queued);
        napi::tokio::runtime::Runtime::new().unwrap().block_on(actor.close(false)).unwrap();
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
}
