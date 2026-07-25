//! The compression pool: the expensive half of an append, moved off the write path.
//!
//! Measured on real corpora, zstd-19 was ~80% of ingest wall time and ran **inside** the serialized
//! writer, so the rest of the machine idled. It is also embarrassingly parallel — blocks are
//! independent — and the only thing that forced it to be serial was physical addressing, where a
//! block's offset depended on its predecessor's *compressed* size.
//!
//! With logical block ids that chain is cut. The writer assigns an id the moment a block seals
//! (routing and allocation — cheap), the pool compresses, and finished blocks are appended in
//! whatever order they complete.

use super::block::{CODEC_STORED, CODEC_ZSTD, CODEC_ZSTD_DICT};
use anyhow::Result;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;

/// A sealed block on its way to disk.
pub struct Job {
    pub block_id: u32,
    pub raw: Arc<Vec<u8>>,
}

/// A compressed block, ready to append.
pub struct Done {
    pub block_id: u32,
    pub codec: u8,
    pub raw: Arc<Vec<u8>>,
    pub payload: Vec<u8>,
}

pub struct Pool {
    tx: Option<SyncSender<Job>>,
    rx: Receiver<Done>,
    workers: Vec<std::thread::JoinHandle<()>>,
    /// Blocks sealed but not yet written — reads are served from these, and `drain_all` waits on them.
    pub outstanding: usize,
}

impl Pool {
    /// `dict` is the active segment's trained dictionary, if any. It is cloned into each worker
    /// because a dictionary belongs to a segment and outlives any single block.
    pub fn new(threads: usize, level: i32, dict: Option<Arc<Vec<u8>>>) -> Pool {
        let threads = threads.max(1);
        // Bounded both ways: a fast writer cannot run away from the pool, and a fast pool cannot
        // build an unbounded backlog of compressed blocks waiting to be written.
        let (jtx, jrx) = sync_channel::<Job>(threads * 2);
        let (dtx, drx) = sync_channel::<Done>(threads * 4);
        let jrx = Arc::new(std::sync::Mutex::new(jrx));
        let mut workers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let jrx = jrx.clone();
            let dtx = dtx.clone();
            let dict = dict.clone();
            workers.push(std::thread::spawn(move || loop {
                let job = {
                    let g = jrx.lock().unwrap();
                    match g.recv() {
                        Ok(j) => j,
                        Err(_) => return,
                    }
                };
                let (codec, payload) = match super::codec::encode(&job.raw, dict.as_deref().map(|v| &v[..]), level) {
                    Ok((c, p)) => (c, p.into_owned()),
                    // A compression failure must not be silently dropped: fall back to stored, which
                    // is always valid, and let the frame's own checks catch anything worse.
                    Err(_) => (CODEC_STORED, job.raw.as_ref().clone()),
                };
                debug_assert!(matches!(codec, CODEC_STORED | CODEC_ZSTD | CODEC_ZSTD_DICT));
                if dtx.send(Done { block_id: job.block_id, codec, raw: job.raw, payload }).is_err() {
                    return;
                }
            }));
        }
        Pool { tx: Some(jtx), rx: drx, workers, outstanding: 0 }
    }

    /// Hand a sealed block to the pool. Blocks when the pool is saturated (backpressure).
    pub fn submit(&mut self, block_id: u32, raw: Arc<Vec<u8>>) -> Result<()> {
        self.tx
            .as_ref()
            .expect("pool is shut down")
            .send(Job { block_id, raw })
            .map_err(|_| anyhow::anyhow!("compression pool died"))?;
        self.outstanding += 1;
        Ok(())
    }

    /// Take any finished blocks without waiting.
    pub fn try_take(&mut self) -> Vec<Done> {
        let mut out = Vec::new();
        while let Ok(d) = self.rx.try_recv() {
            self.outstanding -= 1;
            out.push(d);
        }
        out
    }

    /// Wait for every outstanding block. Used by `sync`, which cannot report a durable tail while
    /// blocks are still in flight.
    pub fn take_all(&mut self) -> Result<Vec<Done>> {
        let mut out = Vec::new();
        while self.outstanding > 0 {
            let d = self.rx.recv().map_err(|_| anyhow::anyhow!("compression pool died with work outstanding"))?;
            self.outstanding -= 1;
            out.push(d);
        }
        Ok(out)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.tx.take();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}
