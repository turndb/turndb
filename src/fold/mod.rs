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
}

impl Default for FoldCfg {
    fn default() -> Self {
        FoldCfg {
            seg_max: SEG_MAX_DEFAULT,
            cache_bytes: 64 << 20,
            block_target: BLOCK_TARGET_DEFAULT,
            level: codec::LEVEL_DEFAULT,
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
    map: HashMap<(u32, u32), (u64, Arc<Vec<u8>>)>,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl BlockCache {
    fn new(budget: usize) -> BlockCache {
        BlockCache { budget: budget.max(1), bytes: 0, map: HashMap::new(), clock: 0, hits: 0, misses: 0 }
    }
    fn get(&mut self, k: (u32, u32)) -> Option<Arc<Vec<u8>>> {
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
    fn put(&mut self, k: (u32, u32), v: Arc<Vec<u8>>) {
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
        self.map.insert(k, (c, v));
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
    /// Append point: where the NEXT block frame will be written. Also the `block_off` every piece
    /// currently in the open buffer will carry, which is what lets `put` return a final `Loc` before
    /// the block it belongs to has been sealed.
    cur_off: u32,
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

        let good_tail = segment::scan_tail(&active_f, flen, has_dict)?;

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
            _lock: lock,
        })
    }

    /// Append `raw`, or return the existing location if this content is already folded.
    ///
    /// The returned `Loc` is final even though the block holding it may not be sealed yet: a block's
    /// offset is fixed when the block *opens* (it is the segment's append point), so a piece's address
    /// is known the moment it enters the buffer. Reads of an unsealed piece are served from the buffer.
    pub fn put(&mut self, raw: &[u8]) -> Result<Put> {
        if self.poisoned {
            bail!("fold is poisoned by an earlier failed write; reopen to recover by tail scan");
        }
        if raw.len() as u64 > u32::MAX as u64 {
            bail!("piece of {} bytes exceeds the u32 length cap; carve smaller", raw.len());
        }
        let hash = PieceHash::of(raw);
        if let Some(loc) = self.dedup.get(&hash) {
            return Ok(Put { hash, loc, deduped: true });
        }

        // A block must fit in a segment. If the open buffer plus this piece could not, seal what we
        // have and roll first, so a block never straddles a segment boundary.
        let projected = self.open_block.len() + raw.len() + block::BLOCK_OVERHEAD;
        if self.cur_off as u64 > SEG_HDR_LEN
            && self.cur_off as u64 + projected as u64 > self.cfg.seg_max as u64
        {
            self.seal_block()?;
            self.roll()?;
        }

        let loc = Loc {
            seg: self.active,
            block_off: self.cur_off,
            in_off: self.open_block.len() as u32,
            raw: raw.len() as u32,
        };
        self.open_block.extend_from_slice(raw);
        self.dedup.insert(hash, loc);

        if self.open_block.len() >= self.cfg.block_target {
            self.seal_block()?;
        }
        Ok(Put { hash, loc, deduped: false })
    }

    /// Read one piece back, exactly as it was written.
    pub fn read(&self, loc: Loc) -> Result<Vec<u8>> {
        let end = loc.in_off as u64 + loc.raw as u64;
        // still in the open buffer — not yet compressed or on disk
        if loc.seg == self.active && loc.block_off == self.cur_off {
            if end > self.open_block.len() as u64 {
                bail!("Loc names bytes past the open block");
            }
            return Ok(self.open_block[loc.in_off as usize..end as usize].to_vec());
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
            bail!("content hash mismatch at seg {} block {}: got {got}, expected {expect}", loc.seg, loc.block_off);
        }
        Ok(out)
    }

    /// The decompressed bytes of the block holding `loc`, through the cache.
    fn block_bytes(&self, loc: Loc) -> Result<Arc<Vec<u8>>> {
        let key = loc.block_key();
        if let Some(v) = self.cache.lock().unwrap().get(key) {
            return Ok(v);
        }
        let seg = loc.seg as usize;
        let f = self
            .readers
            .get(seg)
            .ok_or_else(|| anyhow::anyhow!("Loc names segment {} which does not exist", loc.seg))?;
        if (loc.block_off as u64) < SEG_HDR_LEN {
            bail!("Loc block offset {} is inside the segment header", loc.block_off);
        }
        let has_dict = self.headers[seg].has_dict();

        let mut hb = [0u8; block::BLOCK_HDR_LEN];
        f.read_exact_at(&mut hb, loc.block_off as u64)
            .with_context(|| format!("read block header at seg {} off {}", loc.seg, loc.block_off))?;
        let hdr = block::parse_hdr(&hb, has_dict)?;

        let span = hdr.frame_len() as usize;
        let mut buf = vec![0u8; span];
        f.read_exact_at(&mut buf, loc.block_off as u64)
            .with_context(|| format!("read block at seg {} off {}", loc.seg, loc.block_off))?;
        block::verify_frame_bytes(&buf, has_dict)?;

        let dict = self.dicts.get(&self.headers[seg].dict_id).cloned();
        let payload = &buf[block::BLOCK_HDR_LEN..block::BLOCK_HDR_LEN + hdr.stored as usize];
        let raw = codec::decode(hdr.codec, payload, hdr.raw, dict.as_deref().map(|v| &v[..]))?;
        if blake3::hash(&raw).as_bytes()[0..2] != hdr.r16 {
            bail!("decoded block does not match its content prefix (seg {} off {})", loc.seg, loc.block_off);
        }
        let arc = Arc::new(raw);
        self.cache.lock().unwrap().put(key, arc.clone());
        Ok(arc)
    }

    /// Compress and append the open block, if any. Idempotent when the buffer is empty.
    fn seal_block(&mut self) -> Result<()> {
        if self.open_block.is_empty() {
            return Ok(());
        }
        let dict = self.active_dict();
        let (tag, payload) = codec::encode(&self.open_block, dict.as_deref().map(|v| &v[..]), self.cfg.level)?;
        let n = block::encode(&mut self.scratch, tag, &self.open_block, &payload);
        if let Err(e) = self.active_f.write_all_at(&self.scratch[..n], self.cur_off as u64) {
            self.poisoned = true;
            return Err(anyhow::Error::new(e).context("fold block append failed; fold poisoned"));
        }
        self.cur_off += n as u32;
        self.open_block.clear();
        Ok(())
    }

    /// Seal the open block, make everything durable, and return the tail. Data before pointers: no part
    /// may name a `Loc` at or beyond a tail this has not returned.
    ///
    /// Call this at flush boundaries, not per record — every call seals the open block early, and short
    /// blocks compress worse.
    pub fn sync(&mut self) -> Result<FoldTail> {
        self.seal_block()?;
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
        debug_assert!(self.open_block.is_empty(), "seal before rolling — a block must not straddle segments");
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
