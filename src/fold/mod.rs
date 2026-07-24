//! The fold — an append-only, content-addressed piece store.
//!
//! Identical bytes are stored once, forever, wherever they appear. Everything above the fold refers to
//! content by [`Loc`], and **nothing above the fold ever rewrites it**: merges reorganize references
//! and columns, never content. That is what decouples compaction cost from data volume.
//!
//! # The durability contract, in one sentence
//!
//! No part may be committed naming a [`Loc`] at or beyond a tail that [`Fold::sync`] has not returned.
//!
//! The fold is deliberately *not* a commit point. The WAL makes a record's carved pieces durable before
//! the fold is touched at all, so a crash anywhere between a `put` and a `sync` loses nothing that
//! replay cannot regenerate — which is why `put` never fsyncs.

pub mod codec;
pub mod dedup;
pub mod frame;
pub mod segment;

pub use frame::{Loc, CODEC_STORED, CODEC_ZSTD, CODEC_ZSTD_DICT};
pub use segment::FoldTail;

use crate::types::PieceHash;
use anyhow::{bail, Context, Result};
use dedup::DedupTable;
use segment::{SegHeader, SEG_HDR_LEN, SEG_MAX_DEFAULT, SEG_MAX_LIMIT};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct FoldCfg {
    /// Roll threshold. Bounded by [`SEG_MAX_LIMIT`] because `Loc.off` is a u32.
    pub seg_max: u32,
}

impl Default for FoldCfg {
    fn default() -> Self {
        FoldCfg { seg_max: SEG_MAX_DEFAULT }
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

pub struct Fold {
    dir: PathBuf,
    cfg: FoldCfg,
    /// Header per segment, indexed by segment number (segments are dense: 0..N).
    headers: Vec<SegHeader>,
    /// Read handles per segment, indexed by segment number.
    readers: Vec<Arc<File>>,
    dicts: HashMap<[u8; 32], Arc<Vec<u8>>>,
    active: u32,
    cur_off: u32,
    active_f: File,
    dedup: DedupTable,
    /// A failed or short write makes the in-memory `cur_off` unreliable; every further append refuses
    /// until a reopen re-derives the tail by scanning.
    poisoned: bool,
    scratch: Vec<u8>,
    _lock: File,
}

impl Fold {
    /// Open (or create) a fold with no external commit authority — the fold's own frame chain is the
    /// only truth about where good data ends.
    pub fn open(dir: &Path, cfg: FoldCfg) -> Result<Fold> {
        Fold::open_at(dir, cfg, None)
    }

    /// Open, recovering to `committed`: the tail some higher layer has durably recorded.
    ///
    /// Two layers answer two different questions. The self-scan answers *"where does my frame chain
    /// stop being valid?"*. The committed tail answers *"where did the store promise it stopped?"*. If
    /// the committed tail is **beyond** the last good frame, the disk broke a promise and we refuse
    /// rather than serve a fold that silently lost durable bytes.
    pub fn open_at(dir: &Path, cfg: FoldCfg, committed: Option<FoldTail>) -> Result<Fold> {
        if (cfg.seg_max as u64) > SEG_MAX_LIMIT {
            bail!("seg_max {} exceeds the {} format bound (Loc.off is u32)", cfg.seg_max, SEG_MAX_LIMIT);
        }
        std::fs::create_dir_all(dir).with_context(|| format!("create fold dir {}", dir.display()))?;
        let lock = acquire_writer_lock(dir)?;

        // sweep torn staging files before they can be mistaken for anything
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
                dedup: DedupTable::new(),
                poisoned: false,
                scratch: Vec::new(),
                _lock: lock,
            });
        }

        // Segment numbers must be exactly 0..N. A gap means a segment was lost, and every Loc pointing
        // into it would silently mis-resolve — refuse rather than serve.
        nums.sort_unstable();
        for (i, n) in nums.iter().enumerate() {
            if *n != i as u32 {
                bail!("fold segments are not dense: expected seg {i}, found {n}");
            }
        }

        // Validate headers. A damaged NON-active segment is corruption of sealed history. A damaged
        // ACTIVE segment shorter than its header provably holds no durable frame (create fsyncs the
        // header first), so it can only be a torn create — remove it and fall back.
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

        // Load every dictionary a segment names, verifying it against the id that names it.
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

        // LAYER 1 — always: where does the frame chain stop being valid?
        let good_tail = segment::scan_tail(&active_f, flen, has_dict)?;

        // LAYER 2 — only when a commit authority exists.
        let target = match committed {
            None => good_tail,
            Some(ct) => {
                if (ct.seg, ct.off as u64) > (active, good_tail) {
                    bail!(
                        "committed fold tail (seg {}, off {}) is beyond the last good frame (seg {}, off {}) \
                         — the fold lost durable bytes",
                        ct.seg, ct.off, active, good_tail
                    );
                }
                // discard segments entirely past the commit point
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
            dedup: DedupTable::new(),
            poisoned: false,
            scratch: Vec::new(),
            _lock: lock,
        })
    }

    /// Append `raw`, or return the existing location if this content is already folded.
    ///
    /// Does not fsync — see the module contract. The returned `Loc` is not durable until [`sync`].
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

        // Encode, then roll if it will not fit — and re-encode after a roll, because the first frame of
        // a segment must be in that segment's own codec (a frame whose codec disagrees with its
        // segment's dictionary is unreadable).
        let mut dict = self.active_dict();
        let (mut tag, mut payload) = codec::encode(raw, dict.as_deref().map(|v| &v[..]))?;
        let mut frame_len = frame::FRAME_OVERHEAD as u64 + payload.len() as u64;

        if self.cur_off as u64 > SEG_HDR_LEN
            && self.cur_off as u64 + frame_len > self.cfg.seg_max as u64
        {
            self.roll()?;
            dict = self.active_dict();
            let re = codec::encode(raw, dict.as_deref().map(|v| &v[..]))?;
            tag = re.0;
            payload = re.1;
            frame_len = frame::FRAME_OVERHEAD as u64 + payload.len() as u64;
        }

        let off = self.cur_off;
        let n = frame::encode(&mut self.scratch, tag, raw.len() as u32, &payload, &hash);
        debug_assert_eq!(n as u64, frame_len);

        if let Err(e) = self.active_f.write_all_at(&self.scratch[..n], off as u64) {
            self.poisoned = true;
            return Err(anyhow::Error::new(e).context("fold append failed; fold poisoned"));
        }

        let loc = Loc { seg: self.active, off, stored: payload.len() as u32, raw: raw.len() as u32 };
        self.cur_off = off + n as u32;
        self.dedup.insert(hash, loc);
        Ok(Put { hash, loc, deduped: false })
    }

    /// Read one piece back, exactly as it was written.
    pub fn read(&self, loc: Loc) -> Result<Vec<u8>> {
        let seg = loc.seg as usize;
        let f = self
            .readers
            .get(seg)
            .ok_or_else(|| anyhow::anyhow!("Loc names segment {} which does not exist", loc.seg))?;
        if (loc.off as u64) < SEG_HDR_LEN {
            bail!("Loc offset {} is inside the segment header", loc.off);
        }
        let span = loc.frame_len() as usize;
        let mut buf = vec![0u8; span];
        f.read_exact_at(&mut buf, loc.off as u64)
            .with_context(|| format!("read frame at seg {} off {}", loc.seg, loc.off))?;
        let has_dict = self.headers[seg].has_dict();
        let hdr = frame::verify_frame_bytes(&buf, has_dict)?;
        if hdr.raw != loc.raw || hdr.stored != loc.stored {
            bail!(
                "frame at seg {} off {} declares raw/stored {}/{} but the Loc says {}/{}",
                loc.seg, loc.off, hdr.raw, hdr.stored, loc.raw, loc.stored
            );
        }
        let dict = self.dicts.get(&self.headers[seg].dict_id).cloned();
        let payload = &buf[frame::FRAME_HDR_LEN..frame::FRAME_HDR_LEN + hdr.stored as usize];
        let out = codec::decode(hdr.codec, payload, hdr.raw, dict.as_deref().map(|v| &v[..]))?;
        // The frame's 16-bit content prefix is a free self-check that the decode produced the bytes
        // this frame was written for. It filters; it never concludes identity.
        let h = blake3::hash(&out);
        if h.as_bytes()[0..2] != hdr.h16 {
            bail!("decoded content does not match the frame's content prefix (seg {} off {})", loc.seg, loc.off);
        }
        Ok(out)
    }

    /// Read and confirm full content identity — the caller knows what hash it expects.
    pub fn read_verified(&self, loc: Loc, expect: PieceHash) -> Result<Vec<u8>> {
        let out = self.read(loc)?;
        let got = PieceHash::of(&out);
        if got != expect {
            bail!("content hash mismatch at seg {} off {}: got {got}, expected {expect}", loc.seg, loc.off);
        }
        Ok(out)
    }

    /// Make everything appended so far durable and return the tail. Data before pointers: no part may
    /// name a `Loc` at or beyond a tail this has not returned.
    pub fn sync(&mut self) -> Result<FoldTail> {
        self.active_f.sync_all().context("fsync active fold segment")?;
        Ok(self.tail())
    }

    /// The current (not necessarily durable) append point.
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

    /// Distinct pieces held in the unsealed dedup window.
    pub fn window_len(&self) -> usize {
        self.dedup.len()
    }

    /// Release the dedup window — called once the pieces it covers are sealed into a part, so resident
    /// memory tracks the flush interval rather than the store.
    pub fn seal_window(&mut self) {
        self.dedup.clear();
    }

    pub fn segment_count(&self) -> u32 {
        self.headers.len() as u32
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Total bytes across all segment files.
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
        self.active_f.sync_all().context("fsync before roll")?;
        let next = self
            .active
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("segment number space exhausted"))?;
        // No dictionary training yet: a new segment inherits "no dictionary". The codec seam and the
        // header field are already in place for when it arrives.
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
