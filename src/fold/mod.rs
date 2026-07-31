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

use crate::readat::ReadAt;
use crate::types::PieceHash;
use anyhow::{bail, Context, Result};
use dedup::DedupTable;
use segment::{SegHeader, SEG_HDR_LEN, SEG_MAX_DEFAULT, SEG_MAX_LIMIT};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::File;
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

/// LRU of decompressed blocks, keyed by logical `block_id`, bounded by total decompressed bytes.
///
/// Recency lives in a second index (`order`: clock -> id) kept exactly in step with the map, so
/// eviction pops the coldest entry in O(log n). The old shape scanned the whole map per eviction
/// — O(n) per admit once warm, on a lock that every parallel scan partition contends for.
struct BlockCache {
    budget: usize,
    bytes: usize,
    map: HashMap<u32, (u64, Arc<Vec<u8>>)>,
    order: std::collections::BTreeMap<u64, u32>,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl BlockCache {
    fn new(budget: usize) -> BlockCache {
        BlockCache {
            budget: budget.max(1),
            bytes: 0,
            map: HashMap::new(),
            order: std::collections::BTreeMap::new(),
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }
    fn touch(&mut self, k: u32, old_clock: u64) -> u64 {
        self.clock += 1;
        self.order.remove(&old_clock);
        self.order.insert(self.clock, k);
        self.clock
    }
    fn get(&mut self, k: u32) -> Option<Arc<Vec<u8>>> {
        match self.map.get(&k).map(|e| (e.0, e.1.clone())) {
            Some((old, v)) => {
                let c = self.touch(k, old);
                self.map.get_mut(&k).expect("just found").0 = c;
                self.hits += 1;
                Some(v)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }
    /// Drop an entry outright — for a block whose bytes no longer exist.
    fn forget(&mut self, k: u32) {
        if let Some((clock, v)) = self.map.remove(&k) {
            self.bytes -= v.len();
            self.order.remove(&clock);
        }
    }
    fn put(&mut self, k: u32, v: Arc<Vec<u8>>) {
        let add = v.len();
        // always admit one block, however large, then evict coldest until back inside the budget
        while self.bytes + add > self.budget && !self.order.is_empty() {
            let (&coldest, &victim) = self.order.iter().next().expect("non-empty");
            self.order.remove(&coldest);
            if let Some((_, gone)) = self.map.remove(&victim) {
                self.bytes -= gone.len();
            }
        }
        self.clock += 1;
        let c = self.clock;
        self.bytes += add;
        self.order.insert(c, k);
        // Re-inserting a key DISPLACES a value whose bytes are still counted. Two readers racing the
        // same block is ordinary now that scan partitions run in parallel, and each race leaked a
        // block's worth of budget — enough repeats and a 64 MiB cache believes it is full while
        // holding one entry.
        if let Some((old_clock, old)) = self.map.insert(k, (c, v)) {
            self.bytes -= old.len();
            self.order.remove(&old_clock);
        }
    }
}

/// Cache effectiveness — the thing to watch if read latency regresses.
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
}

/// What [`Fold::scrub`] verified.
#[derive(Clone, Copy, Debug, Default)]
pub struct FoldScrub {
    pub segments: u32,
    pub blocks: usize,
    /// Frame bytes whose checksums verified.
    pub bytes: u64,
    /// Bytes past the active segment's last valid frame — uncommitted residue a crash left, which
    /// the next writer open truncates. Reported so "verified" never silently skips them.
    pub trailing_uncommitted: u64,
}

/// One segment as a read source hands it to [`Fold::open_read_from`]: its number, its bytes, and
/// its advisory sidecar if the source has one.
pub struct SegmentInput {
    pub seg: u32,
    pub reader: Arc<dyn ReadAt>,
    pub sidecar: Option<Vec<u8>>,
}

pub struct Fold {
    dir: PathBuf,
    cfg: FoldCfg,
    headers: Vec<SegHeader>,
    /// Read handles, one per segment — behind [`ReadAt`] so a sealed fold can later be read out of
    /// a pack extent or a remote range exactly as it is read out of a directory.
    readers: Vec<Arc<dyn ReadAt>>,
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
    /// `None` for a read-only fold with no directory behind it (a pack): there is nothing to
    /// append to, and `poisoned` refuses every write long before this would matter.
    active_f: Option<File>,
    /// Pieces gathered but not yet compressed and appended.
    open_block: Vec<u8>,
    dedup: DedupTable,
    cache: Mutex<BlockCache>,
    poisoned: bool,
    scratch: Vec<u8>,
    _lock: Option<File>,
    /// Blocks the MANIFEST declares erased, as inclusive `[lo, hi]` ranges — the authority for
    /// telling erasure from corruption.
    ///
    /// It has to be declared rather than detected. Punching zeroes a block's PAYLOAD and leaves its
    /// header intact so the frame chain stays walkable, so an erased block presents as a valid
    /// header over a checksum that will not verify — which is byte-for-byte what a torn write looks
    /// like. Nothing in the bytes distinguishes them; only the manifest does.
    ///
    /// Empty for a fold nobody declared anything about, which reads exactly as it did before.
    punched: Vec<(u32, u32)>,
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
            bail!(
                "seg_max {} exceeds the {} format bound (Loc.block_off is u32)",
                cfg.seg_max,
                SEG_MAX_LIMIT
            );
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
        crate::vfs::mkdir_all(dir).with_context(|| format!("create fold dir {}", dir.display()))?;
        let lock = acquire_writer_lock(dir)?;

        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = crate::vfs::unlink(&e.path());
                }
            }
        }

        let mut nums = list_segments(dir)?;
        nums.sort_unstable();
        for (i, n) in nums.iter().enumerate() {
            if *n != i as u32 {
                bail!("fold segments are not dense: expected seg {i}, found {n}");
            }
        }

        let mut headers: Vec<SegHeader> = Vec::with_capacity(nums.len());
        'scan: while !nums.is_empty() {
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
                        crate::vfs::unlink(&path)?;
                        segment::fsync_dir(dir)?;
                        // A torn create is not data — removing the last one may empty the fold
                        // entirely, which the fresh path below handles. (This used to refuse,
                        // which stranded a store that crashed during its very first segment's
                        // creation: found by the DST harness at crash point 3.)
                        nums.pop();
                        retry = true;
                        break;
                    }
                }
            }
            if !retry {
                break 'scan;
            }
        }

        if nums.is_empty() {
            // A virgin fold, or one whose only segment was a torn create, just removed. Either way
            // no durable fold bytes exist — and a committed tail must AGREE with that: a manifest
            // naming bytes an empty fold cannot serve means the fold lost durable data, and
            // creating a fresh fold underneath it would bury the loss instead of reporting it.
            if let Some(ct) = committed {
                if ct > (FoldTail { seg: 0, off: SEG_HDR_LEN as u32 }) {
                    bail!(
                        "committed fold tail (seg {}, off {}) but the fold holds no durable bytes \
                         — the fold lost durable data",
                        ct.seg,
                        ct.off
                    );
                }
            }
            let f = segment::create(dir, 0, [0u8; 32])?;
            return Ok(Fold {
                dir: dir.to_path_buf(),
                cfg,
                headers: vec![SegHeader { seg: 0, flags: 0, dict_id: [0u8; 32] }],
                readers: vec![Arc::new(segment::open_rw(dir, 0)?)],
                dicts: HashMap::new(),
                active: 0,
                cur_off: SEG_HDR_LEN as u32,
                active_f: Some(f),
                open_block: Vec::with_capacity(cfg.block_target * 2),
                dedup: DedupTable::new(),
                cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
                poisoned: false,
                scratch: Vec::new(),
                blockdir: Vec::new(),
                next_block: 0,
                inflight: HashMap::new(),
                pool: pipe::Pool::new(nthreads(cfg.compress_threads), cfg.level, None),
                _lock: Some(lock),
                punched: Vec::new(),
            });
        }

        let mut dicts: HashMap<[u8; 32], Arc<Vec<u8>>> = HashMap::new();
        for h in &headers {
            if !h.has_dict() || dicts.contains_key(&h.dict_id) {
                continue;
            }
            let name = format!("zdict-{}.zd", PieceHash(h.dict_id).to_hex());
            let bytes = std::fs::read(dir.join(&name)).with_context(|| {
                format!("segment {} names dictionary {name} but it is unreadable", h.seg)
            })?;
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
                    crate::vfs::unlink(&p)?;
                    headers.pop();
                    active -= 1;
                    active_f = segment::open_rw(dir, active)?;
                }
                segment::fsync_dir(dir)?;
                ct.off as u64
            }
        };

        let active_path = segment::seg_path(dir, active);
        crate::vfs::set_len(&active_f, &active_path, target)?;
        crate::vfs::sync_file(&active_f, &active_path)?;
        segment::fsync_dir(dir)?;

        let mut readers: Vec<Arc<dyn ReadAt>> = Vec::with_capacity(headers.len());
        for h in &headers {
            readers.push(Arc::new(segment::open_rw(dir, h.seg)?));
        }

        // Rebuild the block directory across every segment. Frames carry their ids, so this works
        // even though blocks were written in completion order rather than id order. Sealed
        // segments answer from their directory sidecars when they can — that is what keeps open
        // O(active segment) instead of O(store) — and are rescanned (and their sidecar
        // regenerated) when they cannot.
        let mut blockdir: Vec<Option<(u32, u32)>> = Vec::new();
        let mut next_block = 0u32;
        for (i, h) in headers.iter().enumerate() {
            let len = readers[i].len()?;
            let entries = if h.seg != active {
                match segment::read_dir_sidecar(dir, h.seg, len) {
                    Some((_, e)) => e,
                    None => {
                        let (tail, e) = segment::scan_tail(&readers[i], len, h.has_dict())?;
                        // Regenerate so the next open finds it. Best-effort: advisory data must
                        // never fail an open, only slow one down.
                        let _ = segment::write_dir_sidecar(dir, h.seg, tail as u32, &e);
                        e
                    }
                }
            } else {
                segment::scan_tail(&readers[i], len, h.has_dict())?.1
            };
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
            active_f: Some(active_f),
            open_block: Vec::with_capacity(cfg.block_target * 2),
            dedup: DedupTable::new(),
            cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
            poisoned: false,
            scratch: Vec::new(),
            blockdir,
            next_block,
            inflight: HashMap::new(),
            pool: pipe::Pool::new(nthreads(cfg.compress_threads), cfg.level, None),
            _lock: Some(lock),
            punched: Vec::new(),
        })
    }

    /// Open WITHOUT the writer lock, read-only.
    ///
    /// Takes no lock, truncates nothing, sweeps nothing — a reader must never mutate a store another
    /// process is writing. Safe concurrently with a live writer: segments are append-only and blocks
    /// are immutable once written, so a reader sees a prefix that only ever grows.
    /// Tell this fold which blocks the manifest declares erased, as inclusive `[lo, hi]` ranges.
    ///
    /// The caller owes the right manifest, and for a RETAINED snapshot that is the LIVE one, not the
    /// snapshot's own: punching commits a new manifest, so the retained copy predates the erasure
    /// and declares nothing. `punched` is cumulative in the live manifest, which is what makes this
    /// answerable at all.
    pub fn declare_punched(&mut self, ranges: &[(u32, u32)]) {
        self.punched = ranges.to_vec();
    }

    fn is_punched(&self, block_id: u32) -> bool {
        self.punched.iter().any(|&(lo, hi)| block_id >= lo && block_id <= hi)
    }

    pub fn open_read(dir: &Path, cfg: FoldCfg) -> Result<Fold> {
        let mut nums = list_segments(dir)?;
        if nums.is_empty() {
            bail!("no fold segments under {}", dir.display());
        }
        nums.sort_unstable();
        let mut segs = Vec::with_capacity(nums.len());
        for &n in &nums {
            segs.push(SegmentInput {
                seg: n,
                reader: Arc::new(segment::open_read(dir, n)?) as Arc<dyn ReadAt>,
                sidecar: std::fs::read(segment::dir_path(dir, n)).ok(),
            });
        }
        let mut dict_files = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("zdict-") && n.ends_with(".zd") {
                    dict_files.push(std::fs::read(e.path())?);
                }
            }
        }
        Fold::open_read_from(segs, dict_files, cfg, dir)
    }

    /// Open read-only from prepared inputs — the entry every SOURCE uses. A directory hands in
    /// files; a pack hands in extents; a remote store would hand in range readers. Nothing below
    /// this knows or cares which.
    ///
    /// `dict_files` are candidate trained dictionaries, identified by content hash — a segment
    /// naming a dictionary no candidate hashes to is refused. `label` names the source in errors
    /// and [`Fold::dir`], and is never touched as a path.
    pub fn open_read_from(
        mut segs: Vec<SegmentInput>,
        dict_files: Vec<Vec<u8>>,
        cfg: FoldCfg,
        label: &Path,
    ) -> Result<Fold> {
        if segs.is_empty() {
            bail!("no fold segments under {}", label.display());
        }
        segs.sort_by_key(|s| s.seg);
        // The same density rule the writer applies. A gap means a segment is missing, and reading
        // around it would silently serve a fold with a hole in its block space rather than refuse —
        // the writer refused and the reader did not, which is the worse half of an asymmetry.
        for (i, s) in segs.iter().enumerate() {
            if s.seg != i as u32 {
                bail!("fold segments are not dense: expected seg {i}, found {}", s.seg);
            }
        }
        let mut headers = Vec::with_capacity(segs.len());
        let mut readers: Vec<Arc<dyn ReadAt>> = Vec::with_capacity(segs.len());
        for s in &segs {
            let mut hb = [0u8; SEG_HDR_LEN as usize];
            s.reader.read_exact_at(&mut hb, 0)?;
            headers.push(SegHeader::decode(&hb, s.seg)?);
            readers.push(s.reader.clone());
        }
        let mut dicts: HashMap<[u8; 32], Arc<Vec<u8>>> = HashMap::new();
        for bytes in dict_files {
            let id: [u8; 32] = blake3::hash(&bytes).into();
            dicts.insert(id, Arc::new(bytes));
        }
        for h in &headers {
            if h.has_dict() && !dicts.contains_key(&h.dict_id) {
                bail!(
                    "segment {} names dictionary {} but no candidate hashes to it",
                    h.seg,
                    PieceHash(h.dict_id).to_hex()
                );
            }
        }
        // The directory is rebuilt from the ids the frames carry — the same sidecar-or-scan rule
        // the writer applies, except a reader NEVER writes a missing sidecar back: a reader must
        // not mutate a store it does not own, and a slower open is the whole cost.
        let last = segs.last().unwrap().seg;
        let mut blockdir: Vec<Option<(u32, u32)>> = Vec::new();
        let mut next_block = 0u32;
        for (i, h) in headers.iter().enumerate() {
            let len = readers[i].len()?;
            let entries = if h.seg != last {
                match segs[i]
                    .sidecar
                    .as_ref()
                    .and_then(|b| segment::parse_dir_sidecar(b, h.seg, len))
                {
                    Some((_, e)) => e,
                    None => segment::scan_tail(&readers[i], len, h.has_dict())?.1,
                }
            } else {
                segment::scan_tail(&readers[i], len, h.has_dict())?.1
            };
            for (id, off) in entries {
                if blockdir.len() <= id as usize {
                    blockdir.resize(id as usize + 1, None);
                }
                blockdir[id as usize] = Some((h.seg, off));
                next_block = next_block.max(id + 1);
            }
        }
        let cur_off = readers.last().unwrap().len()? as u32;
        Ok(Fold {
            dir: label.to_path_buf(),
            cfg,
            headers,
            readers,
            dicts,
            active: last,
            cur_off,
            active_f: None,
            open_block: Vec::new(),
            dedup: DedupTable::with_capacity(16),
            cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
            poisoned: true, // a read-only fold must refuse every append
            scratch: Vec::new(),
            blockdir,
            next_block,
            inflight: HashMap::new(),
            pool: pipe::Pool::new(1, cfg.level, None),
            _lock: None,
            punched: Vec::new(),
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
        debug_assert_eq!(
            hash,
            PieceHash::of(raw),
            "put_hashed given a digest that is not its content's"
        );
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

    /// Run `f` over the piece's bytes wherever they live — the open buffer, the compression
    /// pipeline, or a (cached) decompressed block — WITHOUT copying them anywhere first. Every
    /// read shape is a projection of this one.
    fn with_piece<T>(&self, loc: Loc, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        let end = loc.in_off as u64 + loc.raw as u64;
        // still gathering — not yet sealed
        if loc.block_id == self.next_block {
            if end > self.open_block.len() as u64 {
                bail!("Loc names bytes past the open block");
            }
            return Ok(f(&self.open_block[loc.in_off as usize..end as usize]));
        }
        // sealed but still in the pipeline — already uncompressed, so serve it directly
        if let Some(raw) = self.inflight.get(&loc.block_id) {
            if end > raw.len() as u64 {
                bail!("Loc names bytes past its block");
            }
            return Ok(f(&raw[loc.in_off as usize..end as usize]));
        }
        let blk = self.block_bytes(loc)?;
        if end > blk.len() as u64 {
            bail!(
                "Loc (in_off {}, raw {}) exceeds its block of {} bytes",
                loc.in_off,
                loc.raw,
                blk.len()
            );
        }
        Ok(f(&blk[loc.in_off as usize..end as usize]))
    }

    /// Read one piece back, exactly as it was written.
    pub fn read(&self, loc: Loc) -> Result<Vec<u8>> {
        self.with_piece(loc, |s| s.to_vec())
    }

    /// Read and confirm full content identity — the caller knows what hash it expects.
    pub fn read_verified(&self, loc: Loc, expect: PieceHash) -> Result<Vec<u8>> {
        self.with_piece(loc, |s| -> Result<Vec<u8>> {
            let got = PieceHash::of(s);
            if got != expect {
                bail!(
                    "content hash mismatch in block {} at +{}: got {got}, expected {expect}",
                    loc.block_id,
                    loc.in_off
                );
            }
            Ok(s.to_vec())
        })?
    }

    /// [`Fold::read_verified`], appending straight into `out` — reconstruction's read. A body is
    /// many pieces concatenated, and the intermediate Vec per piece was pure overhead: verify the
    /// bytes where they sit in the block, then copy ONCE, into their final place.
    pub fn read_verified_into(&self, loc: Loc, expect: PieceHash, out: &mut Vec<u8>) -> Result<()> {
        self.with_piece(loc, |s| -> Result<()> {
            let got = PieceHash::of(s);
            if got != expect {
                bail!(
                    "content hash mismatch in block {} at +{}: got {got}, expected {expect}",
                    loc.block_id,
                    loc.in_off
                );
            }
            out.extend_from_slice(s);
            Ok(())
        })?
    }

    /// The decompressed bytes of the block holding `loc`, through the cache.
    fn block_bytes(&self, loc: Loc) -> Result<Arc<Vec<u8>>> {
        if let Some(v) = self.cache.lock().unwrap().get(loc.block_id) {
            return Ok(v);
        }
        let (seg, off) =
            *self.blockdir.get(loc.block_id as usize).and_then(|e| e.as_ref()).ok_or_else(
                || anyhow::anyhow!("block {} is not in the fold's directory", loc.block_id),
            )?;
        let f = self.readers.get(seg as usize).ok_or_else(|| {
            anyhow::anyhow!("block {} names segment {seg} which does not exist", loc.block_id)
        })?;
        if (off as u64) < SEG_HDR_LEN {
            bail!("block {} offset {off} is inside the segment header", loc.block_id);
        }
        let has_dict = self.headers[seg as usize].has_dict();

        let mut hb = [0u8; block::BLOCK_HDR_LEN];
        f.read_exact_at(&mut hb, off as u64)
            .with_context(|| format!("read block header at seg {seg} off {off}"))?;
        // Naming an erasure is the difference between "your disk is failing" and "this content was
        // erased on purpose", and only one of those is true.
        //
        // The DECLARATION is checked first because it is the only thing that can be right. Punching
        // zeroes a block's payload and deliberately leaves its 16-byte header intact so the frame
        // chain stays walkable, so an erased block presents as a valid header over a payload whose
        // checksum will not verify — which is byte-for-byte indistinguishable from a torn write.
        // The zero-header test below cannot see that case, which is every block turndb itself
        // punches; it survives for a frame zeroed by something other than `punch_blocks`.
        if self.is_punched(loc.block_id) {
            bail!(
                "block {} was ERASED (its bytes were punched out of the fold); \
                 the manifest's punched list is authoritative for which",
                loc.block_id
            );
        }
        if hb.iter().all(|&b| b == 0) {
            bail!(
                "block {} was ERASED (its bytes were punched out of the fold); \
                 the manifest's punched list is authoritative for which",
                loc.block_id
            );
        }
        let hdr = block::parse_hdr(&hb, has_dict)?;
        if hdr.block_id != loc.block_id {
            bail!("directory sent block {} to a frame carrying id {}", loc.block_id, hdr.block_id);
        }

        let span = hdr.frame_len() as usize;
        let mut buf = vec![0u8; span];
        f.read_exact_at(&mut buf, off as u64)
            .with_context(|| format!("read block at seg {seg} off {off}"))?;
        block::verify_frame_bytes(&buf, has_dict)?;

        let dict = self.dicts.get(&self.headers[seg as usize].dict_id).cloned();
        let payload = &buf[block::BLOCK_HDR_LEN..block::BLOCK_HDR_LEN + hdr.stored as usize];
        let raw = codec::decode(hdr.codec, payload, hdr.raw, dict.as_deref().map(|v| &v[..]))?;
        // A free check that a decode produced the bytes this block was written for. It filters; it
        // never concludes identity. Unconditional — every block in this build carries r16. (The
        // ENCRYPTED segment flag is reserved and refused at open, so nothing reaching here was
        // ever ciphertext.)
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
        if self.cur_off as u64 > SEG_HDR_LEN
            && self.cur_off as u64 + n as u64 > self.cfg.seg_max as u64
        {
            self.roll()?;
        }
        let path = segment::seg_path(&self.dir, self.active);
        let f = self
            .active_f
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("read-only fold cannot append"))?;
        if let Err(e) = crate::vfs::write_all_at(f, &path, &self.scratch[..n], self.cur_off as u64)
        {
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
        let f =
            self.active_f.as_ref().ok_or_else(|| anyhow::anyhow!("read-only fold cannot sync"))?;
        crate::vfs::sync_file(f, &segment::seg_path(&self.dir, self.active))
            .context("fsync active fold segment")?;
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

    /// Physically destroy the bytes of blocks whose every piece is dead, WITHOUT moving anything.
    ///
    /// `FALLOC_FL_PUNCH_HOLE` deallocates a file's extents and reads them back as zeros: the file
    /// length, and therefore every offset in it, is unchanged — so no `Loc` above the fold is
    /// invalidated and no part needs rebuilding. That is what makes this the sub-refold erasure
    /// primitive: a refold rewrites the world to reclaim space, and this reclaims the same space
    /// in place, at the cost of leaving the block's frame header behind unpunched.
    ///
    /// `dead` names BLOCK IDS, and the caller owes the truth of that: a block is punchable only
    /// when no live record references any piece in it. The store computes that from the parts and
    /// records the result in the manifest before calling here — because a punched block read as
    /// merely corrupt is an ops fire drill, and only an authoritative record prevents it.
    ///
    /// Returns the ids actually punched. A block already punched, or whose frame will not parse,
    /// is skipped rather than errored — this is reclamation, and a block it cannot account for is
    /// a block it leaves alone.
    pub fn punch_blocks(&mut self, dead: &[u32]) -> Result<Vec<u32>> {
        let mut done = Vec::new();
        for &id in dead {
            let Some(Some((seg, off))) = self.blockdir.get(id as usize).copied() else { continue };
            let mut hb = [0u8; block::BLOCK_HDR_LEN];
            if self.readers[seg as usize].read_exact_at(&mut hb, off as u64).is_err() {
                continue;
            }
            let has_dict = self.headers[seg as usize].has_dict();
            let Ok(hdr) = block::parse_hdr(&hb, has_dict) else { continue };
            if hdr.block_id != id {
                bail!("directory sent block {id} to a frame carrying id {}", hdr.block_id);
            }
            let f = segment::open_rw(&self.dir, seg)?;
            // The PAYLOAD only — the header stays so the frame chain remains walkable. See
            // `segment::punch`.
            segment::punch(&f, off as u64 + block::BLOCK_HDR_LEN as u64, hdr.stored as u64)?;
            // The block is gone as content: drop it from the directory so no Loc can resolve
            // into erased bytes, exactly as a reopened store's scan would.
            self.blockdir[id as usize] = None;
            done.push(id);
        }
        // Punched blocks are no longer readable content: drop their cache entries so a warm
        // reader cannot keep serving what the disk no longer holds.
        {
            let mut c = self.cache.lock().unwrap();
            for id in &done {
                c.forget(*id);
            }
        }
        Ok(done)
    }

    /// Verify every block frame in every segment — the fold's half of a scrub.
    ///
    /// This is the whole-file read that sidecars removed from OPEN, done deliberately where it
    /// belongs: a scrub's cost is the point of a scrub. Covers what reconstruction-based deep
    /// verification cannot — blocks holding only retained or unreferenced pieces — and works
    /// identically over a directory or a pack, because it reads through [`ReadAt`].
    ///
    /// A frame that fails its checksum ends a segment's valid span; a sealed segment whose span
    /// ends before its file does is corruption and errors. The ACTIVE segment is allowed a
    /// trailing invalid region (uncommitted writes a crash abandoned), which is reported, not
    /// condemned.
    pub fn scrub(&self) -> Result<FoldScrub> {
        let mut report = FoldScrub::default();
        for (i, h) in self.headers.iter().enumerate() {
            let len = self.readers[i].len()?;
            let (end, entries) = segment::scan_tail(&self.readers[i], len, h.has_dict())?;
            report.segments += 1;
            report.blocks += entries.len();
            report.bytes += end.saturating_sub(segment::SEG_HDR_LEN);
            if end < len {
                if h.seg == self.active {
                    report.trailing_uncommitted = len - end;
                } else {
                    bail!(
                        "sealed segment {} holds valid frames only to byte {end} of {len} — corruption",
                        h.seg
                    );
                }
            }
        }
        Ok(report)
    }

    pub fn cache_stats(&self) -> CacheStats {
        let c = self.cache.lock().unwrap();
        CacheStats { hits: c.hits, misses: c.misses }
    }

    /// Every block id the directory knows — the universe a reachability sweep works against.
    pub fn block_ids(&self) -> Vec<u32> {
        self.blockdir.iter().enumerate().filter_map(|(id, e)| e.map(|_| id as u32)).collect()
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
        let flags = self.headers[self.active as usize].flags;
        let f =
            self.active_f.as_ref().ok_or_else(|| anyhow::anyhow!("read-only fold cannot roll"))?;
        crate::vfs::sync_file(f, &segment::seg_path(&self.dir, self.active))
            .context("fsync before roll")?;
        // The segment being sealed gets its directory sidecar now — the write that turns the next
        // open's full scan of it into a 2 KB read. Best-effort, AFTER the fsync above: advisory
        // data must never fail a roll, and a sidecar must never describe bytes less durable than
        // itself.
        let mut entries: Vec<(u32, u32)> = self
            .blockdir
            .iter()
            .enumerate()
            .filter_map(|(id, e)| match e {
                Some((s, o)) if *s == self.active => Some((id as u32, *o)),
                _ => None,
            })
            .collect();
        entries.sort_by_key(|&(_, off)| off);
        let _ = segment::write_dir_sidecar(&self.dir, self.active, self.cur_off, &entries);
        let next = self
            .active
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("segment number space exhausted"))?;
        let f = segment::create_flagged(&self.dir, next, [0u8; 32], flags)?;
        let reader = Arc::new(segment::open_rw(&self.dir, next)?);

        self.active_f = Some(f);
        self.headers.push(SegHeader { seg: next, flags, dict_id: [0u8; 32] });
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
        // wasm32 has no threads; the pool compresses inline there, so asking for more than one
        // worker would be meaningless.
        #[cfg(target_arch = "wasm32")]
        {
            1
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
        }
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

/// Exclusive writer lock held for the fold's whole lifetime — the single-writer invariant.
///
/// Enforced by the OS *on Unix*. On `wasm32-wasip1` `sys::lock_exclusive` succeeds unconditionally,
/// so this creates the file and gates nothing: there the invariant IS convention, and it is the
/// embedder's to keep.
fn acquire_writer_lock(dir: &Path) -> Result<File> {
    let path = dir.join("WRITER.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    if !crate::sys::lock_exclusive(&f).with_context(|| format!("locking {}", path.display()))? {
        bail!("fold at {} is already open by another writer", dir.display());
    }
    Ok(f)
}
