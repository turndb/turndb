//! The store: WAL, memtable, flush, manifest, recovery — the layer that turns a fold and some parts
//! into a database.
//!
//! # Substrate
//!
//! A store is a **directory**, and reading one requires nothing but the files. There is no daemon in
//! this design; a server is a *role* a process takes when it holds the writer lock, not a thing the
//! format depends on. [`Store::open_read`] takes no lock, replays nothing, and is safe to run
//! concurrently with a writer — parts are immutable and the fold is append-only, so a reader pinned to
//! a manifest sees a consistent store with no coordination at all.
//!
//! # The commit point
//!
//! The manifest is the only one. It names the live parts, the fold tail, and the log cursor;
//! everything else — the block directory, the dedup index, part contents — is derived. It is written
//! tmp + fsync + rename + fsync-dir, so a crash either sees the old manifest or the new one.
//!
//! # Ordering, and why recovery is simple
//!
//! ```text
//! put    -> fold.put (no fsync)  +  WAL append
//! sync   -> WAL fsync                        <- the ACK point
//! flush  -> fold.sync -> write part -> commit manifest -> truncate WAL
//! ```
//! Recovery does not try to work out how far the fold got. It **truncates the fold to the tail the
//! manifest committed** and replays the log, which carries the bytes of every piece that was new.
//! Anything the fold wrote past that tail is discarded and regenerated, so there is no window in
//! which a part could reference content that never landed.

pub mod wal;

use crate::fold::{Fold, FoldCfg, FoldTail, Loc};
use crate::part::cache::SectionCache;
use crate::part::{self, Part};
use crate::types::{AttrValue, BodyOp, PieceHash, Record};
use std::collections::HashMap;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wal::Wal;

/// A carved span handed to the store: content to fold, or bytes to inline.
pub enum Span<'a> {
    /// Connective tissue too small to be worth folding.
    Lit(&'a [u8]),
    /// Content — deduped by identity across the whole store.
    Piece(&'a [u8]),
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct PartRef {
    pub file: String,
    pub seq_lo: u64,
    pub seq_hi: u64,
    pub records: u32,
}

/// The committed state of the store. Small, atomic, and the only source of truth about what is live.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub parts: Vec<PartRef>,
    pub fold_seg: u32,
    pub fold_off: u32,
    pub next_seq: u64,
}

impl Manifest {
    /// A MISSING manifest is a new store. An UNREADABLE one is an error.
    ///
    /// These were conflated, and the orphan sweep made the conflation destructive: a transient EACCES
    /// or EIO yielded an empty manifest, and the sweep then unlinked every part it did not name. One
    /// unreadable byte turned a live store into an empty directory.
    fn load(dir: &Path) -> Result<Manifest> {
        match std::fs::read(dir.join("MANIFEST")) {
            Ok(b) => Ok(serde_json::from_slice(&b).context("corrupt MANIFEST")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
            Err(e) => Err(anyhow::Error::new(e).context(format!(
                "cannot read {} — refusing to treat an unreadable manifest as an empty store",
                dir.join("MANIFEST").display()
            ))),
        }
    }

    /// tmp + fsync + rename + fsync-dir: a crash sees either the old manifest or the new one.
    fn commit(&self, dir: &Path) -> Result<()> {
        let tmp = dir.join("MANIFEST.tmp");
        let f = File::create(&tmp)?;
        serde_json::to_writer(&f, self)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, dir.join("MANIFEST"))?;
        File::open(dir)?.sync_all()?;
        Ok(())
    }

    fn fold_tail(&self) -> Option<FoldTail> {
        if self.next_seq == 0 && self.parts.is_empty() && self.fold_off == 0 {
            None
        } else {
            Some(FoldTail { seg: self.fold_seg, off: self.fold_off })
        }
    }
}

pub struct Store {
    dir: PathBuf,
    fold: Fold,
    parts: Vec<Arc<Part>>,
    manifest: Manifest,
    /// Uncommitted records, last-write-wins by id.
    /// Uncommitted records. `None` is a staged DELETION — it must be a value rather than an absence,
    /// because it has to shadow whatever older parts still say about the id.
    mem: BTreeMap<String, Option<Record>>,
    mem_bytes: usize,
    wal: Wal,
    cfg: FoldCfg,
    /// ONE budget for every part in this store, not one per part. Section caches are what make a
    /// whole-part walk linear, so they cannot be removed — but unbounded they pinned 9.5x each part's
    /// on-disk size, which is a per-part cost that multiplies by part count.
    pcache: Arc<SectionCache>,
}

impl Store {
    /// Open for writing. Takes the writer lock (through the fold) and recovers.
    pub fn open(dir: &Path, cfg: FoldCfg) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        let manifest = Manifest::load(dir)?;

        // Recovery is a truncate, not a negotiation: whatever the fold wrote past the committed tail
        // is discarded, and the log regenerates it.
        let mut fold = Fold::open_at(&dir.join("fold"), cfg, manifest.fold_tail())?;

        let pcache = SectionCache::shared();
        let mut parts = Vec::with_capacity(manifest.parts.len());
        for p in &manifest.parts {
            parts.push(Arc::new(Part::open_in(&dir.join(&p.file), pcache.clone())?));
        }

        // A part file the manifest does not name was written by a flush or merge that crashed before
        // committing, or superseded by a merge that committed. Either way it is unreachable. Safe to
        // unlink even with readers attached: Unix keeps their open mappings alive.
        let live: std::collections::HashSet<&str> = manifest.parts.iter().map(|p| p.file.as_str()).collect();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("part-") && n.ends_with(".part") && !live.contains(n.as_str()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }

        let wal_path = dir.join("WAL");
        let frames = Wal::replay(&wal_path)?;
        let mut mem: BTreeMap<String, Option<Record>> = BTreeMap::new();
        let mut mem_bytes = 0usize;
        for f in frames {
            // Re-fold every piece this frame introduced. Content already below the committed tail
            // dedups; content discarded by the truncate is written again.
            for (h, bytes) in &f.novel {
                let put = fold.put_hashed(bytes, *h)?;
                debug_assert_eq!(put.hash, *h);
            }
            mem_bytes += approx_bytes(&f.record);
            if f.tomb {
                mem.insert(f.record.id, None);
            } else {
                mem.insert(f.record.id.clone(), Some(f.record));
            }
        }
        let wal = Wal::open(&wal_path)?;

        Ok(Store { dir: dir.to_path_buf(), fold, parts, manifest, mem, mem_bytes, wal, cfg, pcache })
    }

    /// Open for reading only: no lock, no replay, no daemon.
    ///
    /// Sees exactly the committed manifest — uncommitted records in some writer's memtable are
    /// invisible, which is the correct snapshot. Safe alongside a live writer because parts are
    /// immutable and the fold is append-only.
    pub fn open_read(dir: &Path, cfg: FoldCfg) -> Result<ReadStore> {
        let fold = Fold::open_read(&dir.join("fold"), cfg)?;

        // Reading the manifest and opening the parts it names is not atomic, and a writer may commit a
        // merge and unlink the replaced inputs in between. The manifest IS the linearization point, so
        // the fix is simply to start over: a re-read gets the newer manifest, whose parts exist. An
        // already-open part is unaffected — Unix keeps it alive through the unlink — so this window is
        // only ever about parts not yet opened.
        //
        // Bounded, because a manifest naming a genuinely absent part must eventually surface as an
        // error rather than spin.
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..8 {
            let manifest = Manifest::load(dir)?;
            let pcache = SectionCache::shared();
            let mut parts = Vec::with_capacity(manifest.parts.len());
            let mut missed = false;
            for p in &manifest.parts {
                match Part::open_in(&dir.join(&p.file), pcache.clone()) {
                    Ok(part) => parts.push(Arc::new(part)),
                    Err(e) => {
                        let gone = e
                            .downcast_ref::<std::io::Error>()
                            .map(|io| io.kind() == std::io::ErrorKind::NotFound)
                            .unwrap_or(false)
                            || !dir.join(&p.file).exists();
                        if !gone {
                            return Err(e);
                        }
                        last = Some(e);
                        missed = true;
                        break;
                    }
                }
            }
            if !missed {
                return Ok(ReadStore { fold, parts, manifest, pcache });
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("manifest names a part that does not exist")))
    }

    /// Resolve one piece of content to a location, consulting both dedup tiers before appending.
    ///
    /// ```text
    ///   Tier 0   the fold's in-memory window   — this flush's pieces, no I/O
    ///   Tier 1   every live part's dictionary  — everything ever committed, filter then search
    ///   append   genuinely novel content
    /// ```
    ///
    /// Tier 1 is what makes dedup **unbounded** while Tier 0 stays bounded: the window is released at
    /// every flush (see [`Store::flush`]), so resident dedup memory tracks the flush interval rather
    /// than the store, and Tier 1 is what keeps that from costing any dedup at all.
    ///
    /// Parts are consulted newest-first: recently written content is the content most likely to repeat.
    ///
    /// **Why a Tier-1 hit needs no WAL bytes.** A part is only named by the manifest after its content
    /// was durable, and the committed fold tail only grows — so any location reachable through a part's
    /// dictionary already sits below the tail that recovery truncates to. The bytes cannot be the ones
    /// a crash discards.
    fn fold_piece(&mut self, b: &[u8]) -> Result<crate::fold::Put> {
        let hash = PieceHash::of(b);
        if let Some(loc) = self.locate(&hash)? {
            // Seed the window so further references in this flush interval answer from memory.
            self.fold.note(hash, loc);
            return Ok(crate::fold::Put { hash, loc, deduped: true });
        }
        self.fold.put_hashed(b, hash)
    }

    /// Fold the spans, log the record, and stage it. Durable only after [`sync`].
    pub fn put(&mut self, id: &str, spans: &[Span], attrs: Vec<(String, AttrValue)>) -> Result<()> {
        let mut body = Vec::with_capacity(spans.len());
        let mut novel = Vec::new();
        for s in spans {
            match s {
                Span::Lit(b) => body.push(BodyOp::Lit(b.to_vec())),
                Span::Piece(b) => {
                    let put = self.fold_piece(b)?;
                    if !put.deduped {
                        // new content: the log must carry the bytes, because recovery discards
                        // anything the fold wrote past the committed tail
                        novel.push((put.hash, b.to_vec()));
                    }
                    body.push(BodyOp::Piece { hash: put.hash, len: b.len() as u32 });
                }
            }
        }
        let rec = Record { id: id.to_string(), body, attrs };
        self.wal.append(self.manifest.next_seq, &rec, &novel)?;
        self.mem_bytes += approx_bytes(&rec);
        self.mem.insert(rec.id.clone(), Some(rec));
        Ok(())
    }

    /// Delete `id`. Durable only after [`sync`], exactly like a put.
    ///
    /// Recorded as a TOMBSTONE rather than by removing anything: older parts are immutable and still
    /// hold the record, so a deletion has to be a newer version that says "absent". Space is not
    /// reclaimed here — the content stays in the fold, which is append-only. Reclaiming it is a
    /// separate, deliberate operation, because the fold is shared and the same bytes may be referenced
    /// by records that are still live.
    pub fn delete(&mut self, id: &str) -> Result<()> {
        self.wal.append_tomb(self.manifest.next_seq, id)?;
        self.mem_bytes += id.len() + 32;
        self.mem.insert(id.to_string(), None);
        Ok(())
    }

    /// The ACK point: everything put so far survives a crash.
    pub fn sync(&mut self) -> Result<()> {
        self.wal.sync()
    }

    /// Seal the memtable into a part and commit it.
    ///
    /// Data before pointers, and the manifest last: the fold is durable before a part names any of
    /// it, and the part is durable before the manifest names the part.
    pub fn flush(&mut self) -> Result<Option<PartRef>> {
        if self.mem.is_empty() {
            return Ok(None);
        }
        let tail = self.fold.sync()?;
        let seq = self.manifest.next_seq + 1;
        let file = format!("part-{seq:08}.part");
        let path = self.dir.join(&file);
        let mut recs: Vec<Record> = Vec::with_capacity(self.mem.len());
        let mut tombs: Vec<bool> = Vec::with_capacity(self.mem.len());
        for (id, v) in &self.mem {
            match v {
                Some(r) => {
                    recs.push(r.clone());
                    tombs.push(false);
                }
                // A tombstone still needs a row, so it gets an empty one carrying only its id.
                None => {
                    recs.push(Record { id: id.clone(), body: Vec::new(), attrs: Vec::new() });
                    tombs.push(true);
                }
            }
        }

        // Resolve every referenced piece through BOTH tiers, exactly as the write path does.
        //
        // A Tier-0-only resolve is correct only while the process that staged the records is still
        // alive: `fold_piece` notes a Tier-1 hit into the window, so the window covers it. After a
        // CRASH it does not. Replay re-folds only pieces the WAL carried bytes for, and a Tier-1 hit
        // carries none by design — the content was already durable in an older part. Those pieces are
        // then in no window at all, and a Tier-0-only resolve would fail here on every subsequent
        // flush attempt, permanently: records unreadable, WAL growing without bound. On the
        // high-duplication corpora this engine exists for, that is nearly every record after the
        // first flush.
        let mut locs: HashMap<PieceHash, Loc> = HashMap::new();
        for r in &recs {
            for op in &r.body {
                let BodyOp::Piece { hash, .. } = op else { continue };
                if locs.contains_key(hash) {
                    continue;
                }
                let loc = self.locate(hash)?.ok_or_else(|| {
                    anyhow::anyhow!("staged piece {hash} is in neither the fold window nor any live part")
                })?;
                locs.insert(*hash, loc);
            }
        }
        let meta = part::build_full(
            &path, &recs, &tombs, seq, seq, self.cfg.level,
            |h| locs.get(h).copied(), &HashMap::new(),
        )?;

        let mut m = self.manifest.clone();
        m.parts.push(PartRef { file: file.clone(), seq_lo: seq, seq_hi: seq, records: meta.n_records });
        m.fold_seg = tail.seg;
        m.fold_off = tail.off;
        m.next_seq = seq;
        m.commit(&self.dir)?; // <- the linearization point

        self.parts.push(Arc::new(Part::open_in(&path, self.pcache.clone())?));
        self.manifest = m;
        self.mem.clear();
        self.mem_bytes = 0;
        // Release Tier 0 — but only HERE, after the part is committed and open. Sealing any earlier
        // would drop the window while the part being built still needs it, and the part cannot answer
        // a Tier-1 lookup until it is committed and in `self.parts`. Everything the window covered is
        // now reachable through that part's dictionary, so nothing is lost but the memory.
        self.fold.seal_window();
        // Only now: the records are in a committed part, so the log that carried them is redundant.
        self.wal.truncate()?;
        Ok(self.manifest.parts.last().cloned())
    }

    /// Merge a CONTIGUOUS run of live parts into one, and publish it atomically.
    ///
    /// Contiguity is the correctness gate: parts resolve versions by sequence, so merging a
    /// non-adjacent set would drop whatever an excluded part said about a shared id. The range is
    /// therefore expressed as a slice of the live list, which cannot express a gap.
    pub fn merge_range(&mut self, lo: usize, len: usize) -> Result<Option<crate::part::merge::MergeStats>> {
        if len < 2 || lo + len > self.parts.len() {
            return Ok(None);
        }
        let inputs: Vec<Arc<Part>> = self.parts[lo..lo + len].to_vec();
        // Named by the sequence RANGE it spans. The output's range strictly contains every input's
        // (the inputs are disjoint and there are at least two), so the name cannot collide with a part
        // this merge is about to replace — which the post-commit sweep would otherwise unlink.
        let seq_lo = self.manifest.parts[lo].seq_lo;
        let seq_hi = self.manifest.parts[lo + len - 1].seq_hi;
        let file = format!("part-{seq_lo:08}-{seq_hi:08}.part");
        debug_assert!(
            !self.manifest.parts.iter().any(|p| p.file == file),
            "merge output {file} collides with a live part"
        );
        let path = self.dir.join(&file);
        // A tombstone may only be discarded when this merge covers the ENTIRE live list — otherwise a
        // part outside the run could still hold an older version of the deleted id, and dropping the
        // tombstone would resurrect it.
        let total = lo == 0 && len == self.parts.len();
        let (meta, stats) =
            crate::part::merge::merge_opts(&path, &inputs, self.cfg.level, total)?;

        // Publish: the merged part is durable (part::build fsyncs) before the manifest names it, and
        // the manifest swap is the single linearization point. A crash before it leaves the merged
        // file as an unreachable orphan; a crash after it leaves the inputs as orphans. Both are swept.
        let mut m = self.manifest.clone();
        let replaced: Vec<PartRef> = m.parts.splice(
            lo..lo + len,
            [PartRef {
                file: file.clone(),
                seq_lo: meta.seq_lo,
                seq_hi: meta.seq_hi,
                records: meta.n_records,
            }],
        ).collect();
        m.commit(&self.dir)?;

        self.parts.splice(lo..lo + len, [Arc::new(Part::open_in(&path, self.pcache.clone())?)]);
        self.manifest = m;
        for r in replaced {
            let _ = std::fs::remove_file(self.dir.join(&r.file));
        }
        Ok(Some(stats))
    }

    /// Size-tiered compaction: when parts pile up, fold the oldest run together.
    ///
    /// Merging the OLDEST parts keeps the run contiguous by construction and matches the access
    /// pattern — old parts are cold and stop being rewritten. Bounding part count is not only about
    /// read amplification: a Tier-1 dedup lookup is O(parts), so this is what keeps global dedup
    /// affordable.
    pub fn maybe_compact(&mut self, trigger: usize, run: usize) -> Result<Option<crate::part::merge::MergeStats>> {
        if self.parts.len() < trigger {
            return Ok(None);
        }
        self.merge_range(0, run.min(self.parts.len()))
    }

    /// Newest-wins across the committed parts, then the memtable, which is newer than all of them.
    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        if let Some(v) = self.mem.get(id) {
            return Ok(v.clone());
        }
        newest(&self.parts, id)
    }

    /// Byte-exact content for `id`.
    pub fn reconstruct(&self, id: &str) -> Result<Option<Vec<u8>>> {
        if let Some(v) = self.mem.get(id) {
            return match v {
                Some(r) => Ok(Some(self.rebuild(r)?)),
                None => Ok(None), // staged deletion
            };
        }
        for p in self.parts.iter().rev() {
            if let Some(row) = p.find(id)? {
                // The NEWEST part holding the id decides, and if it says deleted the answer is
                // absent — older parts still holding it are superseded, not consulted.
                if p.is_tombstone(row)? {
                    return Ok(None);
                }
                return Ok(Some(p.reconstruct(row, &self.fold)?));
            }
        }
        Ok(None)
    }

    /// Where content lives, through BOTH dedup tiers.
    ///
    /// The single answer to "where is this piece" for every caller that needs one — the write path,
    /// the flush path, and the staged-record read path. They disagreed before, and each disagreement
    /// was the same bug wearing a different hat: a piece deduped against a committed part is not in
    /// the in-memory window, and after a crash nothing puts it back there, because the WAL carries no
    /// bytes for content that was already durable.
    fn locate(&self, h: &PieceHash) -> Result<Option<Loc>> {
        if let Some(l) = self.fold.lookup(*h) {
            return Ok(Some(l));
        }
        for p in self.parts.iter().rev() {
            if let Some(l) = p.lookup_piece(h)? {
                return Ok(Some(l));
            }
        }
        Ok(None)
    }

    fn rebuild(&self, r: &Record) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for op in &r.body {
            match op {
                BodyOp::Lit(b) => out.extend_from_slice(b),
                BodyOp::Piece { hash, .. } => {
                    let loc = self
                        .locate(hash)?
                        .ok_or_else(|| anyhow::anyhow!("piece {hash} not resolvable"))?;
                    out.extend_from_slice(&self.fold.read_verified(loc, *hash)?);
                }
            }
        }
        Ok(out)
    }

    pub fn memtable_len(&self) -> usize {
        self.mem.len()
    }
    pub fn memtable_bytes(&self) -> usize {
        self.mem_bytes
    }
    /// Hand the fold and parts to a lens. Consumes the store because the query layer takes ownership
    /// of both; the manifest snapshot it was opened at is already baked into `parts`.
    pub fn into_parts(self) -> (Arc<Fold>, Vec<Arc<Part>>) {
        (Arc::new(self.fold), self.parts)
    }

    pub fn part_count(&self) -> usize {
        self.parts.len()
    }

    /// Every live id: committed parts plus the uncommitted memtable.
    ///
    /// Includes staged records, unlike [`ReadStore::ids`], because a writer can see its own writes.
    pub fn ids(&self) -> Result<Vec<String>> {
        let mut all: Vec<String> = self.mem.iter().filter(|(_, v)| v.is_some()).map(|(k, _)| k.clone()).collect();
        for p in &self.parts {
            all.extend(live_ids(p)?);
        }
        all.sort();
        all.dedup();
        // An id deleted in a newer part or staged as deleted must not appear because an older part
        // still lists it.
        all.retain(|id| match self.mem.get(id) {
            Some(None) => false,
            Some(Some(_)) => true,
            None => newest_exists(&self.parts, id).unwrap_or(true),
        });
        Ok(all)
    }

    /// Bytes pinned by every open part's section caches, against their shared budget.
    pub fn part_cache_bytes(&self) -> (usize, usize) {
        (self.pcache.bytes(), self.pcache.budget())
    }

    /// Pieces resident in the Tier-0 dedup window. Bounded by the flush interval, not by store size.
    pub fn dedup_window_len(&self) -> usize {
        self.fold.window_len()
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn fold(&self) -> &Fold {
        &self.fold
    }
    pub fn wal_bytes(&self) -> u64 {
        self.wal.bytes()
    }
}

/// A reader over the committed state. No lock, no writer, no daemon.
pub struct ReadStore {
    fold: Fold,
    parts: Vec<Arc<Part>>,
    manifest: Manifest,
    pcache: Arc<SectionCache>,
}

impl ReadStore {
    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        newest(&self.parts, id)
    }

    pub fn reconstruct(&self, id: &str) -> Result<Option<Vec<u8>>> {
        for p in self.parts.iter().rev() {
            if let Some(row) = p.find(id)? {
                if p.is_tombstone(row)? {
                    return Ok(None);
                }
                return Ok(Some(p.reconstruct(row, &self.fold)?));
            }
        }
        Ok(None)
    }

    /// Distinct committed ids, sorted — the union across parts, newest-wins.
    pub fn ids(&self) -> Result<Vec<String>> {
        let mut all: Vec<String> = Vec::new();
        for p in &self.parts {
            all.extend(live_ids(p)?);
        }
        all.sort();
        all.dedup();
        all.retain(|id| newest_exists(&self.parts, id).unwrap_or(true));
        Ok(all)
    }

    /// Hand the fold and parts to a lens. Consumes the store because the query layer takes ownership
    /// of both; the manifest snapshot it was opened at is already baked into `parts`.
    pub fn into_parts(self) -> (Arc<Fold>, Vec<Arc<Part>>) {
        (Arc::new(self.fold), self.parts)
    }

    pub fn part_count(&self) -> usize {
        self.parts.len()
    }
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

/// Later parts hold later sequence numbers, so the last hit wins.
/// Ids a part lists that it does not itself delete.
fn live_ids(p: &Arc<Part>) -> Result<Vec<String>> {
    let tombs = p.tombstones()?;
    if tombs.is_empty() {
        return p.ids();
    }
    let ids = p.ids()?;
    Ok(ids
        .into_iter()
        .enumerate()
        .filter(|(i, _)| tombs.binary_search(&(*i as u64)).is_err())
        .map(|(_, id)| id)
        .collect())
}

/// Does the NEWEST part holding `id` say it exists?
fn newest_exists(parts: &[Arc<Part>], id: &str) -> Result<bool> {
    for p in parts.iter().rev() {
        if let Some(row) = p.find(id)? {
            return Ok(!p.is_tombstone(row)?);
        }
    }
    Ok(false)
}

/// The newest committed version of `id`, or `None` if the newest one is a deletion.
fn newest(parts: &[Arc<Part>], id: &str) -> Result<Option<Record>> {
    for p in parts.iter().rev() {
        if let Some(row) = p.find(id)? {
            if p.is_tombstone(row)? {
                return Ok(None);
            }
            return Ok(Some(p.record(row)?));
        }
    }
    Ok(None)
}

fn approx_bytes(r: &Record) -> usize {
    r.id.len()
        + r.body
            .iter()
            .map(|o| match o {
                BodyOp::Lit(b) => b.len() + 8,
                BodyOp::Piece { .. } => 40,
            })
            .sum::<usize>()
        + r.attrs.iter().map(|(k, _)| k.len() + 24).sum::<usize>()
}

/// Reject a store directory that is obviously not one, before doing anything destructive.
pub fn looks_like_store(dir: &Path) -> bool {
    dir.join("MANIFEST").exists() || dir.join("fold").exists()
}

#[cfg(not(unix))]
compile_error!("turndb requires a Unix filesystem (flock, positioned reads, mmap-survives-unlink)");

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_roundtrips() {
        let d = std::env::temp_dir().join(format!("turndb-man-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let m = super::Manifest {
            parts: vec![super::PartRef { file: "p.part".into(), seq_lo: 1, seq_hi: 1, records: 7 }],
            fold_seg: 2,
            fold_off: 4096,
            next_seq: 9,
        };
        m.commit(&d).unwrap();
        let got = super::Manifest::load(&d).unwrap();
        assert_eq!(got.parts.len(), 1);
        assert_eq!(got.fold_off, 4096);
        assert_eq!(got.next_seq, 9);
        assert!(!d.join("MANIFEST.tmp").exists(), "staging file must not survive a commit");
        std::fs::remove_dir_all(&d).ok();
    }
}
