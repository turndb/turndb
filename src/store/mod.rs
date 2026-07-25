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

use crate::fold::{Fold, FoldCfg, FoldTail};
use crate::part::{self, Part};
use crate::types::{AttrValue, BodyOp, PieceHash, Record};
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
    fn load(dir: &Path) -> Result<Manifest> {
        match std::fs::read(dir.join("MANIFEST")) {
            Ok(b) => Ok(serde_json::from_slice(&b).context("corrupt MANIFEST")?),
            Err(_) => Ok(Manifest::default()),
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
    mem: BTreeMap<String, Record>,
    mem_bytes: usize,
    wal: Wal,
    cfg: FoldCfg,
}

impl Store {
    /// Open for writing. Takes the writer lock (through the fold) and recovers.
    pub fn open(dir: &Path, cfg: FoldCfg) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        let manifest = Manifest::load(dir)?;

        // Recovery is a truncate, not a negotiation: whatever the fold wrote past the committed tail
        // is discarded, and the log regenerates it.
        let mut fold = Fold::open_at(&dir.join("fold"), cfg, manifest.fold_tail())?;

        let mut parts = Vec::with_capacity(manifest.parts.len());
        for p in &manifest.parts {
            parts.push(Arc::new(Part::open(&dir.join(&p.file))?));
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
        let mut mem = BTreeMap::new();
        let mut mem_bytes = 0usize;
        for f in frames {
            // Re-fold every piece this frame introduced. Content already below the committed tail
            // dedups; content discarded by the truncate is written again.
            for (h, bytes) in &f.novel {
                let put = fold.put_hashed(bytes, *h)?;
                debug_assert_eq!(put.hash, *h);
            }
            mem_bytes += approx_bytes(&f.record);
            mem.insert(f.record.id.clone(), f.record);
        }
        let wal = Wal::open(&wal_path)?;

        Ok(Store { dir: dir.to_path_buf(), fold, parts, manifest, mem, mem_bytes, wal, cfg })
    }

    /// Open for reading only: no lock, no replay, no daemon.
    ///
    /// Sees exactly the committed manifest — uncommitted records in some writer's memtable are
    /// invisible, which is the correct snapshot. Safe alongside a live writer because parts are
    /// immutable and the fold is append-only.
    pub fn open_read(dir: &Path, cfg: FoldCfg) -> Result<ReadStore> {
        let manifest = Manifest::load(dir)?;
        let fold = Fold::open_read(&dir.join("fold"), cfg)?;
        let mut parts = Vec::with_capacity(manifest.parts.len());
        for p in &manifest.parts {
            parts.push(Arc::new(Part::open(&dir.join(&p.file))?));
        }
        Ok(ReadStore { fold, parts, manifest })
    }

    /// Resolve one piece of content to a location, consulting both dedup tiers before appending.
    ///
    /// ```text
    ///   Tier 0   the fold's in-memory window   — this flush's pieces, no I/O
    ///   Tier 1   every live part's dictionary  — everything ever committed, filter then search
    ///   append   genuinely novel content
    /// ```
    ///
    /// Tier 1 is what makes dedup **unbounded**: Tier 0 is bounded by the flush interval by design (it
    /// is released at every flush so resident memory tracks the interval, not the store), and without
    /// Tier 1 the same content re-appended after a flush would be stored twice.
    ///
    /// Parts are consulted newest-first: recently written content is the content most likely to repeat.
    ///
    /// **Why a Tier-1 hit needs no WAL bytes.** A part is only named by the manifest after its content
    /// was durable, and the committed fold tail only grows — so any location reachable through a part's
    /// dictionary already sits below the tail that recovery truncates to. The bytes cannot be the ones
    /// a crash discards.
    fn fold_piece(&mut self, b: &[u8]) -> Result<crate::fold::Put> {
        let hash = PieceHash::of(b);
        if let Some(loc) = self.fold.lookup(hash) {
            return Ok(crate::fold::Put { hash, loc, deduped: true });
        }
        let mut found = None;
        for p in self.parts.iter().rev() {
            if let Some(loc) = p.lookup_piece(&hash)? {
                found = Some(loc);
                break;
            }
        }
        if let Some(loc) = found {
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
        self.mem.insert(rec.id.clone(), rec);
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
        let recs: Vec<Record> = self.mem.values().cloned().collect();
        let fold = &self.fold;
        let meta = part::build(&path, &recs, seq, seq, self.cfg.level, |h| fold.lookup(*h))?;

        let mut m = self.manifest.clone();
        m.parts.push(PartRef { file: file.clone(), seq_lo: seq, seq_hi: seq, records: meta.n_records });
        m.fold_seg = tail.seg;
        m.fold_off = tail.off;
        m.next_seq = seq;
        m.commit(&self.dir)?; // <- the linearization point

        self.parts.push(Arc::new(Part::open(&path)?));
        self.manifest = m;
        self.mem.clear();
        self.mem_bytes = 0;
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
        let seq_hi = self.manifest.parts[lo + len - 1].seq_hi;
        let file = format!("part-{seq_hi:08}-m{len}.part");
        let path = self.dir.join(&file);
        let (meta, stats) = crate::part::merge::merge(&path, &inputs, self.cfg.level)?;

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

        self.parts.splice(lo..lo + len, [Arc::new(Part::open(&path)?)]);
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
        if let Some(r) = self.mem.get(id) {
            return Ok(Some(r.clone()));
        }
        newest(&self.parts, id)
    }

    /// Byte-exact content for `id`.
    pub fn reconstruct(&self, id: &str) -> Result<Option<Vec<u8>>> {
        if let Some(r) = self.mem.get(id) {
            return Ok(Some(self.rebuild(r)?));
        }
        for p in self.parts.iter().rev() {
            if let Some(row) = p.find(id)? {
                return Ok(Some(p.reconstruct(row, &self.fold)?));
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
                        .fold
                        .lookup(*hash)
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
}

impl ReadStore {
    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        newest(&self.parts, id)
    }

    pub fn reconstruct(&self, id: &str) -> Result<Option<Vec<u8>>> {
        for p in self.parts.iter().rev() {
            if let Some(row) = p.find(id)? {
                return Ok(Some(p.reconstruct(row, &self.fold)?));
            }
        }
        Ok(None)
    }

    /// Distinct committed ids, sorted — the union across parts, newest-wins.
    pub fn ids(&self) -> Result<Vec<String>> {
        let mut all: Vec<String> = Vec::new();
        for p in &self.parts {
            all.extend(p.ids()?);
        }
        all.sort();
        all.dedup();
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
fn newest(parts: &[Arc<Part>], id: &str) -> Result<Option<Record>> {
    for p in parts.iter().rev() {
        if let Some(row) = p.find(id)? {
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
