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
use turndb::fold::FoldCfg;
use turndb::scan::{ScanPage, ScanRequest};
use turndb::store::{Batch, ContentSpans, ReadStore, Store};
use turndb::types::AttrValue;

const QUEUE_CAPACITY: usize = 64;
const OPEN: u8 = 0;
const CLOSING_OR_CLOSED: u8 = 1;

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
    Write { ops: Vec<WriteOp>, durable: bool, reply: oneshot::Sender<Result<()>> },
    Sync { reply: oneshot::Sender<Result<()>> },
    Flush { reply: oneshot::Sender<Result<bool>> },
    Scan { request: ScanRequest, reply: oneshot::Sender<Result<ScanPage>> },
    ReadContent { id: String, name: String, reply: oneshot::Sender<Result<Option<Vec<u8>>>> },
    Snapshot { reply: oneshot::Sender<Result<ReadStore>> },
    Close { durable: bool, reply: oneshot::Sender<Result<()>> },
}

struct Inner {
    tx: mpsc::SyncSender<Command>,
    state: AtomicU8,
}

#[derive(Clone)]
pub(crate) struct Actor {
    inner: Arc<Inner>,
}

impl Actor {
    pub fn open(path: &Path) -> Result<Actor> {
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let path = path.to_path_buf();
        std::thread::Builder::new()
            .name("turndb-store".into())
            .spawn(move || match Store::open(&path, FoldCfg::default()) {
                Ok(store) => {
                    let _ = ready_tx.send(Ok(()));
                    run(store, &path, rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("{error:#}")));
                }
            })
            .context("spawn TurnDB store thread")?;
        match ready_rx.recv().context("TurnDB store thread exited during open")? {
            Ok(()) => Ok(Actor { inner: Arc::new(Inner { tx, state: AtomicU8::new(OPEN) }) }),
            Err(message) => Err(anyhow!(message)).context("open TurnDB store"),
        }
    }

    fn submit<R: Send + 'static>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<R>>) -> Command,
    ) -> Result<oneshot::Receiver<Result<R>>> {
        if self.inner.state.load(Ordering::Acquire) != OPEN {
            return Err(anyhow!("TurnDB store is closed"));
        }
        let (reply, receive) = oneshot::channel();
        self.inner.tx.try_send(make(reply)).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => anyhow!(
                "TurnDB store command queue is full (capacity {QUEUE_CAPACITY}); retry after pending operations settle"
            ),
            mpsc::TrySendError::Disconnected(_) => anyhow!("TurnDB store thread has exited"),
        })?;
        Ok(receive)
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

    pub async fn read_content(&self, id: String, name: String) -> Result<Option<Vec<u8>>> {
        Self::receive(self.submit(|reply| Command::ReadContent { id, name, reply })?).await
    }

    pub async fn snapshot(&self) -> Result<ReadStore> {
        Self::receive(self.submit(|reply| Command::Snapshot { reply })?).await
    }

    pub async fn close(&self, durable: bool) -> Result<()> {
        if self
            .inner
            .state
            .compare_exchange(OPEN, CLOSING_OR_CLOSED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(anyhow!("TurnDB store is already closed"));
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
            return Err(anyhow!("TurnDB store thread has exited"));
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
            Command::Close { durable, reply } => {
                let result = if durable { store.sync() } else { Ok(()) };
                let _ = reply.send(result);
                break;
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
            let actor = Actor::open(&dir).unwrap();
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
}
