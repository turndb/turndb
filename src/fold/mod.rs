//! The fold — an append-only, content-addressed piece store with block compression.
//!
//! Identical bytes are stored once, forever, wherever they appear. Everything above the fold refers to
//! content by [`Loc`], and **nothing above the fold ever rewrites it**: merges reorganize references
//! and columns, never content. That is what decouples compaction cost from data volume.
//!
//! # Two units, deliberately separated
//!
//! A **piece** is the unit of identity and dedup. A **block** is the unit of compression and I/O.
//! Pieces accumulate in an open buffer; at the configured block target the buffer is compressed as one
//! block and appended. This captures the cross-piece redundancy that dominates trace data, and because
//! a record's pieces are captured together they land in a handful of blocks — so reconstructing a
//! record costs a few large decompressions instead of dozens of tiny ones. Measured on two real
//! corpora, this is both smaller on disk and faster per record than framing each piece alone.
//!
//! # The durability contract, in one sentence
//!
//! No part may be committed naming a [`Loc`] at or beyond a tail that [`Fold::sync`] has not returned.
//!
//! The fold is deliberately *not* a commit point. The WAL makes a record's carved pieces durable before
//! the fold is touched, so a crash between a `put` and a `sync` loses nothing replay cannot regenerate
//! — which is why `put` never fsyncs. It is also why `sync` must stay tied to flush boundaries rather
//! than to individual records: every `sync` seals the open block early, and blocks sealed short
//! compress worse.

pub mod block;
pub mod codec;
pub mod dedup;
pub mod pipe;
pub mod segment;

pub use block::{Loc, BLOCK_TARGET_DEFAULT, CODEC_STORED, CODEC_ZSTD, CODEC_ZSTD_DICT};
pub use segment::FoldTail;

use crate::types::PieceHash;
use anyhow::{bail, Context, Result};
use dedup::DedupTable;
use segment::{SegHeader, SEG_HDR_LEN, SEG_MAX_DEFAULT, SEG_MAX_LIMIT};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::collections::hash_map::Entry;

#[derive(Clone, Copy, Debug)]
pub struct FoldCfg {
    /// Roll threshold. Bounded by [`SEG_MAX_LIMIT`] because `Loc.block_off` is a u32.
    pub seg_max: u32,
    /// Decompressed-block cache budget in BYTES. Sized in bytes rather than blocks on purpose: a
    /// fixed block COUNT would make cache memory scale with the block-size dial, which is backwards.
    /// Blocking makes this load-bearing — a record's pieces span a few blocks, so the cache turns the
    /// rest of its reads into slices.
    pub cache_bytes: usize,
    /// Raw bytes gathered before a block seals. THE compression/read dial. Write-side only — a reader
    /// never needs to know it. Bigger blocks compress harder (more cross-piece redundancy in reach)
    /// and cost more to touch, since a read decompresses a whole block.
    pub block_target: usize,
    /// zstd level. Also write-side only. Costs ingest throughput, barely affects reads.
    pub level: i32,
    /// Compression threads. Compression is the expensive half of an append and blocks are
    /// independent, so it runs off the write path. 0 = one per core.
    pub compress_threads: usize,
}

impl Default for FoldCfg {
    fn default() -> Self {
        FoldCfg {
            seg_max: SEG_MAX_DEFAULT,
            cache_bytes: 64 << 20,
            block_target: BLOCK_TARGET_DEFAULT,
            level: codec::LEVEL_DEFAULT,
            compress_threads: 0,
        }
    }
}

/// Outcome of an append.
#[derive(Clone, Copy, Debug)]
pub struct Put {
    pub hash: PieceHash,
    pub loc: Loc,
    /// True when the content was already present and no bytes were written.
    pub deduped: bool,
}

/// LRU of decompressed blocks, keyed by `(seg, block_off)`, bounded by total decompressed bytes.
struct BlockCache {
    budget: usize,
    bytes: usize,
    map: HashMap<u32, (u64, Arc<Vec<u8>>)>,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl BlockCache {
    fn new(budget: usize) -> BlockCache {
        BlockCache { budget: budget.max(1), bytes: 0, map: HashMap::new(), clock: 0, hits: 0, misses: 0 }
    }
    fn get(&mut self, k: u32) -> Option<Arc<Vec<u8>>> {
        self.clock += 1;
        let c = self.clock;
        match self.map.get_mut(&k) {
            Some(e) => {
                e.0 = c;
                self.hits += 1;
                Some(e.1.clone())
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }
    fn put(&mut self, k: u32, v: Arc<Vec<u8>>) {
        let add = v.len();
        // always admit one block, however large, then evict coldest until back inside the budget
        while self.bytes + add > self.budget && !self.map.is_empty() {
            if let Some((&victim, _)) = self.map.iter().min_by_key(|(_, (t, _))| *t) {
                if let Some((_, gone)) = self.map.remove(&victim) {
                    self.bytes -= gone.len();
                }
            }
        }
        self.clock += 1;
        let c = self.clock;
        self.bytes += add;
        // Re-inserting a key DISPLACES a value whose bytes are still counted. Two readers racing the
        // same block is ordinary now that scan partitions run in parallel, and each race leaked a
        // block's worth of budget — enough repeats and a 64 MiB cache believes it is full while
        // holding one entry.
        if let Some((_, old)) = self.map.insert(k, (c, v)) {
            self.bytes -= old.len();
        }
    }
}

/// Cache effectiveness — the thing to watch if read latency regresses.
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
}

pub struct Fold {
    dir: PathBuf,
    cfg: FoldCfg,
    headers: Vec<SegHeader>,
    readers: Vec<Arc<File>>,
    dicts: HashMap<[u8; 32], Arc<Vec<u8>>>,
    active: u32,
    /// Physical append point in the active segment. No `Loc` refers to it — addressing is logical.
    cur_off: u32,
    /// block id -> (segment, offset). Rebuilt at open by scanning the frames, which carry their ids.
    blockdir: Vec<Option<(u32, u32)>>,
    /// The id the next sealed block will take.
    next_block: u32,
    /// Sealed but not yet written (still compressing, or waiting to be appended). Reads are served
    /// from here, so a piece is readable the instant `put` returns.
    inflight: HashMap<u32, Arc<Vec<u8>>>,
    pool: pipe::Pool,
    active_f: File,
    /// Pieces gathered but not yet compressed and appended.
    open_block: Vec<u8>,
    dedup: DedupTable,
    cache: Mutex<BlockCache>,
    poisoned: bool,
    scratch: Vec<u8>,
    _lock: File,
}

impl Fold {
    pub fn open(dir: &Path, cfg: FoldCfg) -> Result<Fold> {
        Fold::open_at(dir, cfg, None)
    }

    /// Open, recovering to `committed`: the tail some higher layer durably recorded.
    ///
    /// Two layers answer two different questions. The self-scan answers *"where does my block chain
    /// stop being valid?"*. The committed tail answers *"where did the store promise it stopped?"*. A
    /// committed tail **beyond** the last good block means the disk broke an fsync promise, and we
    /// refuse rather than serve a fold that silently lost durable bytes.
    pub fn open_at(dir: &Path, cfg: FoldCfg, committed: Option<FoldTail>) -> Result<Fold> {
        if (cfg.seg_max as u64) > SEG_MAX_LIMIT {
            bail!("seg_max {} exceeds the {} format bound (Loc.block_off is u32)", cfg.seg_max, SEG_MAX_LIMIT);
        }
        // A block is admitted into a FRESH segment however large it is — otherwise a block bigger than
        // seg_max could never be written at all. That admission is what makes block_target load-bearing
        // for overflow: the segment append point and `Loc.in_off` are both u32, so a block target past
        // 4 GiB wraps them and writes a block directory pointing at the wrong offset. In release that
        // is silent.
        if cfg.block_target == 0 {
            bail!("block_target must be non-zero");
        }
        if cfg.block_target as u64 > (u32::MAX as u64) / 2 {
            bail!(
                "block_target {} is too large; the segment append point and Loc.in_off are u32, so a \
                 block must stay well under 4 GiB",
                cfg.block_target
            );
        }
        // A deliberate NARROWING, not a bug fix: zstd itself accepts 0 (meaning "default") and
        // negative "fast" levels. Neither belongs in a store whose stated posture is compression-first,
        // and an invalid level otherwise surfaces at the first block write rather than at open — long
        // after the caller could do anything about it.
        if !(1..=22).contains(&cfg.level) {
            bail!("zstd level {} is outside the 1..=22 range this fold accepts", cfg.level);
        }
        std::fs::create_dir_all(dir).with_context(|| format!("create fold dir {}", dir.display()))?;
        let lock = acquire_writer_lock(dir)?;

        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }

        let mut nums = list_segments(dir)?;
        if nums.is_empty() {
            let f = segment::create(dir, 0, [0u8; 32])?;
            return Ok(Fold {
                dir: dir.to_path_buf(),
                cfg,
                headers: vec![SegHeader { seg: 0, dict_id: [0u8; 32] }],
                readers: vec![Arc::new(segment::open_rw(dir, 0)?)],
                dicts: HashMap::new(),
                active: 0,
                cur_off: SEG_HDR_LEN as u32,
                active_f: f,
                open_block: Vec::with_capacity(cfg.block_target * 2),
                dedup: DedupTable::new(),
                cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
                poisoned: false,
                scratch: Vec::new(),
                blockdir: Vec::new(),
                next_block: 0,
                inflight: HashMap::new(),
                pool: pipe::Pool::new(nthreads(cfg.compress_threads), cfg.level, None),
                _lock: lock,
            });
        }

        nums.sort_unstable();
        for (i, n) in nums.iter().enumerate() {
            if *n != i as u32 {
                bail!("fold segments are not dense: expected seg {i}, found {n}");
            }
        }

        let mut headers: Vec<SegHeader> = Vec::with_capacity(nums.len());
        loop {
            headers.clear();
            let last = *nums.last().unwrap();
            let mut retry = false;
            for &n in &nums {
                let path = segment::seg_path(dir, n);
                let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
                let len = f.metadata()?.len();
                let mut hb = [0u8; SEG_HDR_LEN as usize];
                let ok = len >= SEG_HDR_LEN && f.read_exact_at(&mut hb, 0).is_ok();
                match ok.then(|| SegHeader::decode(&hb, n)).transpose() {
                    Ok(Some(h)) => headers.push(h),
                    _ => {
                        if n != last {
                            bail!("segment {n} has an unreadable header — refusing (sealed history is corrupt)");
                        }
                        if len > SEG_HDR_LEN {
                            bail!("active segment {n} has a bad header but holds {len} bytes — refusing");
                        }
                        drop(f);
                        std::fs::remove_file(&path)?;
                        segment::fsync_dir(dir)?;
                        nums.pop();
                        if nums.is_empty() {
                            bail!("every fold segment was a torn create — refusing to guess");
                        }
                        retry = true;
                        break;
                    }
                }
            }
            if !retry {
                break;
            }
        }

        let mut dicts: HashMap<[u8; 32], Arc<Vec<u8>>> = HashMap::new();
        for h in &headers {
            if !h.has_dict() || dicts.contains_key(&h.dict_id) {
                continue;
            }
            let name = format!("zdict-{}.zd", PieceHash(h.dict_id).to_hex());
            let bytes = std::fs::read(dir.join(&name))
                .with_context(|| format!("segment {} names dictionary {name} but it is unreadable", h.seg))?;
            let got: [u8; 32] = blake3::hash(&bytes).into();
            if got != h.dict_id {
                bail!("dictionary {name} content hash does not match the id naming it");
            }
            dicts.insert(h.dict_id, Arc::new(bytes));
        }

        let mut active = *nums.last().unwrap();
        let mut active_f = segment::open_rw(dir, active)?;
        let flen = active_f.metadata()?.len();
        let has_dict = headers[active as usize].has_dict();

        let (good_tail, _) = segment::scan_tail(&active_f, flen, has_dict)?;

        let target = match committed {
            None => good_tail,
            Some(ct) => {
                if (ct.seg, ct.off as u64) > (active, good_tail) {
                    bail!(
                        "committed fold tail (seg {}, off {}) is beyond the last good block (seg {}, off {}) \
                         — the fold lost durable bytes",
                        ct.seg, ct.off, active, good_tail
                    );
                }
                while active > ct.seg {
                    let p = segment::seg_path(dir, active);
                    drop(active_f);
                    std::fs::remove_file(&p)?;
                    headers.pop();
                    active -= 1;
                    active_f = segment::open_rw(dir, active)?;
                }
                segment::fsync_dir(dir)?;
                ct.off as u64
            }
        };

        active_f.set_len(target)?;
        active_f.sync_all()?;
        segment::fsync_dir(dir)?;

        let mut readers = Vec::with_capacity(headers.len());
        for h in &headers {
            readers.push(Arc::new(segment::open_rw(dir, h.seg)?));
        }

        // Rebuild the block directory across every segment. Frames carry their ids, so this works
        // even though blocks were written in completion order rather than id order.
        let mut blockdir: Vec<Option<(u32, u32)>> = Vec::new();
        let mut next_block = 0u32;
        for (i, h) in headers.iter().enumerate() {
            let len = readers[i].metadata()?.len();
            let (_, entries) = segment::scan_tail(&readers[i], len, h.has_dict())?;
            for (id, off) in entries {
                if blockdir.len() <= id as usize {
                    blockdir.resize(id as usize + 1, None);
                }
                blockdir[id as usize] = Some((h.seg, off));
                next_block = next_block.max(id + 1);
            }
        }

        Ok(Fold {
            dir: dir.to_path_buf(),
            cfg,
            headers,
            readers,
            dicts,
            active,
            cur_off: target as u32,
            active_f,
            open_block: Vec::with_capacity(cfg.block_target * 2),
            dedup: DedupTable::new(),
            cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
            poisoned: false,
            scratch: Vec::new(),
            blockdir,
            next_block,
            inflight: HashMap::new(),
            pool: pipe::Pool::new(nthreads(cfg.compress_threads), cfg.level, None),
            _lock: lock,
        })
    }

    /// Open WITHOUT the writer lock, read-only.
    ///
    /// Takes no lock, truncates nothing, sweeps nothing — a reader must never mutate a store another
    /// process is writing. Safe concurrently with a live writer: segments are append-only and blocks
    /// are immutable once written, so a reader sees a prefix that only ever grows.
    pub fn open_read(dir: &Path, cfg: FoldCfg) -> Result<Fold> {
        let mut nums = list_segments(dir)?;
        if nums.is_empty() {
            bail!("no fold segments under {}", dir.display());
        }
        nums.sort_unstable();
        // The same density rule the writer applies. A gap means a segment is missing, and reading
        // around it would silently serve a fold with a hole in its block space rather than refuse —
        // the writer refused and the reader did not, which is the worse half of an asymmetry.
        for (i, n) in nums.iter().enumerate() {
            if *n != i as u32 {
                bail!("fold segments are not dense: expected seg {i}, found {n}");
            }
        }
        let mut headers = Vec::with_capacity(nums.len());
        let mut readers = Vec::with_capacity(nums.len());
        for &n in &nums {
            let f = segment::open_read(dir, n)?;
            let mut hb = [0u8; SEG_HDR_LEN as usize];
            f.read_exact_at(&mut hb, 0)?;
            headers.push(SegHeader::decode(&hb, n)?);
            readers.push(Arc::new(f));
        }
        let mut dicts: HashMap<[u8; 32], Arc<Vec<u8>>> = HashMap::new();
        for h in &headers {
            if !h.has_dict() || dicts.contains_key(&h.dict_id) {
                continue;
            }
            let name = format!("zdict-{}.zd", PieceHash(h.dict_id).to_hex());
            let bytes = std::fs::read(dir.join(&name))?;
            let got: [u8; 32] = blake3::hash(&bytes).into();
            if got != h.dict_id {
                bail!("dictionary {name} does not match the id naming it");
            }
            dicts.insert(h.dict_id, Arc::new(bytes));
        }
        // The directory is rebuilt from the ids the frames carry — the same scan the writer does.
        let mut blockdir: Vec<Option<(u32, u32)>> = Vec::new();
        let mut next_block = 0u32;
        for (i, h) in headers.iter().enumerate() {
            let len = readers[i].metadata()?.len();
            let (_, entries) = segment::scan_tail(&readers[i], len, h.has_dict())?;
            for (id, off) in entries {
                if blockdir.len() <= id as usize {
                    blockdir.resize(id as usize + 1, None);
                }
                blockdir[id as usize] = Some((h.seg, off));
                next_block = next_block.max(id + 1);
            }
        }
        let active = *nums.last().unwrap();
        let active_f = segment::open_read(dir, active)?;
        let cur_off = active_f.metadata()?.len() as u32;
        Ok(Fold {
            dir: dir.to_path_buf(),
            cfg,
            headers,
            readers,
            dicts,
            active,
            cur_off,
            active_f,
            open_block: Vec::new(),
            dedup: DedupTable::with_capacity(16),
            cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
            poisoned: true, // a read-only fold must refuse every append
            scratch: Vec::new(),
            blockdir,
            next_block,
            inflight: HashMap::new(),
            pool: pipe::Pool::new(1, cfg.level, None),
            _lock: File::open(dir)?,
        })
    }

    /// Append `raw`, or return the existing location if this content is already folded.
    ///
    /// The returned `Loc` is final even though the block holding it may not be sealed yet: a block's
    /// offset is fixed when the block *opens* (it is the segment's append point), so a piece's address
    /// is known the moment it enters the buffer. Reads of an unsealed piece are served from the buffer.
    pub fn put(&mut self, raw: &[u8]) -> Result<Put> {
        let hash = PieceHash::of(raw);
        self.put_hashed(raw, hash)
    }

    /// `put` for a caller that has ALREADY hashed the content.
    ///
    /// Hashing is the parallelizable half of an append and the serialized writer is the scarce
    /// resource, so an ingest pipeline hashes on worker threads and hands the digest in. The hash is
    /// trusted: it is the caller's assertion about bytes it holds, exactly as `put` trusts its own.
    pub fn put_hashed(&mut self, raw: &[u8], hash: PieceHash) -> Result<Put> {
        if self.poisoned {
            bail!("fold is poisoned by an earlier failed write; reopen to recover by tail scan");
        }
        if raw.len() as u64 > u32::MAX as u64 {
            bail!("piece of {} bytes exceeds the u32 length cap; carve smaller", raw.len());
        }
        debug_assert_eq!(hash, PieceHash::of(raw), "put_hashed given a digest that is not its content's");
        if let Some(loc) = self.dedup.get(&hash) {
            return Ok(Put { hash, loc, deduped: true });
        }

        let loc = Loc {
            block_id: self.next_block,
            in_off: self.open_block.len() as u32,
            raw: raw.len() as u32,
        };
        self.open_block.extend_from_slice(raw);
        self.dedup.insert(hash, loc);

        if self.open_block.len() >= self.cfg.block_target {
            self.seal_block()?;
        }
        // Write anything the pool has finished. Cheap, and it keeps the backlog shallow.
        self.write_ready(false)?;
        Ok(Put { hash, loc, deduped: false })
    }

    /// Read one piece back, exactly as it was written.
    pub fn read(&self, loc: Loc) -> Result<Vec<u8>> {
        let end = loc.in_off as u64 + loc.raw as u64;
        // still gathering — not yet sealed
        if loc.block_id == self.next_block {
            if end > self.open_block.len() as u64 {
                bail!("Loc names bytes past the open block");
            }
            return Ok(self.open_block[loc.in_off as usize..end as usize].to_vec());
        }
        // sealed but still in the pipeline — already uncompressed, so serve it directly
        if let Some(raw) = self.inflight.get(&loc.block_id) {
            if end > raw.len() as u64 {
                bail!("Loc names bytes past its block");
            }
            return Ok(raw[loc.in_off as usize..end as usize].to_vec());
        }
        let blk = self.block_bytes(loc)?;
        if end > blk.len() as u64 {
            bail!(
                "Loc (in_off {}, raw {}) exceeds its block of {} bytes",
                loc.in_off, loc.raw, blk.len()
            );
        }
        Ok(blk[loc.in_off as usize..end as usize].to_vec())
    }

    /// Read and confirm full content identity — the caller knows what hash it expects.
    pub fn read_verified(&self, loc: Loc, expect: PieceHash) -> Result<Vec<u8>> {
        let out = self.read(loc)?;
        let got = PieceHash::of(&out);
        if got != expect {
            bail!("content hash mismatch in block {} at +{}: got {got}, expected {expect}", loc.block_id, loc.in_off);
        }
        Ok(out)
    }

    /// The decompressed bytes of the block holding `loc`, through the cache.
    fn block_bytes(&self, loc: Loc) -> Result<Arc<Vec<u8>>> {
        if let Some(v) = self.cache.lock().unwrap().get(loc.block_id) {
            return Ok(v);
        }
        let (seg, off) = *self
            .blockdir
            .get(loc.block_id as usize)
            .and_then(|e| e.as_ref())
            .ok_or_else(|| anyhow::anyhow!("block {} is not in the fold's directory", loc.block_id))?;
        let f = self
            .readers
            .get(seg as usize)
            .ok_or_else(|| anyhow::anyhow!("block {} names segment {seg} which does not exist", loc.block_id))?;
        if (off as u64) < SEG_HDR_LEN {
            bail!("block {} offset {off} is inside the segment header", loc.block_id);
        }
        let has_dict = self.headers[seg as usize].has_dict();

        let mut hb = [0u8; block::BLOCK_HDR_LEN];
        f.read_exact_at(&mut hb, off as u64)
            .with_context(|| format!("read block header at seg {seg} off {off}"))?;
        let hdr = block::parse_hdr(&hb, has_dict)?;
        if hdr.block_id != loc.block_id {
            bail!("directory sent block {} to a frame carrying id {}", loc.block_id, hdr.block_id);
        }

        let span = hdr.frame_len() as usize;
        let mut buf = vec![0u8; span];
        f.read_exact_at(&mut buf, off as u64).with_context(|| format!("read block at seg {seg} off {off}"))?;
        block::verify_frame_bytes(&buf, has_dict)?;

        let dict = self.dicts.get(&self.headers[seg as usize].dict_id).cloned();
        let payload = &buf[block::BLOCK_HDR_LEN..block::BLOCK_HDR_LEN + hdr.stored as usize];
        let raw = codec::decode(hdr.codec, payload, hdr.raw, dict.as_deref().map(|v| &v[..]))?;
        if blake3::hash(&raw).as_bytes()[0..2] != hdr.r16 {
            bail!("decoded block does not match its content prefix (block {})", loc.block_id);
        }
        let arc = Arc::new(raw);
        self.cache.lock().unwrap().put(loc.block_id, arc.clone());
        Ok(arc)
    }

    /// Seal the open block: assign its id and hand it to the compression pool. Does no compression
    /// and no I/O — that is the whole point. Idempotent when the buffer is empty.
    fn seal_block(&mut self) -> Result<()> {
        if self.open_block.is_empty() {
            return Ok(());
        }
        let id = self.next_block;
        self.next_block = self
            .next_block
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("block id space exhausted"))?;
        let raw = Arc::new(std::mem::replace(
            &mut self.open_block,
            Vec::with_capacity(self.cfg.block_target * 2),
        ));
        self.inflight.insert(id, raw.clone());
        self.pool.submit(id, raw)?;
        Ok(())
    }

    /// Append every block the pool has finished. With `wait`, blocks until the pool is empty.
    fn write_ready(&mut self, wait: bool) -> Result<()> {
        let done = if wait { self.pool.take_all()? } else { self.pool.try_take() };
        for d in done {
            self.write_block(d)?;
        }
        Ok(())
    }

    fn write_block(&mut self, d: pipe::Done) -> Result<()> {
        let n = block::encode(&mut self.scratch, d.block_id, d.codec, &d.raw, &d.payload);
        // Roll if this block would not fit. Blocks are self-contained, so one never straddles.
        if self.cur_off as u64 > SEG_HDR_LEN && self.cur_off as u64 + n as u64 > self.cfg.seg_max as u64 {
            self.roll()?;
        }
        if let Err(e) = self.active_f.write_all_at(&self.scratch[..n], self.cur_off as u64) {
            self.poisoned = true;
            return Err(anyhow::Error::new(e).context("fold block append failed; fold poisoned"));
        }
        if self.blockdir.len() <= d.block_id as usize {
            self.blockdir.resize(d.block_id as usize + 1, None);
        }
        self.blockdir[d.block_id as usize] = Some((self.active, self.cur_off));
        self.cur_off += n as u32;
        match self.inflight.entry(d.block_id) {
            Entry::Occupied(e) => {
                e.remove();
            }
            Entry::Vacant(_) => {}
        }
        Ok(())
    }

    /// Seal the open block, make everything durable, and return the tail. Data before pointers: no part
    /// may name a `Loc` at or beyond a tail this has not returned.
    ///
    /// Call this at flush boundaries, not per record — every call seals the open block early, and short
    /// blocks compress worse.
    pub fn sync(&mut self) -> Result<FoldTail> {
        self.seal_block()?;
        // Every block must be compressed AND written before a tail can be reported durable.
        self.write_ready(true)?;
        self.active_f.sync_all().context("fsync active fold segment")?;
        Ok(self.tail())
    }

    /// The current append point. Pieces in the open buffer live AT this offset and are not yet durable.
    pub fn tail(&self) -> FoldTail {
        FoldTail { seg: self.active, off: self.cur_off }
    }

    /// Resolve content to a location through the unsealed dedup window.
    ///
    /// Only covers pieces not yet sealed into a part — sealed pieces are found through the parts' own
    /// dictionaries, which is why this index needs no on-disk form. A miss is never wrong, only slower.
    pub fn lookup(&self, hash: PieceHash) -> Option<Loc> {
        self.dedup.get(&hash)
    }

    /// Record a location the caller resolved elsewhere (a Tier-1 part lookup), so that further
    /// references to the same content in this flush window are answered from memory.
    ///
    /// This only ever populates a cache with a mapping that is already true on disk. It cannot create
    /// content, and a wrong entry here would be indistinguishable from a wrong entry made by `put`.
    pub fn note(&mut self, hash: PieceHash, loc: Loc) {
        self.dedup.insert(hash, loc);
    }

    pub fn window_len(&self) -> usize {
        self.dedup.len()
    }

    /// Release the dedup window — the pieces it covers are sealed into a part, so resident memory
    /// tracks the flush interval rather than the store.
    pub fn seal_window(&mut self) {
        self.dedup.clear();
    }

    pub fn cache_stats(&self) -> CacheStats {
        let c = self.cache.lock().unwrap();
        CacheStats { hits: c.hits, misses: c.misses }
    }

    pub fn segment_count(&self) -> u32 {
        self.headers.len() as u32
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Total bytes across all segment files. Excludes anything still in the open buffer.
    pub fn disk_bytes(&self) -> u64 {
        (0..self.headers.len() as u32)
            .filter_map(|n| std::fs::metadata(segment::seg_path(&self.dir, n)).ok())
            .map(|m| m.len())
            .sum()
    }

    /// The active segment's dictionary. Currently unused because the compression pool is handed its
    /// dictionary at construction; when trained dictionaries land, the pool must be rebuilt on roll.
    #[allow(dead_code)]
    fn active_dict(&self) -> Option<Arc<Vec<u8>>> {
        let id = self.headers[self.active as usize].dict_id;
        if id == [0u8; 32] {
            None
        } else {
            self.dicts.get(&id).cloned()
        }
    }

    /// Roll to a new segment. Every physical step happens before any logical state moves, so a failure
    /// leaves nothing changed and the caller's retry re-enters cleanly. (An earlier generation of this
    /// engine advanced the offset first; a roll-time ENOSPC then left a zero offset over the *old*
    /// segment handle and the next write silently corrupted the fold.)
    fn roll(&mut self) -> Result<()> {
        self.active_f.sync_all().context("fsync before roll")?;
        let next = self
            .active
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("segment number space exhausted"))?;
        let f = segment::create(&self.dir, next, [0u8; 32])?;
        let reader = Arc::new(segment::open_rw(&self.dir, next)?);

        self.active_f = f;
        self.headers.push(SegHeader { seg: next, dict_id: [0u8; 32] });
        self.readers.push(reader);
        self.active = next;
        self.cur_off = SEG_HDR_LEN as u32;
        Ok(())
    }
}

/// Compression threads: 0 means one per core.
fn nthreads(cfg: usize) -> usize {
    if cfg > 0 {
        cfg
    } else {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    }
}

fn list_segments(dir: &Path) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        if let Some(n) = segment::parse_seg_name(&e.file_name().to_string_lossy()) {
            out.push(n);
        }
    }
    Ok(out)
}

/// Exclusive writer lock held for the fold's whole lifetime — the single-writer invariant, enforced by
/// the OS rather than by convention.
fn acquire_writer_lock(dir: &Path) -> Result<File> {
    let path = dir.join("WRITER.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!("fold at {} is already open by another writer", dir.display());
    }
    Ok(f)
}
