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
//! The fold is deliberately not publication authority. The WAL makes a record's decomposed pieces durable before
//! the fold is touched, so a crash between a `put` and a `sync` loses nothing replay cannot regenerate
//! — which is why `put` never fsyncs. It is also why `sync` must stay tied to flush boundaries rather
//! than to individual records: every `sync` seals the open block early, and blocks sealed short
//! compress worse.

pub mod block;
pub mod codec;
pub mod dedup;
pub mod pipe;
pub mod segment;
mod segstore;

pub use block::{Loc, BLOCK_TARGET_DEFAULT, CODEC_STORED, CODEC_ZSTD, CODEC_ZSTD_DICT};
pub use segment::FoldTail;

use crate::readat::{ReadAt, Slice};
use crate::types::PieceHash;
use anyhow::{bail, Context, Result};
use dedup::DedupTable;
use segment::{SegHeader, SEG_HDR_LEN, SEG_MAX_DEFAULT, SEG_MAX_LIMIT};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const SEGMENT_MAX_DEFAULT: u32 = SEG_MAX_DEFAULT;
pub const SEGMENT_MAX_LIMIT: u64 = SEG_MAX_LIMIT;
pub const BLOCK_TARGET_MAX: u64 = (u32::MAX as u64) / 2;
/// Largest stored or raw block payload that can begin after the segment header and still leave its
/// complete frame end representable by the persisted u32 segment tail.
pub const BLOCK_PAYLOAD_MAX: u64 = SEG_MAX_LIMIT - SEG_HDR_LEN - block::BLOCK_OVERHEAD as u64;
/// Runtime ceiling for native compression workers. This is an admission policy, not an on-disk
/// format limit; it keeps caller-controlled channel sizing and worker creation bounded.
pub const COMPRESSION_THREADS_MAX: usize = 256;
/// Candidate dictionary bytes admitted during open. Zstd dictionaries are normally measured in
/// KiB; this generous ceiling prevents an unrelated sparse file with a dictionary-shaped name from
/// becoming an unbounded allocation.
pub const MAX_DICTIONARY_BYTES: u64 = 64 << 20;

pub(crate) fn validate_cfg(cfg: FoldCfg) -> Result<()> {
    if (cfg.seg_max as u64) > SEG_MAX_LIMIT {
        bail!(
            "seg_max {} exceeds the {} format bound (Loc.block_off is u32)",
            cfg.seg_max,
            SEG_MAX_LIMIT
        );
    }
    if cfg.block_target == 0 {
        bail!("block_target must be non-zero");
    }
    if cfg.block_target as u64 > BLOCK_TARGET_MAX {
        bail!(
            "block_target {} is too large; the segment append point and Loc.in_off are u32, so a \
             block must stay well under 4 GiB",
            cfg.block_target
        );
    }
    if !(1..=22).contains(&cfg.level) {
        bail!("zstd level {} is outside the 1..=22 range this fold accepts", cfg.level);
    }
    if cfg.compress_threads > COMPRESSION_THREADS_MAX {
        bail!(
            "compress_threads {} exceeds the runtime limit of {}",
            cfg.compress_threads,
            COMPRESSION_THREADS_MAX
        );
    }
    Ok(())
}

fn validate_piece_len(len: u64) -> Result<()> {
    if len == 0 {
        bail!("a fold piece must contain at least one byte");
    }
    if len > BLOCK_PAYLOAD_MAX {
        bail!("piece of {len} bytes cannot fit in a current-format fold segment; carve smaller");
    }
    Ok(())
}

fn validate_punched_ranges(
    punched: &[(u32, u32)],
    blockdir: &[Option<(u32, u32)>],
    allow_future: bool,
) -> Result<()> {
    for &(lo, hi) in punched {
        let start = usize::try_from(lo).context("punched block id exceeds address space")?;
        if allow_future && start >= blockdir.len() {
            continue;
        }
        let claimed_end = usize::try_from(u64::from(hi) + 1)
            .context("punched block range exceeds address space")?;
        if !allow_future && claimed_end > blockdir.len() {
            bail!("punched block range {lo}..={hi} names a block that does not exist");
        }
        let end = claimed_end.min(blockdir.len());
        let Some(entries) = blockdir.get(start..end) else {
            bail!("punched block range {lo}..={hi} names a block that does not exist");
        };
        if entries.iter().any(Option::is_none) {
            bail!("punched block range {lo}..={hi} crosses a block id that does not exist");
        }
    }
    Ok(())
}

fn read_bounded_candidate(path: &Path, max: u64) -> Result<Vec<u8>> {
    let file = crate::vfs::open_read(path)?;
    let len = file.metadata()?.len();
    if len > max {
        bail!("candidate file {} is {len} bytes, exceeding the {max}-byte limit", path.display());
    }
    let capacity =
        usize::try_from(len).context("candidate file length does not fit this platform")?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).context("reserve candidate file buffer")?;
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        bail!("candidate file {} grew past the {max}-byte limit while reading", path.display());
    }
    Ok(bytes)
}

/// Another writer currently owns this fold's OS lock.
///
/// This is a typed operational condition rather than prose so bindings can expose stable contention
/// handling without matching an error message. It says nothing about WASI, where the platform lacks
/// an advisory lock and the embedder owns exclusion.
#[derive(Debug)]
pub struct WriterLocked {
    pub path: PathBuf,
}

impl std::fmt::Display for WriterLocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The path speaks for itself — a store file or a fold directory — so the message must
        // not name either layout.
        write!(f, "{} is already open by another writer", self.path.display())
    }
}

impl std::error::Error for WriterLocked {}

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
    /// independent, so it runs off the write path. 0 = one per core; explicit values are bounded
    /// by [`COMPRESSION_THREADS_MAX`].
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
    pub bytes: usize,
    pub budget: usize,
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
    /// Read handles, one per segment — behind [`ReadAt`] so a closed fold can be read from a
    /// container-member extent or a remote range exactly as it is read from staging files.
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
    /// Where the write side lives: segment files in a directory, members of a container, or a
    /// read-only refusal — the fold owns the arithmetic, the store owns where bytes land.
    segs: Box<dyn segstore::SegmentStore>,
    /// Pieces gathered but not yet compressed and appended.
    open_block: Vec<u8>,
    dedup: DedupTable,
    cache: Mutex<BlockCache>,
    poisoned: bool,
    /// Distinguishes an actual failed writer pipeline from `poisoned`'s read-only sentinel.
    /// Store acceptance may inspect a read-only Fold, but it must never reuse dedup state after a
    /// failed writer append.
    write_failed: bool,
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
    read_limits: crate::read_limits::ReadLimits,
}

impl Fold {
    /// Open or create a file-backed fold under `dir`: one segment file per segment, sidecars
    /// beside them, and a lock file. This is the staging form the part builders, tests, and
    /// benchmarks use; a store's fold lives in its container and is opened through
    /// [`Fold::open_container_writer`]. The names this creates under `dir` are not store protocol
    /// state and the debris inventory does not recognize them.
    pub fn open(dir: &Path, cfg: FoldCfg) -> Result<Fold> {
        Fold::open_with_limits(dir, cfg, crate::read_limits::ReadLimits::default())
    }

    /// Open a writer with explicit frame and persistent object-count admission.
    pub fn open_with_limits(
        dir: &Path,
        cfg: FoldCfg,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Fold> {
        // No committed tail means no manifest in hand — and therefore no punched declaration.
        Fold::open_at_with_limits(dir, cfg, None, &[], read_limits)
    }

    /// Open, recovering to `committed`: the tail some higher layer durably recorded.
    ///
    /// Two layers answer two different questions. The self-scan answers *"where does my block chain
    /// stop being valid?"*. The committed tail answers *"where did the store promise it stopped?"*. A
    /// committed tail **beyond** the last good block means the disk broke an fsync promise, and we
    /// refuse rather than serve a fold that silently lost durable bytes.
    ///
    /// `punched` is the same manifest's declared-erased block ranges. The tail scan needs it at
    /// open, not merely after: a crash mid-punch can leave a DECLARED block's payload partially
    /// zeroed, and only the declaration authorizes stepping over that frame instead of reading the
    /// committed tail as beyond the last good block.
    pub fn open_at(
        dir: &Path,
        cfg: FoldCfg,
        committed: Option<FoldTail>,
        punched: &[(u32, u32)],
    ) -> Result<Fold> {
        Fold::open_at_with_limits(
            dir,
            cfg,
            committed,
            punched,
            crate::read_limits::ReadLimits::default(),
        )
    }

    /// Open and recover a writer under explicit frame and object-count admission.
    pub fn open_at_with_limits(
        dir: &Path,
        cfg: FoldCfg,
        committed: Option<FoldTail>,
        punched: &[(u32, u32)],
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Fold> {
        let read_limits = read_limits.validate()?;
        validate_cfg(cfg)?;
        // A block is admitted into a FRESH segment however large it is — otherwise a block bigger than
        // seg_max could never be written at all. That admission is what makes block_target load-bearing
        // for overflow: the segment append point and `Loc.in_off` are both u32, so a block target past
        // 4 GiB wraps them and writes a block directory pointing at the wrong offset. In release that
        // is silent.
        // A deliberate NARROWING, not a bug fix: zstd itself accepts 0 (meaning "default") and
        // negative "fast" levels. Neither belongs in a store whose stated posture is compression-first,
        // and an invalid level otherwise surfaces at the first block write rather than at open — long
        // after the caller could do anything about it.
        if committed.is_some() && !dir.try_exists()? {
            bail!(
                "committed fold tail names an absent fold directory — the fold lost durable data"
            );
        }
        crate::vfs::mkdir_all(dir).with_context(|| format!("create fold dir {}", dir.display()))?;
        let (entries, has_lock, has_segment) = fold_directory_shape(dir, read_limits)?;
        if !has_segment {
            if let Some(ct) = committed {
                bail!(
                    "committed fold tail (seg {}, off {}) but the fold holds no segment — the fold lost durable data",
                    ct.seg,
                    ct.off
                );
            }
        }
        let additions = u64::from(!has_lock) + u64::from(!has_segment);
        read_limits.admit_directory_entries(
            "fold directory during writer open",
            entries.saturating_add(additions),
        )?;

        // Establish the current physical identity before creating the lock or deleting debris.
        // This is deliberately repeated under the lock below: the first pass makes refusal
        // mutation-free, while the second owns any crash-prefix repair without a writer race.
        let preflight_headers = validate_fold_segment_identities(dir, read_limits)?;
        validate_fold_dictionary_dependencies(dir, &preflight_headers)?;
        if let Some(committed) = committed {
            validate_committed_fold_prefix(
                dir,
                &preflight_headers,
                committed,
                punched,
                read_limits,
            )?;
        }
        let lock = acquire_writer_lock(dir)?;

        let rd = std::fs::read_dir(dir)
            .with_context(|| format!("read fold directory {} for cleanup", dir.display()))?;
        let mut visited = 0u64;
        let mut removed_staging = false;
        for e in rd {
            visited = visited.saturating_add(1);
            read_limits.admit_directory_entries("fold directory", visited)?;
            let e = e?;
            let name = e.file_name();
            if segment::is_birth_staging_name(&name) {
                crate::vfs::unlink(&e.path())?;
                removed_staging = true;
                continue;
            }
            if let Some(seg) = name.to_str().and_then(segment::parse_dir_tmp_name) {
                if segment::seg_path(dir, seg).is_file() {
                    crate::vfs::unlink(&e.path()).with_context(|| {
                        format!("remove Fold sidecar staging file {}", e.path().display())
                    })?;
                    removed_staging = true;
                }
            }
        }
        if removed_staging {
            segment::fsync_dir(dir).with_context(|| {
                format!("sync Fold directory {} after removing staging debris", dir.display())
            })?;
        }

        let mut nums = list_segments_with_limits(dir, read_limits)?;
        nums.sort_unstable();
        for (i, n) in nums.iter().enumerate() {
            let want = i as u32;
            if *n != want {
                bail!("fold segments are not dense: expected seg {want}, found {n}");
            }
        }

        let mut headers: Vec<SegHeader> = Vec::with_capacity(nums.len());
        let last = nums.last().copied();
        for &n in &nums {
            let path = segment::seg_path(dir, n);
            let f =
                crate::vfs::open_read(&path).with_context(|| format!("open {}", path.display()))?;
            let len = f.metadata()?.len();
            if len < SEG_HDR_LEN {
                bail!(
                    "segment {n} has a truncated current-format header ({len} bytes) — refusing without mutation"
                );
            }
            let mut hb = [0u8; SEG_HDR_LEN as usize];
            f.read_exact_at(&mut hb, 0).with_context(|| format!("read segment {n} header"))?;
            let h = SegHeader::decode(&hb, n).with_context(|| {
                if Some(n) == last {
                    format!("active segment {n} has an invalid current-format header")
                } else {
                    format!("non-active segment {n} has an invalid current-format header")
                }
            })?;
            headers.push(h);
        }

        if nums.is_empty() {
            if !punched.is_empty() {
                bail!("a fold with no blocks cannot carry punched block declarations");
            }
            debug_assert!(committed.is_none(), "committed empty folds refuse before mutation");
            let f = segment::create(dir, 0, [0u8; 32])?;
            return Ok(Fold {
                dir: dir.to_path_buf(),
                cfg,
                headers: vec![SegHeader { seg: 0, flags: 0, dict_id: [0u8; 32] }],
                readers: vec![Arc::new(segment::open_rw(dir, 0)?)],
                dicts: HashMap::new(),
                active: 0,
                cur_off: SEG_HDR_LEN as u32,
                segs: Box::new(segstore::DirSegments::new(dir.to_path_buf(), 0, f, read_limits)),
                open_block: Vec::with_capacity(cfg.block_target * 2),
                dedup: DedupTable::new(),
                cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
                poisoned: false,
                write_failed: false,
                scratch: Vec::new(),
                blockdir: Vec::new(),
                next_block: 0,
                inflight: HashMap::new(),
                pool: pipe::Pool::new(nthreads(cfg.compress_threads), cfg.level, None),
                _lock: Some(lock),
                punched: punched.to_vec(),
                read_limits,
            });
        }

        let mut dicts: HashMap<[u8; 32], Arc<Vec<u8>>> = HashMap::new();
        for h in &headers {
            if !h.has_dict() || dicts.contains_key(&h.dict_id) {
                continue;
            }
            let name = format!("zdict-{}.zd", PieceHash(h.dict_id).to_hex());
            let bytes = read_bounded_candidate(&dir.join(&name), MAX_DICTIONARY_BYTES)
                .with_context(|| {
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
        let target = match committed {
            None => {
                let flen = active_f.metadata()?.len();
                let has_dict = headers[active as usize].has_dict();
                segment::scan_tail_with_limits(&active_f, flen, has_dict, punched, read_limits)?.0
            }
            Some(ct) => {
                if ct.seg > active || u64::from(ct.off) < SEG_HDR_LEN {
                    bail!(
                        "committed fold tail (seg {}, off {}) is outside the current-format segment domain",
                        ct.seg,
                        ct.off
                    );
                }
                // Prove the complete surviving prefix before unlinking a later crash segment or
                // truncating the target's uncommitted suffix. Lexicographic comparison with the
                // newest physical segment is insufficient: a later segment can exist while the
                // committed target segment has itself lost bytes. Extending that loss with zeros
                // would destroy the evidence and manufacture authority.
                for seg in 0..=ct.seg {
                    let file = segment::open_read(dir, seg)?;
                    let physical = file.metadata()?.len();
                    let expected = if seg == ct.seg { u64::from(ct.off) } else { physical };
                    if expected > physical {
                        bail!(
                            "committed fold tail (seg {}, off {}) exceeds segment {seg}'s {physical} bytes — the fold lost durable bytes",
                            ct.seg,
                            ct.off
                        );
                    }
                    let (good, _) = segment::scan_tail_with_limits(
                        &file,
                        expected,
                        headers[seg as usize].has_dict(),
                        punched,
                        read_limits,
                    )?;
                    if good != expected {
                        bail!(
                            "committed fold segment {seg} scans to {good} of its required {expected} bytes — the fold lost durable bytes"
                        );
                    }
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

        // `headers[i]`, `readers[i]`, and segment number i are the same index.
        let mut readers: Vec<Arc<dyn ReadAt>> = Vec::with_capacity(headers.len());
        for h in &headers {
            readers.push(Arc::new(segment::open_rw(dir, h.seg)?));
        }

        // Rebuild the block directory across every segment. Frames carry their ids, so this works
        // even though blocks were written in completion order rather than id order. Closed
        // segments answer from their directory sidecars when they can — that is what keeps open
        // O(active segment) instead of O(store) — and are rescanned (and their sidecar
        // regenerated) when they cannot.
        let mut blockdir: Vec<Option<(u32, u32)>> = Vec::new();
        let mut next_block = 0u32;
        let mut seen_blocks = 0u64;
        for (i, h) in headers.iter().enumerate() {
            let len = readers[i].len()?;
            let entries = if h.seg != active {
                match segment::read_dir_sidecar_with_limits(dir, h.seg, len, read_limits)? {
                    Some((_, e))
                        if segment::validate_dir_sidecar_entries(
                            &*readers[i],
                            len,
                            h.has_dict(),
                            &e,
                            read_limits,
                        )? =>
                    {
                        e
                    }
                    None => {
                        let (tail, e) = segment::scan_tail_with_limits(
                            &readers[i],
                            len,
                            h.has_dict(),
                            punched,
                            read_limits,
                        )?;
                        if tail != len {
                            bail!(
                                "closed fold segment {} scans to {tail} of its {len} bytes",
                                h.seg
                            );
                        }
                        // Regenerate so the next open finds it. Best-effort: advisory data must
                        // never fail an open, only slow one down.
                        let _ = segment::write_dir_sidecar(dir, h.seg, tail as u32, &e);
                        e
                    }
                    Some(_) => {
                        let (tail, e) = segment::scan_tail_with_limits(
                            &*readers[i],
                            len,
                            h.has_dict(),
                            punched,
                            read_limits,
                        )?;
                        if tail != len {
                            bail!(
                                "closed fold segment {} scans to {tail} of its {len} bytes",
                                h.seg
                            );
                        }
                        let _ = segment::write_dir_sidecar(dir, h.seg, tail as u32, &e);
                        e
                    }
                }
            } else {
                let (tail, entries) = segment::scan_tail_with_limits(
                    &readers[i],
                    len,
                    h.has_dict(),
                    punched,
                    read_limits,
                )?;
                if tail != len {
                    bail!("active fold segment {} scans to {tail} of its {len} bytes", h.seg);
                }
                entries
            };
            for (id, off) in entries {
                install_block_location(
                    &mut blockdir,
                    &mut next_block,
                    &mut seen_blocks,
                    id,
                    (h.seg, off),
                    read_limits,
                )?;
            }
        }
        validate_punched_ranges(punched, &blockdir, false)?;

        Ok(Fold {
            dir: dir.to_path_buf(),
            cfg,
            headers,
            readers,
            dicts,
            active,
            cur_off: target as u32,
            segs: Box::new(segstore::DirSegments::new(
                dir.to_path_buf(),
                active,
                active_f,
                read_limits,
            )),
            open_block: Vec::with_capacity(cfg.block_target * 2),
            dedup: DedupTable::new(),
            cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
            poisoned: false,
            write_failed: false,
            scratch: Vec::new(),
            blockdir,
            next_block,
            inflight: HashMap::new(),
            pool: pipe::Pool::new(nthreads(cfg.compress_threads), cfg.level, None),
            _lock: Some(lock),
            punched: punched.to_vec(),
            read_limits,
        })
    }

    /// Open a writer fold whose segments live as members of a container — the live-file store's
    /// content plane. No truncation and no unlinking happen here, because they cannot be needed:
    /// the selected container directory's extent lists ARE the truncation. Bytes a crash left past the
    /// published state are outside every selected extent and therefore do not exist; a segment
    /// created after the last container-state publication is in no directory and therefore does not exist. The two
    /// crash-tail truncation and cleanup that the file-backed staging fold performs at open are
    /// replaced by selection through the container directory.
    ///
    /// The caller owes: `committed` from the manifest this container carries, `punched` from that
    /// same manifest, and writer exclusion at the container-file level — the fold takes no lock
    /// of its own here.
    pub fn open_container_writer(
        container: std::sync::Arc<std::sync::Mutex<crate::container::Container>>,
        fold_gen: u32,
        cfg: FoldCfg,
        committed: Option<FoldTail>,
        punched: &[(u32, u32)],
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Fold> {
        let read_limits = read_limits.validate()?;
        validate_cfg(cfg)?;
        let prefix = fold_member_prefix(fold_gen);
        let member_of = |seg: u32| format!("{prefix}/{}", segment::seg_name(seg));

        // Enumerate this generation's segment members. Names come from a committed (or staged)
        // directory that already validated them; parse rather than trust regardless.
        let nums: Vec<u32> = {
            let c = container.lock().expect("container lock poisoned");
            let mut nums: Vec<u32> = c
                .names()
                .filter_map(|name| name.strip_prefix(&format!("{prefix}/")))
                .filter_map(segment::parse_seg_name)
                .collect();
            nums.sort_unstable();
            nums
        };
        for (i, &n) in nums.iter().enumerate() {
            if n != i as u32 {
                bail!("fold {prefix} has a gap: segment {} is missing", i);
            }
        }

        if nums.is_empty() {
            if !punched.is_empty() {
                bail!("a fold with no blocks cannot carry punched block declarations");
            }
            if let Some(ct) = committed {
                bail!(
                    "committed fold tail (seg {}, off {}) but the container holds no fold segment — the fold lost durable data",
                    ct.seg,
                    ct.off
                );
            }
            let name = member_of(0);
            {
                let mut c = container.lock().expect("container lock poisoned");
                c.put_bytes(&name, &SegHeader { seg: 0, flags: 0, dict_id: [0u8; 32] }.encode())?;
            }
            let reader: Arc<dyn ReadAt> =
                Arc::new(crate::container::MemberReader::new(container.clone(), name));
            return Ok(Fold {
                dir: PathBuf::from(&prefix),
                cfg,
                headers: vec![SegHeader { seg: 0, flags: 0, dict_id: [0u8; 32] }],
                readers: vec![reader],
                dicts: HashMap::new(),
                active: 0,
                cur_off: SEG_HDR_LEN as u32,
                segs: Box::new(segstore::ContainerSegments::new(container, prefix)),
                open_block: Vec::with_capacity(cfg.block_target * 2),
                dedup: DedupTable::new(),
                cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
                poisoned: false,
                write_failed: false,
                scratch: Vec::new(),
                blockdir: Vec::new(),
                next_block: 0,
                inflight: HashMap::new(),
                pool: pipe::Pool::new(nthreads(cfg.compress_threads), cfg.level, None),
                _lock: None,
                punched: punched.to_vec(),
                read_limits,
            });
        }

        let mut headers = Vec::with_capacity(nums.len());
        let mut readers: Vec<Arc<dyn ReadAt>> = Vec::with_capacity(nums.len());
        for &n in &nums {
            let name = member_of(n);
            let reader = crate::container::MemberReader::new(container.clone(), name.clone());
            let mut hb = [0u8; SEG_HDR_LEN as usize];
            crate::readat::ReadAt::read_exact_at(&reader, &mut hb, 0)
                .with_context(|| format!("read header of segment member {name}"))?;
            headers.push(SegHeader::decode(&hb, n)?);
            readers.push(Arc::new(reader));
        }

        let mut dicts: HashMap<[u8; 32], Arc<Vec<u8>>> = HashMap::new();
        for h in &headers {
            if !h.has_dict() || dicts.contains_key(&h.dict_id) {
                continue;
            }
            let name = format!("{prefix}/zdict-{}.zd", PieceHash(h.dict_id).to_hex());
            let bytes = {
                let c = container.lock().expect("container lock poisoned");
                c.read_file_bounded(&name, MAX_DICTIONARY_BYTES).with_context(|| {
                    format!("segment {} names dictionary {name} but it is unreadable", h.seg)
                })?
            };
            let got: [u8; 32] = blake3::hash(&bytes).into();
            if got != h.dict_id {
                bail!("dictionary {name} content hash does not match the id naming it");
            }
            dicts.insert(h.dict_id, Arc::new(bytes));
        }

        // The committed directory and the manifest must AGREE on the tail: they publish in the
        // same flip, so a disagreement is corruption or loss, never a crash to roll back from.
        let active = *nums.last().unwrap();
        let alen = crate::readat::ReadAt::len(&readers[active as usize])?;
        let cur_off = match committed {
            Some(ct) => {
                if ct.seg != active || u64::from(ct.off) != alen {
                    bail!(
                        "committed fold tail (seg {}, off {}) but the container's active segment \
                         member is (seg {active}, len {alen}) — the manifest and the container \
                         disagree",
                        ct.seg,
                        ct.off,
                    );
                }
                ct.off
            }
            None => u32::try_from(alen).context("segment member longer than a segment can be")?,
        };

        let mut blockdir: Vec<Option<(u32, u32)>> = Vec::new();
        let mut next_block = 0u32;
        let mut seen_blocks = 0u64;
        for (i, h) in headers.iter().enumerate() {
            let len = crate::readat::ReadAt::len(&readers[i])?;
            let (good, entries) = segment::scan_tail_with_limits(
                &readers[i],
                len,
                h.has_dict(),
                punched,
                read_limits,
            )?;
            // Committed extents hold only good frames by construction — the pre-flip barrier
            // made every byte durable before the directory named it. A scan that ends early is
            // therefore damage, and the refusal must say so.
            if good != len {
                bail!(
                    "fold segment {} scans to {good} of its {len} committed bytes — the fold \
                     lost durable data",
                    h.seg
                );
            }
            for (id, off) in entries {
                install_block_location(
                    &mut blockdir,
                    &mut next_block,
                    &mut seen_blocks,
                    id,
                    (h.seg, off),
                    read_limits,
                )?;
            }
        }
        validate_punched_ranges(punched, &blockdir, false)?;

        Ok(Fold {
            dir: PathBuf::from(&prefix),
            cfg,
            headers,
            readers,
            dicts,
            active,
            cur_off,
            segs: Box::new(segstore::ContainerSegments::new(container, prefix)),
            open_block: Vec::with_capacity(cfg.block_target * 2),
            dedup: DedupTable::new(),
            cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
            poisoned: false,
            write_failed: false,
            scratch: Vec::new(),
            blockdir,
            next_block,
            inflight: HashMap::new(),
            pool: pipe::Pool::new(nthreads(cfg.compress_threads), cfg.level, None),
            _lock: None,
            punched: punched.to_vec(),
            read_limits,
        })
    }

    /// Tell this fold which blocks the manifest declares erased, as inclusive `[lo, hi]` ranges.
    ///
    /// The caller owes the right manifest, and for a retained read view that is the current one, not the
    /// snapshot's own: punching commits a new manifest, so the retained copy predates the erasure
    /// and declares nothing. `punched` is cumulative in the current manifest revision, which makes this
    /// answerable at all.
    pub fn declare_punched(&mut self, ranges: &[(u32, u32)]) {
        self.punched = ranges.to_vec();
    }

    pub(crate) fn is_punched(&self, block_id: u32) -> bool {
        self.punched.iter().any(|&(lo, hi)| block_id >= lo && block_id <= hi)
    }

    pub(crate) fn verify_location_shape(&self, loc: Loc) -> Result<()> {
        let (seg, off) =
            *self.blockdir.get(loc.block_id as usize).and_then(|entry| entry.as_ref()).ok_or_else(
                || {
                    anyhow::anyhow!(
                        "block {} is outside this store authority's fold tail",
                        loc.block_id
                    )
                },
            )?;
        if u64::from(off) < SEG_HDR_LEN {
            bail!("block {} offset {off} is inside the segment header", loc.block_id);
        }
        let reader = self
            .readers
            .get(seg as usize)
            .ok_or_else(|| anyhow::anyhow!("block {} names absent segment {seg}", loc.block_id))?;
        let mut encoded = [0u8; block::BLOCK_HDR_LEN];
        reader.read_exact_at(&mut encoded, u64::from(off))?;
        let header = block::parse_hdr(&encoded, self.headers[seg as usize].has_dict())?;
        if header.block_id != loc.block_id {
            bail!(
                "block directory maps {} to a frame carrying id {}",
                loc.block_id,
                header.block_id
            );
        }
        let piece_end = loc
            .in_off
            .checked_add(loc.raw)
            .ok_or_else(|| anyhow::anyhow!("piece location in block {} overflows", loc.block_id))?;
        if piece_end > header.raw {
            bail!(
                "piece location {}..{} lies outside block {}'s {} raw bytes",
                loc.in_off,
                piece_end,
                loc.block_id,
                header.raw
            );
        }
        Ok(())
    }

    /// Open WITHOUT the writer lock, read-only.
    ///
    /// Takes no lock, truncates nothing, sweeps nothing — a reader must never mutate a store another
    /// process is writing. Safe concurrently with a live writer: segments are append-only and blocks
    /// are immutable once written, so a reader sees a prefix that only ever grows. No manifest in
    /// hand, so no punched declaration reaches the scan.
    pub fn open_read(dir: &Path, cfg: FoldCfg) -> Result<Fold> {
        Fold::open_read_with_limits(dir, cfg, &[], crate::read_limits::ReadLimits::default())
    }

    /// Open an unlocked reader with explicit frame and object-count admission, declaring the
    /// manifest's punched block ranges so the scan can step over erased frames.
    pub fn open_read_with_limits(
        dir: &Path,
        cfg: FoldCfg,
        punched: &[(u32, u32)],
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Fold> {
        let read_limits = read_limits.validate()?;
        validate_cfg(cfg)?;
        let mut nums = list_segments_with_limits(dir, read_limits)?;
        if nums.is_empty() {
            bail!("no fold segments under {}", dir.display());
        }
        nums.sort_unstable();
        let mut segs = Vec::with_capacity(nums.len());
        for &n in &nums {
            let reader = Arc::new(segment::open_read(dir, n)?);
            let len = reader.metadata()?.len();
            segs.push(SegmentInput {
                seg: n,
                reader: reader as Arc<dyn ReadAt>,
                sidecar: segment::read_dir_sidecar_bytes_with_limits(dir, n, len, read_limits)?,
            });
        }
        let dict_files = read_directory_dictionaries(dir, read_limits)?;
        Fold::open_read_from_with_limits(segs, dict_files, cfg, dir, punched, read_limits)
    }

    /// Open only the durable prefix named by a manifest, ignoring later append residue. No
    /// punched declaration in hand — a caller holding the manifest uses the `_with_limits` form.
    pub fn open_read_at(dir: &Path, cfg: FoldCfg, committed: FoldTail) -> Result<Fold> {
        Fold::open_read_at_with_limits(
            dir,
            cfg,
            committed,
            &[],
            crate::read_limits::ReadLimits::default(),
        )
    }

    /// Open a published read-only prefix with explicit frame and object-count admission, plus the
    /// manifest's punched declaration for the scan.
    pub fn open_read_at_with_limits(
        dir: &Path,
        cfg: FoldCfg,
        committed: FoldTail,
        punched: &[(u32, u32)],
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Fold> {
        let read_limits = read_limits.validate()?;
        let mut nums = list_segments_with_limits(dir, read_limits)?;
        nums.retain(|segment| *segment <= committed.seg);
        nums.sort_unstable();
        if nums.last().copied() != Some(committed.seg) {
            bail!(
                "committed fold tail names segment {} but it is absent under {}",
                committed.seg,
                dir.display()
            );
        }
        let mut segs = Vec::with_capacity(nums.len());
        for &segment in &nums {
            let file = Arc::new(segment::open_read(dir, segment)?);
            let physical = file.metadata()?.len();
            let len = if segment == committed.seg { committed.off as u64 } else { physical };
            if len > physical {
                bail!(
                    "committed fold tail is {} bytes into segment {segment}, which holds only {physical}",
                    committed.off
                );
            }
            segs.push(SegmentInput {
                seg: segment,
                reader: Arc::new(Slice::new(file, 0, len)) as Arc<dyn ReadAt>,
                // The committed segment may have been sealed before a later generation became
                // active, and punched blocks can only be located through its sidecar. The parser
                // below accepts it only when its embedded tail equals this bounded reader's length,
                // so a sidecar describing a newer suffix remains safely advisory.
                sidecar: segment::read_dir_sidecar_bytes_with_limits(
                    dir,
                    segment,
                    len,
                    read_limits,
                )?,
            });
        }
        let dict_files = read_directory_dictionaries(dir, read_limits)?;
        Fold::open_read_from_with_limits(segs, dict_files, cfg, dir, punched, read_limits)
    }

    /// Open read-only from prepared inputs — the entry every source uses. A container hands in
    /// member extents; a remote store would hand in range readers. Nothing below this knows or
    /// cares which.
    ///
    /// `dict_files` are candidate trained dictionaries, identified by content hash — a segment
    /// naming a dictionary no candidate hashes to is refused. `label` names the source in errors
    /// and [`Fold::dir`], and is never touched as a path.
    pub fn open_read_from(
        segs: Vec<SegmentInput>,
        dict_files: Vec<Vec<u8>>,
        cfg: FoldCfg,
        label: &Path,
    ) -> Result<Fold> {
        Fold::open_read_from_with_limits(
            segs,
            dict_files,
            cfg,
            label,
            &[],
            crate::read_limits::ReadLimits::default(),
        )
    }

    /// Backend-neutral read open with explicit frame and object-count admission. `punched` is the
    /// owning manifest's declared-erased block ranges: a source that has punched blocks may hold a
    /// frame whose payload a crash left partially zeroed, and the declaration is what lets the
    /// rebuild scan step over it.
    pub fn open_read_from_with_limits(
        segs: Vec<SegmentInput>,
        dict_files: Vec<Vec<u8>>,
        cfg: FoldCfg,
        label: &Path,
        punched: &[(u32, u32)],
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Fold> {
        Self::open_read_from_with_limits_inner(
            segs,
            dict_files,
            cfg,
            label,
            punched,
            read_limits,
            false,
        )
    }

    /// Open an older retained prefix using the current manifest revision's content-punch declarations. Declarations
    /// above this prefix's block-id ceiling belong to later publication and do not make the older
    /// prefix malformed; declarations within it remain mandatory and authoritative.
    pub(crate) fn open_retained_read_from_with_limits(
        segs: Vec<SegmentInput>,
        dict_files: Vec<Vec<u8>>,
        cfg: FoldCfg,
        label: &Path,
        punched: &[(u32, u32)],
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Fold> {
        Self::open_read_from_with_limits_inner(
            segs,
            dict_files,
            cfg,
            label,
            punched,
            read_limits,
            true,
        )
    }

    fn open_read_from_with_limits_inner(
        mut segs: Vec<SegmentInput>,
        dict_files: Vec<Vec<u8>>,
        cfg: FoldCfg,
        label: &Path,
        punched: &[(u32, u32)],
        read_limits: crate::read_limits::ReadLimits,
        allow_future_punched: bool,
    ) -> Result<Fold> {
        let read_limits = read_limits.validate()?;
        // An empty fold is a store nothing has flushed yet: readable and answering nothing. A
        // reader may arrive before the first flush creates a segment.
        if segs.is_empty() {
            if !dict_files.is_empty() {
                bail!("fold has dictionary members but no segment can reference them");
            }
            if !punched.is_empty() && !allow_future_punched {
                bail!("a fold with no blocks cannot carry punched block declarations");
            }
            return Ok(Fold {
                dir: label.to_path_buf(),
                cfg,
                headers: Vec::new(),
                readers: Vec::new(),
                dicts: HashMap::new(),
                active: 0,
                cur_off: SEG_HDR_LEN as u32,
                segs: Box::new(segstore::NoSegments),
                open_block: Vec::new(),
                dedup: DedupTable::new(),
                cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
                poisoned: false,
                write_failed: false,
                scratch: Vec::new(),
                blockdir: Vec::new(),
                next_block: 0,
                inflight: HashMap::new(),
                pool: pipe::Pool::new(nthreads(cfg.compress_threads), cfg.level, None),
                _lock: None,
                punched: punched.to_vec(),
                read_limits,
            });
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
        let mut blockdir: Vec<Option<(u32, u32)>> = Vec::new();
        let mut next_block = 0u32;
        let mut seen_blocks = 0u64;
        for (i, h) in headers.iter().enumerate() {
            let len = readers[i].len()?;
            if len > SEG_MAX_LIMIT {
                bail!(
                    "fold segment {} is {len} bytes, over the {SEG_MAX_LIMIT} format bound",
                    h.seg
                );
            }
            let (good, entries) = match segs[i].sidecar.as_ref() {
                Some(bytes) => {
                    match segment::parse_dir_sidecar_with_limits(bytes, h.seg, len, read_limits)? {
                        Some((_, entries))
                            if segment::validate_dir_sidecar_entries(
                                &*readers[i],
                                len,
                                h.has_dict(),
                                &entries,
                                read_limits,
                            )? =>
                        {
                            (len, entries)
                        }
                        None => segment::scan_tail_with_limits(
                            &readers[i],
                            len,
                            h.has_dict(),
                            punched,
                            read_limits,
                        )?,
                        Some(_) => segment::scan_tail_with_limits(
                            &*readers[i],
                            len,
                            h.has_dict(),
                            punched,
                            read_limits,
                        )?,
                    }
                }
                None => segment::scan_tail_with_limits(
                    &readers[i],
                    len,
                    h.has_dict(),
                    punched,
                    read_limits,
                )?,
            };
            if good != len {
                bail!(
                    "fold segment {} scans to {good} of its {len} committed bytes — the fold lost durable data",
                    h.seg
                );
            }
            for (id, off) in entries {
                install_block_location(
                    &mut blockdir,
                    &mut next_block,
                    &mut seen_blocks,
                    id,
                    (h.seg, off),
                    read_limits,
                )?;
            }
        }
        validate_punched_ranges(punched, &blockdir, allow_future_punched)?;
        let cur_off = u32::try_from(readers.last().unwrap().len()?)
            .context("active segment is longer than its u32 offset domain")?;
        Ok(Fold {
            dir: label.to_path_buf(),
            cfg,
            headers,
            readers,
            dicts,
            active: segs.last().unwrap().seg,
            cur_off,
            segs: Box::new(segstore::NoSegments),
            open_block: Vec::new(),
            dedup: DedupTable::with_capacity(16),
            cache: Mutex::new(BlockCache::new(cfg.cache_bytes)),
            poisoned: true, // a read-only fold must refuse every append
            write_failed: false,
            scratch: Vec::new(),
            blockdir,
            next_block,
            inflight: HashMap::new(),
            pool: pipe::Pool::new(1, cfg.level, None),
            _lock: None,
            punched: punched.to_vec(),
            read_limits,
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
        validate_piece_len(raw.len() as u64)?;
        debug_assert_eq!(
            hash,
            PieceHash::of(raw),
            "put_hashed given a digest that is not its content's"
        );
        if let Some(loc) = self.dedup.get(&hash) {
            return Ok(Put { hash, loc, deduped: true });
        }

        // A limit smaller than the ordinary block target deliberately becomes the effective seal
        // target. Seal BEFORE appending when needed, and refuse one indivisible piece before any
        // fold mutation. This is the progress rule that lets strict profiles keep accepting small
        // records without ever writing a block they cannot reopen.
        let atomic = self
            .read_limits
            .max_stored_frame_bytes
            .min(self.read_limits.max_decoded_frame_bytes)
            .min(BLOCK_PAYLOAD_MAX);
        self.read_limits.admit("new fold block", raw.len() as u64, raw.len() as u64)?;
        let starts_new_block = self.open_block.is_empty()
            || self.open_block.len() as u64 > atomic.saturating_sub(raw.len() as u64);
        let proposed_block = if self.open_block.is_empty() {
            self.next_block
        } else if starts_new_block {
            self.next_block
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("block id space exhausted"))?
        } else {
            self.next_block
        };
        // `next_block` is the ID of the open block and becomes its successor when that block
        // seals. A block at u32::MAX could not leave a representable successor and could not be
        // reopened by the current format, so refuse before appending or seeding dedup state.
        if proposed_block == u32::MAX {
            bail!("block id space exhausted");
        }
        if starts_new_block {
            self.read_limits.admit_fold_blocks(u64::from(proposed_block) + 1)?;
        }
        if !self.open_block.is_empty() && starts_new_block {
            self.seal_block()?;
            self.write_ready(false)?;
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

    pub(crate) fn ensure_no_failed_write(&self) -> Result<()> {
        if self.write_failed {
            bail!("fold is poisoned by an earlier failed write; reopen to recover by tail scan");
        }
        Ok(())
    }

    /// Run `f` over the piece's bytes wherever they live — the open buffer, the compression
    /// pipeline, or a (cached) decompressed block — WITHOUT copying them anywhere first. Every
    /// read shape is a projection of this one.
    fn with_piece<T>(&self, loc: Loc, f: impl FnOnce(&[u8]) -> T) -> Result<T> {
        crate::io_trace::fold_block_touched(loc.block_id);
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

    /// Verify one piece and expose its borrowed bytes to a bounded caller without materializing a
    /// second piece-sized allocation. The bytes remain owned by the Fold block cache and are valid
    /// only for the duration of `visit`.
    pub(crate) fn visit_verified(
        &self,
        loc: Loc,
        expect: PieceHash,
        visit: impl FnOnce(&[u8]),
    ) -> Result<()> {
        self.with_piece(loc, |bytes| -> Result<()> {
            let got = PieceHash::of(bytes);
            if got != expect {
                bail!(
                    "content hash mismatch in block {} at +{}: got {got}, expected {expect}",
                    loc.block_id,
                    loc.in_off
                );
            }
            visit(bytes);
            Ok(())
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
            crate::io_trace::fold_block_cache_hit();
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
        if self.is_punched(loc.block_id) {
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

        self.read_limits.admit(
            format!("fold block {}", hdr.block_id),
            u64::from(hdr.stored),
            u64::from(hdr.raw),
        )?;

        let frame_end = u64::from(off)
            .checked_add(hdr.frame_len())
            .ok_or_else(|| anyhow::anyhow!("fold block end overflows"))?;
        if frame_end > SEG_MAX_LIMIT {
            bail!(
                "fold block {} ends at {frame_end}, over the {SEG_MAX_LIMIT} segment bound",
                hdr.block_id
            );
        }
        let span = block::frame_span_usize(hdr.stored)?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(span).map_err(|_| {
            anyhow::anyhow!("cannot allocate {span} bytes for fold block {}", hdr.block_id)
        })?;
        buf.resize(span, 0);
        buf[..block::BLOCK_HDR_LEN].copy_from_slice(&hb);
        f.read_exact_at(&mut buf[block::BLOCK_HDR_LEN..], off as u64 + block::BLOCK_HDR_LEN as u64)
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
        crate::io_trace::fold_block_cache_miss(span as u64, hdr.raw);
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
        if let Err(error) = self.pool.submit(id, raw) {
            self.poisoned = true;
            self.write_failed = true;
            return Err(error).context("submit fold block; fold poisoned until reopen");
        }
        Ok(())
    }

    /// Append every block the pool has finished. With `wait`, blocks until the pool is empty.
    fn write_ready(&mut self, wait: bool) -> Result<()> {
        let done = if wait {
            match self.pool.take_all() {
                Ok(done) => done,
                Err(error) => {
                    self.poisoned = true;
                    self.write_failed = true;
                    return Err(error)
                        .context("drain fold compression pool; fold poisoned until reopen");
                }
            }
        } else {
            self.pool.try_take()
        };
        for d in done {
            self.write_block(d)?;
        }
        Ok(())
    }

    fn write_block(&mut self, d: pipe::Done) -> Result<()> {
        let result = self.write_block_inner(d);
        if result.is_err() {
            self.poisoned = true;
            self.write_failed = true;
        }
        result.context("write fold block; fold poisoned until reopen")
    }

    fn write_block_inner(&mut self, d: pipe::Done) -> Result<()> {
        let n = block::encode(&mut self.scratch, d.block_id, d.codec, &d.raw, &d.payload)?;
        // Roll if this block would not fit. Blocks are self-contained, so one never straddles.
        let mut end = u64::from(self.cur_off)
            .checked_add(n as u64)
            .ok_or_else(|| anyhow::anyhow!("fold segment append point overflows"))?;
        if self.cur_off as u64 > SEG_HDR_LEN && end > self.cfg.seg_max as u64 {
            self.roll()?;
            end = u64::from(self.cur_off)
                .checked_add(n as u64)
                .ok_or_else(|| anyhow::anyhow!("fold segment append point overflows"))?;
        }
        if end > SEG_MAX_LIMIT {
            bail!(
                "fold block {} would end at {end}, over the {SEG_MAX_LIMIT} segment bound",
                d.block_id
            );
        }
        let frame_end = n;
        self.segs.append(self.active, self.cur_off, &self.scratch[..frame_end])?;
        self.read_limits.admit_fold_blocks(u64::from(d.block_id) + 1)?;
        if self.blockdir.len() <= d.block_id as usize {
            self.blockdir.resize(d.block_id as usize + 1, None);
        }
        if self.blockdir[d.block_id as usize].replace((self.active, self.cur_off)).is_some() {
            bail!("fold block id {} was written more than once", d.block_id);
        }
        self.cur_off = u32::try_from(end).context("fold segment tail exceeds u32")?;
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
        if self.poisoned {
            bail!("fold is poisoned by an earlier failed write; reopen to recover by tail scan");
        }
        let result = (|| -> Result<FoldTail> {
            self.seal_block()?;
            // Every block must be compressed AND written before a tail can be reported durable.
            self.write_ready(true)?;
            self.segs.sync(self.active)?;
            let entries = self.active_block_entries();
            self.segs.stage_active_sidecar(self.active, self.cur_off, &entries)?;
            Ok(self.tail())
        })();
        if result.is_err() {
            self.poisoned = true;
            self.write_failed = true;
        }
        result.context("synchronize fold; fold poisoned until reopen")
    }

    /// The current append point. Pieces in the open buffer live AT this offset and are not yet durable.
    pub fn tail(&self) -> FoldTail {
        FoldTail { seg: self.active, off: self.cur_off }
    }

    /// Resolve content to a location through the current in-memory dedup window.
    ///
    /// Only covers pieces not yet published in a part — published pieces are found through the
    /// parts' own dictionaries, which is why this index needs no on-disk form. A miss is never wrong,
    /// only slower.
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

    /// Release the dedup window — the pieces it covers are now indexed by a published part, so
    /// resident memory tracks the flush interval rather than the store.
    pub fn release_dedup_window(&mut self) {
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
    /// when no record resolved by current authority references any piece in it. The store computes that from the parts and
    /// records the result in the manifest before calling here — because a punched block read as
    /// merely corrupt is an ops fire drill, and only an authoritative record prevents it.
    ///
    /// Returns the ids actually punched. A block already punched, or whose frame will not parse,
    /// is skipped rather than errored — this is reclamation, and a block it cannot account for is
    /// a block it leaves alone.
    pub fn punch_blocks(&mut self, dead: &[u32]) -> Result<Vec<u32>> {
        self.punch_blocks_with_control(dead, &crate::control::OperationControl::default())
    }

    /// [`Fold::punch_blocks`] with a safe checkpoint before each independently reclaimable block.
    pub fn punch_blocks_with_control(
        &mut self,
        dead: &[u32],
        control: &crate::control::OperationControl,
    ) -> Result<Vec<u32>> {
        let mut done = Vec::new();
        for &id in dead {
            control.check("fold punch")?;
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
            // The PAYLOAD only — the header stays so the frame chain remains walkable. See
            // `segment::punch`.
            self.segs.punch(seg, off as u64 + block::BLOCK_HDR_LEN as u64, hdr.stored as u64)?;
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
    /// identically over local or remote positioned sources because it reads through [`ReadAt`].
    ///
    /// A frame that fails its checksum ends a segment's valid span; a sealed segment whose span
    /// ends before its file does is corruption and errors. The ACTIVE segment is allowed a
    /// trailing invalid region (uncommitted writes a crash abandoned), which is reported, not
    /// condemned.
    pub fn scrub(&self) -> Result<FoldScrub> {
        self.scrub_with_control(&crate::control::OperationControl::default())
    }

    /// [`Fold::scrub`] with cooperative checks between segments and complete frames.
    pub fn scrub_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<FoldScrub> {
        let mut report = FoldScrub::default();
        for (i, h) in self.headers.iter().enumerate() {
            control.check("fold verification")?;
            let len = self.readers[i].len()?;
            // With this fold's punched declaration: verification runs against crash residue too,
            // and a declared block mid-punch (payload part-zeroed, retry pending) is accounted
            // for, not corrupt.
            let (end, entries) = segment::scan_tail_controlled_with_limits(
                &self.readers[i],
                len,
                h.has_dict(),
                &self.punched,
                control,
                "fold verification",
                self.read_limits,
            )?;
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
        CacheStats { hits: c.hits, misses: c.misses, bytes: c.bytes, budget: c.budget }
    }

    /// Every block id the directory knows — the universe a reachability sweep works against.
    pub fn block_ids(&self) -> Vec<u32> {
        self.blockdir.iter().enumerate().filter_map(|(id, e)| e.map(|_| id as u32)).collect()
    }

    /// Read the framing facts for every block the current directory can address.
    ///
    /// This deliberately reads headers only: callers can classify compressed storage without
    /// decompressing content or warming the block cache. Blocks declared punched may still appear
    /// here after reopen because sealed-segment sidecars preserve their framing locations; the
    /// store layer owns that manifest-level classification.
    pub(crate) fn block_inventory_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<Vec<BlockStorage>> {
        let mut inventory = Vec::new();
        for (id, entry) in self.blockdir.iter().enumerate() {
            control.check("fold block inventory")?;
            let Some((seg, off)) = *entry else { continue };
            let id = u32::try_from(id).map_err(|_| anyhow::anyhow!("block id exceeds u32"))?;
            let reader = self.readers.get(seg as usize).ok_or_else(|| {
                anyhow::anyhow!("block {id} names segment {seg} which does not exist")
            })?;
            let mut bytes = [0u8; block::BLOCK_HDR_LEN];
            reader
                .read_exact_at(&mut bytes, u64::from(off))
                .with_context(|| format!("read block {id} header at seg {seg} off {off}"))?;
            let header = block::parse_hdr(&bytes, self.headers[seg as usize].has_dict())?;
            if header.block_id != id {
                bail!("directory sent block {id} to a frame carrying id {}", header.block_id);
            }
            inventory.push(BlockStorage {
                block_id: id,
                segment: seg,
                raw_bytes: header.raw,
                stored_bytes: header.stored,
            });
        }
        Ok(inventory)
    }

    pub fn segment_count(&self) -> u32 {
        self.headers.len() as u32
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Total bytes across all segment files. Excludes anything still in the open buffer.
    pub fn disk_bytes(&self) -> u64 {
        // Measured through the readers rather than reconstructing filesystem paths from `dir`,
        // which may be only a source label. A reader knows its own length whatever is behind it.
        self.readers.iter().filter_map(|r| crate::readat::ReadAt::len(r).ok()).sum()
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

    fn active_block_entries(&self) -> Vec<(u32, u32)> {
        let mut entries: Vec<(u32, u32)> = self
            .blockdir
            .iter()
            .enumerate()
            .filter_map(|(id, entry)| match entry {
                Some((segment, offset)) if *segment == self.active => Some((id as u32, *offset)),
                _ => None,
            })
            .collect();
        entries.sort_by_key(|&(_, offset)| offset);
        entries
    }

    /// Roll to a new segment. Every physical step happens before any logical state moves, so a failure
    /// leaves nothing changed and the caller's retry re-enters cleanly. (An earlier generation of this
    /// engine advanced the offset first; a roll-time ENOSPC then left a zero offset over the *old*
    /// segment handle and the next write silently corrupted the fold.)
    fn roll(&mut self) -> Result<()> {
        self.segs.admit_roll()?;
        let flags = self.headers[self.active as usize].flags;
        self.segs.sync(self.active).context("fsync before roll")?;
        // The segment being sealed gets its directory sidecar now — the write that turns the next
        // open's full scan of it into a 2 KB read. AFTER the sync above, so a sidecar can never
        // describe bytes less durable than itself. A container propagates failure so remote-open
        // locality is a property of every state its superblock publishes.
        let entries = self.active_block_entries();
        self.segs.write_sidecar(self.active, self.cur_off, &entries)?;
        let next = self
            .active
            .checked_add(1)
            .filter(|number| *number <= segment::MAX_SEGMENT_NUMBER)
            .ok_or_else(|| anyhow::anyhow!("segment number space exhausted"))?;
        let reader = self.segs.create_segment(next, [0u8; 32], flags)?;

        self.headers.push(SegHeader { seg: next, flags, dict_id: [0u8; 32] });
        self.readers.push(reader);
        self.active = next;
        self.cur_off = SEG_HDR_LEN as u32;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockStorage {
    pub block_id: u32,
    pub segment: u32,
    pub raw_bytes: u32,
    pub stored_bytes: u32,
}

/// The member-name prefix a fold generation lives under inside a container: `fold` for
/// generation 0 and `fold-NNNN` above it.
pub(crate) fn fold_member_prefix(fold_gen: u32) -> String {
    if fold_gen == 0 {
        "fold".to_string()
    } else {
        format!("fold-{fold_gen:04}")
    }
}

/// The four-decimal-digit generation namespace admitted by the current draft format.
pub(crate) const MAX_FOLD_GENERATION: u32 = 9_999;

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

fn list_segments_with_limits(
    dir: &Path,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut visited = 0u64;
    for e in std::fs::read_dir(dir)? {
        visited = visited.saturating_add(1);
        read_limits.admit_directory_entries("fold directory", visited)?;
        let e = e?;
        if let Some(n) = segment::parse_seg_name(&e.file_name().to_string_lossy()) {
            out.push(n);
        }
    }
    Ok(out)
}

/// Read only canonical standalone Fold dictionary names, and bind every name to its bytes. The
/// prepared-source reader accepts anonymous candidate bytes because its namespace was validated by
/// its caller; a directory reader owns the filename grammar itself and must not erase that identity.
fn read_directory_dictionaries(
    dir: &Path,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<Vec<Vec<u8>>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("read fold directory {} for dictionaries", dir.display()))?;
    let mut dictionaries = Vec::new();
    let mut visited = 0u64;
    for entry in entries {
        visited = visited.saturating_add(1);
        read_limits.admit_directory_entries("fold directory", visited)?;
        let entry = entry?;
        let os_name = entry.file_name();
        let lossy = os_name.to_string_lossy();
        if !lossy.starts_with("zdict-") {
            continue;
        }
        let Some(name) = os_name.to_str() else {
            bail!("fold dictionary name is not UTF-8 current-format text");
        };
        let Some(digest) = name.strip_prefix("zdict-").and_then(|rest| rest.strip_suffix(".zd"))
        else {
            bail!("fold dictionary name {name:?} is not current-format grammar");
        };
        if digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("fold dictionary name {name:?} is not 64 lowercase hexadecimal digits");
        }
        let bytes = read_bounded_candidate(&entry.path(), MAX_DICTIONARY_BYTES)?;
        let exact = format!("zdict-{}.zd", PieceHash::of(&bytes).to_hex());
        if name != exact {
            bail!("fold dictionary {name:?} does not match its content identity {exact:?}");
        }
        dictionaries.push(bytes);
    }
    Ok(dictionaries)
}

/// Refuse any segment set that cannot have been written by this current Fold implementation.
/// Segment birth is staged and installed only after the complete header is durable, so every short
/// final-name segment is corruption or an unknown artifact and is never repaired in place.
fn validate_fold_segment_identities(
    dir: &Path,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<Vec<SegHeader>> {
    let mut nums = list_segments_with_limits(dir, read_limits)?;
    nums.sort_unstable();
    let mut headers = Vec::new();
    headers.try_reserve_exact(nums.len()).context("reserve fold segment headers")?;
    for (index, &n) in nums.iter().enumerate() {
        if n != index as u32 {
            bail!("fold segments are not dense: expected seg {index}, found {n}");
        }
        let path = segment::seg_path(dir, n);
        let f = crate::vfs::open_read(&path).with_context(|| format!("open {}", path.display()))?;
        let len = f.metadata()?.len();
        if len < SEG_HDR_LEN {
            bail!(
                "segment {n} has a truncated current-format header ({len} bytes) — refusing without mutation"
            );
        }
        let mut header = [0u8; SEG_HDR_LEN as usize];
        f.read_exact_at(&mut header, 0).with_context(|| format!("read segment {n} header"))?;
        headers.push(
            SegHeader::decode(&header, n)
                .with_context(|| format!("segment {n} has an invalid current-format header"))?,
        );
    }
    Ok(headers)
}

fn validate_fold_dictionary_dependencies(dir: &Path, headers: &[SegHeader]) -> Result<()> {
    let mut verified = HashSet::new();
    for header in headers {
        if !header.has_dict() || !verified.insert(header.dict_id) {
            continue;
        }
        let name = format!("zdict-{}.zd", PieceHash(header.dict_id).to_hex());
        let bytes =
            read_bounded_candidate(&dir.join(&name), MAX_DICTIONARY_BYTES).with_context(|| {
                format!("segment {} names dictionary {name} but it is unreadable", header.seg)
            })?;
        let actual: [u8; 32] = blake3::hash(&bytes).into();
        if actual != header.dict_id {
            bail!("dictionary {name} content hash does not match the id naming it");
        }
    }
    Ok(())
}

fn validate_committed_fold_prefix(
    dir: &Path,
    headers: &[SegHeader],
    committed: FoldTail,
    punched: &[(u32, u32)],
    read_limits: crate::read_limits::ReadLimits,
) -> Result<()> {
    if committed.seg as usize >= headers.len() || u64::from(committed.off) < SEG_HDR_LEN {
        bail!(
            "committed fold tail (seg {}, off {}) is outside the current-format segment domain",
            committed.seg,
            committed.off
        );
    }
    for seg in 0..=committed.seg {
        let file = segment::open_read(dir, seg)?;
        let physical = file.metadata()?.len();
        let expected = if seg == committed.seg { u64::from(committed.off) } else { physical };
        if expected > physical {
            bail!(
                "committed fold tail (seg {}, off {}) exceeds segment {seg}'s {physical} bytes — the fold lost durable bytes",
                committed.seg,
                committed.off
            );
        }
        let (good, _) = segment::scan_tail_with_limits(
            &file,
            expected,
            headers[seg as usize].has_dict(),
            punched,
            read_limits,
        )?;
        if good != expected {
            bail!(
                "committed fold segment {seg} scans to {good} of its required {expected} bytes — the fold lost durable bytes"
            );
        }
    }
    Ok(())
}

pub(super) fn count_fold_directory_entries(
    dir: &Path,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<u64> {
    Ok(fold_directory_shape(dir, read_limits)?.0)
}

fn fold_directory_shape(
    dir: &Path,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<(u64, bool, bool)> {
    let mut visited = 0u64;
    let mut has_lock = false;
    let mut has_segment = false;
    for entry in std::fs::read_dir(dir)? {
        visited = visited.saturating_add(1);
        read_limits.admit_directory_entries("fold directory", visited)?;
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        has_lock |= name == "WRITER.lock";
        has_segment |= segment::parse_seg_name(&name).is_some();
    }
    Ok((visited, has_lock, has_segment))
}

fn install_block_location(
    blockdir: &mut Vec<Option<(u32, u32)>>,
    next_block: &mut u32,
    seen_blocks: &mut u64,
    id: u32,
    location: (u32, u32),
    read_limits: crate::read_limits::ReadLimits,
) -> Result<()> {
    let required_ids = u64::from(id) + 1;
    let required_entries = seen_blocks.saturating_add(1);
    read_limits.admit_fold_blocks(required_ids)?;
    read_limits.admit_fold_blocks(required_entries)?;
    if blockdir.len() <= id as usize {
        blockdir.resize(id as usize + 1, None);
    }
    if blockdir[id as usize].replace(location).is_some() {
        bail!("fold block id {id} appears more than once in one generation");
    }
    *seen_blocks = required_entries;
    *next_block = (*next_block)
        .max(id.checked_add(1).ok_or_else(|| anyhow::anyhow!("fold block id space exhausted"))?);
    Ok(())
}

/// Exclusive writer lock held for the fold's whole lifetime — the single-writer invariant.
///
/// Enforced by the OS on native Unix and Windows. On `wasm32-wasip1`
/// `sys::lock_exclusive` succeeds unconditionally, so this creates the file and gates nothing:
/// there the invariant is convention, and it is the embedder's to keep.
pub(crate) fn acquire_writer_lock(dir: &Path) -> Result<File> {
    let path = dir.join("WRITER.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    if !crate::sys::lock_exclusive(&f).with_context(|| format!("locking {}", path.display()))? {
        return Err(WriterLocked { path: dir.to_path_buf() }.into());
    }
    Ok(f)
}

#[cfg(test)]
mod representation_tests {
    use super::*;

    #[test]
    fn a_piece_must_leave_room_for_its_complete_frame_and_segment_header() {
        validate_piece_len(BLOCK_PAYLOAD_MAX).unwrap();
        assert!(validate_piece_len(BLOCK_PAYLOAD_MAX + 1).is_err());
        assert_eq!(SEG_HDR_LEN + block::BLOCK_OVERHEAD as u64 + BLOCK_PAYLOAD_MAX, u32::MAX as u64);
    }

    #[test]
    fn block_id_exhaustion_refuses_before_open_block_or_dedup_mutation() {
        let root = std::env::temp_dir().join(format!(
            "turndb-fold-block-id-exhaustion-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let mut fold = Fold::open(
            &root,
            FoldCfg { block_target: 1, compress_threads: 1, ..Default::default() },
        )
        .unwrap();
        fold.next_block = u32::MAX;
        let hash = PieceHash::of(b"x");
        let error = fold.put_hashed(b"x", hash).unwrap_err();
        assert!(error.to_string().contains("block id space exhausted"), "{error:#}");
        assert!(fold.open_block.is_empty(), "refusal appended bytes to the open block");
        assert!(fold.lookup(hash).is_none(), "refusal seeded the dedup window");
        std::fs::remove_dir_all(root).ok();
    }
}
