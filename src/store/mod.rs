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

pub mod read;
pub mod refold;
pub mod wal;

use crate::fold::{Fold, FoldCfg, FoldTail, Loc};
use crate::part::cache::SectionCache;
use crate::part::{self, Part};
use crate::types::{AttrValue, BodyOp, PieceHash, Record};
use std::collections::{HashMap, HashSet};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
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

/// How many committed manifests are RETAINED beside the live one, as `MANIFEST.<commit>`.
///
/// Retention is what turns the commit point into a log: every file a retained manifest names
/// survives the sweep, so a reader holding any manifest in the window sees its whole snapshot on
/// disk, and a corrupt `MANIFEST` is recoverable by explicit promotion instead of surgery. The
/// window is a count of COMMITS, not time — each flush, merge, or re-fold advances it by one.
pub const MANIFEST_RETAIN: usize = 4;

/// The committed state of the store. Small, atomic, and the only source of truth about what is live.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub parts: Vec<PartRef>,
    /// Which fold generation is live. A re-fold writes a new one and names it here, so the swap IS the
    /// manifest commit. Absent in stores written before re-folding existed, which serde reads as 0 —
    /// the original `fold/` directory, needing no migration.
    #[serde(default)]
    pub fold_gen: u32,
    pub fold_seg: u32,
    pub fold_off: u32,
    pub next_seq: u64,
    /// Monotonic commit counter — the retained log's namespace. `next_seq` cannot serve here: it
    /// only advances at flush, and merges and re-folds commit without flushing. Absent in stores
    /// written before the log existed, which serde reads as 0.
    #[serde(default)]
    pub commit: u64,
}

impl Manifest {
    /// A MISSING manifest is a new store. An UNREADABLE one is an error.
    ///
    /// These were conflated, and the orphan sweep made the conflation destructive: a transient EACCES
    /// or EIO yielded an empty manifest, and the sweep then unlinked every part it did not name. One
    /// unreadable byte turned a live store into an empty directory.
    fn load(dir: &Path) -> Result<Manifest> {
        match std::fs::read(dir.join("MANIFEST")) {
            Ok(b) => Manifest::parse(&b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A missing manifest is a new store — UNLESS a commit log exists, in which case
                // this store has committed before and `MANIFEST` was lost. Opening it as new
                // would be the destructive conflation all over again, one deletion further
                // upstream: an empty manifest followed by the sweep.
                let retained = list_retained(dir);
                if retained.is_empty() {
                    Ok(Manifest::default())
                } else {
                    bail!(
                        "MANIFEST is missing but {} retained commits exist at {} — a damaged \
                         store, not a new one; recover_manifest() can promote the newest intact copy",
                        retained.len(),
                        dir.display()
                    )
                }
            }
            Err(e) => Err(anyhow::Error::new(e).context(format!(
                "cannot read {} — refusing to treat an unreadable manifest as an empty store",
                dir.join("MANIFEST").display()
            ))),
        }
    }

    /// Parse manifest bytes, verifying the checksum trailer when one is present.
    ///
    /// The manifest is the one file whose corruption used to be able to DESTROY data with no error
    /// anywhere: it is parsed JSON, so a flipped bit that still parses — a shortened `fold_off`, a
    /// wrong generation — was believed, and recovery then truncated durable fold bytes to match it.
    /// Every other structure in the store refuses corruption; this closes the last gap.
    ///
    /// A manifest written before the trailer existed is bare compact JSON and is accepted as-is:
    /// the trailer is recognised by SHAPE (a final line `crc32=XXXXXXXX`), which compact JSON cannot
    /// end with. Corruption cannot demote a checksummed manifest to a legacy one either way it
    /// lands: mangling the trailer leaves trailing bytes that JSON parsing refuses, and mangling
    /// the payload fails the checksum.
    fn parse(bytes: &[u8]) -> Result<Manifest> {
        let payload = match checksum_trailer(bytes) {
            Some((payload, want)) => {
                let got = crc32fast::hash(payload);
                if got != want {
                    bail!(
                        "MANIFEST fails its checksum (crc32 {got:08x}, recorded {want:08x}) — \
                         refusing to open from a corrupt commit point"
                    );
                }
                payload
            }
            None => bytes,
        };
        serde_json::from_slice(payload).context("corrupt MANIFEST")
    }

    /// The bytes as committed: compact JSON, then a `\ncrc32=XXXXXXXX` trailer over the JSON.
    fn encode(&self) -> Result<Vec<u8>> {
        let mut buf = serde_json::to_vec(self)?;
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(format!("\ncrc32={crc:08x}").as_bytes());
        Ok(buf)
    }

    /// tmp + fsync + rename + fsync-dir: a crash sees either the old manifest or the new one.
    ///
    /// Bumps the commit counter, and writes the retained copy `MANIFEST.<commit>` BEFORE the
    /// rename: if the live manifest is later corrupted, the copy of the very state it carried is
    /// what recovery promotes. A crash between the copy and the rename leaves a retained manifest
    /// describing a commit that never took effect — which is exactly the old manifest's state plus
    /// a counter bump, and harmless: promotion would reproduce the state the store is already in.
    ///
    /// One directory fsync at the end covers both dirents. Pruning runs last and is best-effort —
    /// a retained manifest that outlives its window is swept space, never a correctness problem.
    fn commit(&mut self, dir: &Path) -> Result<()> {
        self.commit += 1;
        let bytes = self.encode()?;
        {
            let mut f = File::create(retained_path(dir, self.commit))?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        let tmp = dir.join("MANIFEST.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, dir.join("MANIFEST"))?;
        File::open(dir)?.sync_all()?;
        for c in list_retained(dir) {
            if c + (MANIFEST_RETAIN as u64) <= self.commit {
                let _ = std::fs::remove_file(retained_path(dir, c));
            }
        }
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

/// The `(payload, recorded crc32)` of a checksummed manifest, or `None` for one written before the
/// trailer existed. Recognition is by exact shape — a final line `crc32=` plus eight hex digits —
/// so a legacy manifest (compact JSON, which cannot end that way) is never misread as checksummed,
/// and a checksummed one whose trailer is damaged falls through to JSON parsing, which refuses the
/// trailing bytes rather than silently accepting the payload unverified.
fn checksum_trailer(bytes: &[u8]) -> Option<(&[u8], u32)> {
    let pos = bytes.iter().rposition(|&b| b == b'\n')?;
    let tail = &bytes[pos + 1..];
    if tail.len() != 14 || !tail.starts_with(b"crc32=") {
        return None;
    }
    let hex = std::str::from_utf8(&tail[6..]).ok()?;
    let want = u32::from_str_radix(hex, 16).ok()?;
    Some((&bytes[..pos], want))
}

fn retained_path(dir: &Path, commit: u64) -> PathBuf {
    dir.join(format!("MANIFEST.{commit:08}"))
}

/// Retained commits on disk, ascending. Parsed NUMERICALLY, the same rule as segment names:
/// lexicographic order breaks past the padding width. `MANIFEST.tmp` does not match.
fn list_retained(dir: &Path) -> Vec<u64> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix("MANIFEST.") {
                if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                    if let Ok(n) = rest.parse::<u64>() {
                        out.push(n);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out
}

/// The snapshot commits currently available to [`Store::open_read_at`], ascending.
pub fn retained_commits(dir: &Path) -> Vec<u64> {
    list_retained(dir)
}

fn load_retained(dir: &Path, commit: u64) -> Result<Manifest> {
    let p = retained_path(dir, commit);
    let b = std::fs::read(&p)
        .with_context(|| format!("no retained manifest {} — the retention window has moved past it", p.display()))?;
    Manifest::parse(&b).with_context(|| format!("retained manifest {} is corrupt", p.display()))
}

/// Delete every file that no manifest — live or retained — names. THE deletion path: flush, merge,
/// re-fold, and writer open all converge here, so there is exactly one place that decides
/// reachability. A file a retained manifest names is a live snapshot's file and survives; it is
/// swept only when the window prunes past its last naming manifest.
///
/// A retained manifest that fails its checksum (a torn copy from a crash) pins nothing — it can
/// describe no snapshot anyone can open.
fn sweep_unreachable(dir: &Path) -> Result<()> {
    let mut keep: Vec<Manifest> = vec![Manifest::load(dir)?];
    for c in list_retained(dir) {
        if let Ok(m) = load_retained(dir, c) {
            keep.push(m);
        }
    }
    let live_parts: HashSet<&str> =
        keep.iter().flat_map(|m| m.parts.iter().map(|p| p.file.as_str())).collect();
    let live_gens: HashSet<u32> = keep.iter().map(|m| m.fold_gen).collect();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("part-") && n.ends_with(".part") && !live_parts.contains(n.as_str()) {
                let _ = std::fs::remove_file(e.path());
            }
            if e.path().is_dir() {
                if let Some(g) = refold::parse_fold_gen(&n) {
                    if !live_gens.contains(&g) {
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Promote the newest intact retained manifest over a damaged `MANIFEST`.
///
/// EXPLICITLY an operator action, never automatic. In the common case — bit rot in `MANIFEST`
/// itself — the newest retained copy carries the very same commit, and promotion loses nothing.
/// Only when the newest copies are also damaged does promotion become a ROLLBACK to an older
/// commit, discarding acknowledged flushes; an `open()` that silently fell back would make that
/// loss invisible, which is why open refuses and this function exists to be called on purpose.
///
/// Promoting a copy whose rename never landed (a crash inside `commit`) is safe by the same rule
/// as everything else here: data before pointers — everything a manifest names was durable before
/// the manifest was written, so completing the commit is indistinguishable from the crash having
/// happened a moment later.
///
/// Refuses when `MANIFEST` is intact, and when nothing intact remains to promote. Retained copies
/// NEWER than the promoted commit necessarily failed to parse (an intact one would have been
/// promoted instead) and are removed, so the log ends at the commit the store resumes from.
pub fn recover_manifest(dir: &Path) -> Result<u64> {
    if Manifest::load(dir).is_ok() {
        bail!("MANIFEST at {} is intact — refusing to roll back a healthy store", dir.display());
    }
    for c in list_retained(dir).into_iter().rev() {
        if load_retained(dir, c).is_err() {
            continue;
        }
        let bytes = std::fs::read(retained_path(dir, c))?;
        let tmp = dir.join("MANIFEST.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, dir.join("MANIFEST"))?;
        File::open(dir)?.sync_all()?;
        for n in list_retained(dir) {
            if n > c {
                let _ = std::fs::remove_file(retained_path(dir, n));
            }
        }
        return Ok(c);
    }
    bail!("MANIFEST at {} is damaged and no retained manifest is intact", dir.display());
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
        let mut fold = Fold::open_at(&refold::fold_dir(dir, manifest.fold_gen), cfg, manifest.fold_tail())?;

        let pcache = SectionCache::shared();
        let mut parts = Vec::with_capacity(manifest.parts.len());
        for p in &manifest.parts {
            parts.push(Arc::new(Part::open_in(&dir.join(&p.file), pcache.clone())?));
        }

        // A part file or fold generation no manifest names was written by a flush, merge, or
        // re-fold that crashed before committing, or has aged out of the retention window. Either
        // way it is unreachable. Safe to unlink even with readers attached: Unix keeps their open
        // mappings alive.
        sweep_unreachable(dir)?;

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
        // Reading the manifest and opening the fold generation plus parts it names is not atomic. A
        // writer may commit a merge or re-fold and unlink the replaced files in between, or commit a
        // flush whose new part names fold blocks a reader scanned just before they landed. The
        // manifest IS the linearization point, so every attempt starts from one manifest and opens the
        // fold and parts belonging to that exact snapshot. Once open, Unix keeps all of those handles
        // alive through a later unlink.
        //
        // Bounded, because a manifest naming a genuinely absent part must eventually surface as an
        // error rather than spin.
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..8 {
            let manifest = Manifest::load(dir)?;
            let fold_path = refold::fold_dir(dir, manifest.fold_gen);
            let fold = match Fold::open_read(&fold_path, cfg) {
                Ok(fold) => fold,
                Err(e) => {
                    let gone = e
                        .downcast_ref::<std::io::Error>()
                        .map(|io| io.kind() == std::io::ErrorKind::NotFound)
                        .unwrap_or(false)
                        || !fold_path.exists();
                    if !gone {
                        return Err(e);
                    }
                    last = Some(e);
                    continue;
                }
            };
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
                // A re-fold commit changes the address space itself: the new parts' Locs are only
                // meaningful against the new fold generation. If that swap happened while this
                // attempt was opening files, retry even when every individual open succeeded.
                if Manifest::load(dir)?.fold_gen != manifest.fold_gen {
                    last = Some(anyhow::anyhow!("fold generation changed while opening a reader snapshot"));
                    continue;
                }
                return Ok(ReadStore { fold, parts, manifest });
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("manifest snapshot names storage that does not exist")))
    }

    /// Open a READER on a retained snapshot: the store exactly as commit `commit` left it.
    ///
    /// Only commits still inside the retention window exist — [`retained_commits`] lists them, and
    /// a re-fold empties the list on purpose (time travel must not resurrect erased content).
    ///
    /// No retry loop, unlike [`Store::open_read`], deliberately: the files a LIVE manifest names
    /// can be superseded while opening them, but a retained snapshot's files are pinned on disk by
    /// its manifest, and the one way they vanish is the window advancing past it — a real error,
    /// reported as one.
    pub fn open_read_at(dir: &Path, cfg: FoldCfg, commit: u64) -> Result<ReadStore> {
        let manifest = load_retained(dir, commit)?;
        let fold = Fold::open_read(&refold::fold_dir(dir, manifest.fold_gen), cfg)?;
        let pcache = SectionCache::shared();
        let mut parts = Vec::with_capacity(manifest.parts.len());
        for p in &manifest.parts {
            parts.push(Arc::new(Part::open_in(&dir.join(&p.file), pcache.clone())?));
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
        // The commit may have pruned a retained manifest; whatever only it named is now sweepable.
        sweep_unreachable(&self.dir)?;
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
        // file as an unreachable orphan. The INPUTS are not deleted here: retained manifests still
        // name them, so a reader inside the retention window keeps a complete snapshot on disk.
        // They fall to the sweep when the window prunes past their last naming manifest.
        let mut m = self.manifest.clone();
        m.parts.splice(
            lo..lo + len,
            [PartRef {
                file: file.clone(),
                seq_lo: meta.seq_lo,
                seq_hi: meta.seq_hi,
                records: meta.n_records,
            }],
        );
        m.commit(&self.dir)?;

        self.parts.splice(lo..lo + len, [Arc::new(Part::open_in(&path, self.pcache.clone())?)]);
        self.manifest = m;
        sweep_unreachable(&self.dir)?;
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
        read::get(&self.parts, id)
    }

    /// Byte-exact content for `id`.
    pub fn reconstruct(&self, id: &str) -> Result<Option<Vec<u8>>> {
        // The memtable is newer than every part, so it is consulted first — and it is the ONLY thing
        // this adds over the committed read core.
        if let Some(v) = self.mem.get(id) {
            return match v {
                Some(r) => Ok(Some(self.rebuild(r)?)),
                None => Ok(None), // staged deletion
            };
        }
        read::reconstruct(&self.parts, &self.fold, id)
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
        let mut all = read::ids(&self.parts)?;
        // Overlay the memtable: a staged put adds an id, a staged delete removes one.
        all.retain(|id| !matches!(self.mem.get(id), Some(None)));
        for (id, v) in &self.mem {
            if v.is_some() && !all.contains(id) {
                all.push(id.clone());
            }
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// Rewrite the fold, keeping only content that live records still reference.
    ///
    /// The ONLY operation that touches content. Everything else asserts it does not, which is why this
    /// is a separate call rather than a flag: a reader of the merge path should never have to wonder.
    ///
    /// Requires a flushed memtable — staged records reference the old fold, and rebuilding parts under
    /// them would leave their pieces unresolvable.
    pub fn refold(&mut self) -> Result<refold::RefoldStats> {
        if !self.mem.is_empty() {
            bail!("refold requires a flushed memtable; call sync() and flush() first");
        }
        if self.parts.is_empty() {
            return Ok(refold::RefoldStats::default());
        }
        let seqs: Vec<(u64, u64)> =
            self.manifest.parts.iter().map(|p| (p.seq_lo, p.seq_hi)).collect();
        let (new_gen, built, mut stats) = refold::refold(
            &self.dir,
            &self.parts,
            &seqs,
            &self.fold,
            self.manifest.fold_gen,
            self.cfg,
        )?;

        // Data before pointers, exactly as everywhere else: the new fold and the new parts are durable
        // before the manifest names either, and the manifest swap is the instant it takes effect.
        let mut m = self.manifest.clone();
        m.parts = built
            .iter()
            .map(|(file, lo, hi, n)| PartRef {
                file: file.clone(),
                seq_lo: *lo,
                seq_hi: *hi,
                records: *n,
            })
            .collect();
        m.fold_gen = new_gen;
        // The new fold starts empty of history, so the committed tail is its own.
        let new_dir = refold::fold_dir(&self.dir, new_gen);
        {
            let f = Fold::open(&new_dir, self.cfg)?;
            let t = f.tail();
            m.fold_seg = t.seg;
            m.fold_off = t.off;
        }
        m.commit(&self.dir)?; // <- the linearization point

        // Everything past here is cleanup: a crash leaves orphans, which open() sweeps.
        let old_gen = self.manifest.fold_gen;
        self.manifest = m;
        // PURGE the retained log down to this commit alone. Erasure semantics trump snapshots: a
        // re-fold exists to make dropped content GONE, and a retained manifest would keep the old
        // generation — deleted records included — readable for MANIFEST_RETAIN more commits.
        // Time travel does not cross a re-fold, by design; that is the point of running one.
        for c in list_retained(&self.dir) {
            if c != self.manifest.commit {
                let _ = std::fs::remove_file(retained_path(&self.dir, c));
            }
        }
        self.pcache = SectionCache::shared();
        self.parts.clear();
        for p in &self.manifest.parts {
            self.parts.push(Arc::new(Part::open_in(&self.dir.join(&p.file), self.pcache.clone())?));
        }
        self.fold = Fold::open_at(&new_dir, self.cfg, self.manifest.fold_tail())?;
        sweep_unreachable(&self.dir)?;
        // Reported, not swallowed. Claiming `bytes_reclaimed()` while the old generation still
        // occupies the disk would be a stat that says the opposite of the truth. The re-fold itself
        // is already committed and correct; this is only honest about what is left behind.
        if refold::fold_dir(&self.dir, old_gen).exists() {
            stats.stale_generation_left = true;
        }
        Ok(stats)
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
}

/// A read-only store IS the committed read core, with nothing layered on top — so every method here
/// is a direct delegation, and there is no second implementation to keep in step.
impl ReadStore {
    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        read::get(&self.parts, id)
    }

    pub fn reconstruct(&self, id: &str) -> Result<Option<Vec<u8>>> {
        read::reconstruct(&self.parts, &self.fold, id)
    }

    /// Distinct committed ids, sorted — the union across parts, newest-wins.
    pub fn ids(&self) -> Result<Vec<String>> {
        read::ids(&self.parts)
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
    /// The bug this exists to prevent: corruption that still PARSES. A shortened `fold_off` here
    /// would have been believed, and recovery would then have truncated durable fold bytes to
    /// match it — data destroyed by one flipped bit with no error anywhere.
    #[test]
    fn a_flipped_byte_that_still_parses_is_refused() {
        let d = std::env::temp_dir().join(format!("turndb-mancrc-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut m = super::Manifest { fold_off: 4096, next_seq: 9, ..Default::default() };
        m.commit(&d).unwrap();

        let mut b = std::fs::read(d.join("MANIFEST")).unwrap();
        let at = b.windows(4).position(|w| w == b"4096").expect("fold_off literal in the JSON");
        b[at] = b'1'; // now claims fold_off 1096 — valid JSON, wrong bytes
        std::fs::write(d.join("MANIFEST"), &b).unwrap();

        let err = super::Manifest::load(&d).unwrap_err().to_string();
        assert!(err.contains("checksum"), "must refuse via the checksum, got: {err}");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Damage to the TRAILER must not demote a checksummed manifest to a trusted legacy one.
    #[test]
    fn a_damaged_trailer_is_not_read_as_legacy() {
        let d = std::env::temp_dir().join(format!("turndb-mantrail-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut m = super::Manifest::default();
        m.commit(&d).unwrap();

        let mut b = std::fs::read(d.join("MANIFEST")).unwrap();
        let at = b.len() - 14; // the 'c' of the final "crc32=XXXXXXXX" line
        b[at] = b'x';
        std::fs::write(d.join("MANIFEST"), &b).unwrap();

        assert!(super::Manifest::load(&d).is_err(), "trailing bytes must fail JSON parsing, not be ignored");
        std::fs::remove_dir_all(&d).ok();
    }

    /// The retained log: every commit leaves a copy, the window prunes, recovery promotes.
    #[test]
    fn the_commit_log_retains_prunes_and_recovers() {
        let d = std::env::temp_dir().join(format!("turndb-manlog-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut m = super::Manifest::default();
        for i in 1..=6u32 {
            m.fold_off = i * 100; // distinguishable states
            m.commit(&d).unwrap();
        }
        assert_eq!(m.commit, 6);
        assert_eq!(super::list_retained(&d), vec![3, 4, 5, 6], "window of {} commits", super::MANIFEST_RETAIN);

        // Bit rot in MANIFEST: open refuses; recovery promotes the newest copy — same commit,
        // nothing lost.
        let mut b = std::fs::read(d.join("MANIFEST")).unwrap();
        b[10] ^= 0xFF;
        std::fs::write(d.join("MANIFEST"), &b).unwrap();
        assert!(super::Manifest::load(&d).is_err());
        assert_eq!(super::recover_manifest(&d).unwrap(), 6);
        assert_eq!(super::Manifest::load(&d).unwrap().fold_off, 600);

        // MANIFEST *and* the newest copies damaged: recovery rolls back to the newest intact one
        // and truncates the log to it — the abandoned copies cannot be promoted later.
        let mut b = std::fs::read(d.join("MANIFEST")).unwrap();
        b[10] ^= 0xFF;
        std::fs::write(d.join("MANIFEST"), &b).unwrap();
        std::fs::write(super::retained_path(&d, 6), b"garbage").unwrap();
        std::fs::write(super::retained_path(&d, 5), b"garbage").unwrap();
        assert_eq!(super::recover_manifest(&d).unwrap(), 4);
        assert_eq!(super::Manifest::load(&d).unwrap().fold_off, 400);
        assert_eq!(super::list_retained(&d), vec![3, 4], "the abandoned timeline is cleared");

        // An intact store refuses rollback.
        assert!(super::recover_manifest(&d).is_err(), "recovery of a healthy store must refuse");

        // A MISSING manifest beside a commit log is damage, not a new store.
        std::fs::remove_file(d.join("MANIFEST")).unwrap();
        assert!(super::Manifest::load(&d).is_err(), "missing MANIFEST + commit log must refuse");
        assert_eq!(super::recover_manifest(&d).unwrap(), 4);
        assert_eq!(super::Manifest::load(&d).unwrap().fold_off, 400);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn manifest_roundtrips() {
        let d = std::env::temp_dir().join(format!("turndb-man-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut m = super::Manifest {
            parts: vec![super::PartRef { file: "p.part".into(), seq_lo: 1, seq_hi: 1, records: 7 }],
            fold_seg: 2,
            fold_off: 4096,
            next_seq: 9,
            fold_gen: 3,
            commit: 0,
        };
        m.commit(&d).unwrap();
        let got = super::Manifest::load(&d).unwrap();
        assert_eq!(got.parts.len(), 1);
        assert_eq!(got.fold_off, 4096);
        assert_eq!(got.fold_gen, 3);
        assert_eq!(got.next_seq, 9);
        assert_eq!(got.commit, 1, "commit() must advance the commit counter");
        assert!(!d.join("MANIFEST.tmp").exists(), "staging file must not survive a commit");
        std::fs::remove_dir_all(&d).ok();
    }
}

#[cfg(test)]
mod compat_tests {
    /// A manifest written before fold generations existed must still load, naming generation 0 — the
    /// original `fold/` directory. Otherwise this change would silently orphan every existing store.
    #[test]
    fn a_manifest_without_fold_gen_reads_as_generation_zero() {
        let d = std::env::temp_dir().join(format!("turndb-oldman-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("MANIFEST"),
            br#"{"parts":[],"fold_seg":0,"fold_off":48,"next_seq":4}"#,
        )
        .unwrap();
        let m = super::Manifest::load(&d).unwrap();
        assert_eq!(m.fold_gen, 0, "a pre-generation manifest must mean the original fold/");
        assert_eq!(m.next_seq, 4);
        assert_eq!(super::refold::fold_dir(&d, m.fold_gen), d.join("fold"));
        std::fs::remove_dir_all(&d).ok();
    }
}
