//! A **part**: an immutable, self-contained, id-sorted columnar slice of the store.
//!
//! A part holds record identity, sparse named programs that reconstruct content out of the fold, the
//! piece dictionary those programs reference, and typed attribute columns. It holds literal runs but
//! no carved piece bytes — those live in the fold and are shared by every part. That is what makes
//! merging parts cheap: a merge rewrites references and columns, never carved bytes.
//!
//! ```text
//!   [ sections … ]  [ TOC ]  [ FOOTER (56B, at EOF) ]
//! ```
//! The footer is written last and is the completeness marker: a part whose footer is absent or fails
//! its checksum was torn mid-write and is discarded, never half-read.
//!
//! # Invariants
//! - Ids are strictly increasing. Version resolution across parts is by sequence range, so a part
//!   never holds two versions of one id.
//! - The piece dictionary is sorted by `(block_id, in_off)` — fold order, not hash order.
//!
//! # What fold order actually buys (measured, not assumed)
//!
//! A piece's ordinal *is* its dictionary row index, so the sort bundles three decisions that could be
//! separate: which varint width every reference in `prog` pays, how well `pdict.loc` compresses, and
//! what can be searched. The third is already unbundled — `pdict.hsort` carries hash order separately.
//!
//! Of the other two, only compression is real: `pdict.loc` compresses 2.3x because fold order makes it
//! ascending. Reordering by reference frequency was measured on 10.2M refs over 267k pieces and saves
//! 8.1% of `prog`'s reference bytes *before* a section that already compresses 29.5x — worthless. The
//! histogram says why: 85.6% of pieces are referenced 10-99 times and the top 1% draw only 10.4% of
//! references. There is no hot set, because a piece recurs across the turn-snapshots of its own
//! trajectory and nowhere else. The structure worth exploiting is co-reference, and fold order already
//! approximates it — capture order groups a trajectory's pieces together (worth 1.83x over hash order).
//!
//! NOT a benefit, despite an earlier claim here: merge does not exploit the sortedness. It gathers
//! dictionaries through a hash map, so a linear union remains available but unimplemented.

pub mod attrs;
pub mod bloom;
pub mod builder;
pub mod cache;
pub mod content;
pub mod idcol;
pub mod merge;

use crate::fold::{Fold, Loc};
use crate::readat::ReadAt;
use crate::types::{AttrValue, BodyOp, Content, ContentHash, PieceHash, Record, BODY_CONTENT};
use anyhow::{bail, Context, Result};
use cache::{Held, Kind, SectionCache};
use idcol::{get_varint, put_varint, IdCol};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

pub const MAGIC: &[u8; 8] = b"TURNPART";
pub const FOOTER_LEN: u64 = 56;

/// The part layout this build writes, and the highest it will read.
///
/// The fold could always refuse an unknown future — a segment with unknown `flags` bails rather than
/// negotiate — and the part could not. Magic plus a footer checksum is no defence, because a future
/// writer computes a perfectly valid checksum over a layout this reader will then misparse at fixed
/// offsets. One plane negotiated and the other silently misread.
///
/// This claims one of the footer's padding bytes, which cost nothing because they were already
/// zero-filled: a part written before this existed reads as version 0, which is exactly what it is.
pub const PART_VERSION: u8 = 2;

/// Content-program op tags, packed into the low bit of a varint.
pub(crate) const OP_LIT: u64 = 0;
pub(crate) const OP_PIECE: u64 = 1;

/// RESERVED: the escape codepoint for a future op space.
///
/// The op tag is one bit and both values are taken, so there is no room for a third op — and the
/// obvious fix, widening the tag, re-encodes every op in every part and was measured to cost 2.6% of
/// compressed `prog` forever.
///
/// `tagged == 0` is a zero-length literal: reachable, semantically vacuous, and never emitted (0
/// occurrences across 623,106 body ops on three corpora). Reserving it now buys an UNBOUNDED future op
/// space for zero bytes — a later revision may define it as an escape followed by a varint op number —
/// but only if today's readers refuse it. A shipped reader that decoded it as an empty literal would
/// silently parse a future escape's payload as ops, which is the exact failure the version lever
/// exists to prevent.
const OP_ESCAPE_RESERVED: u64 = 0;

/// One section's location and encoding.
#[derive(Clone, Debug)]
struct Section {
    off: u64,
    stored: u32,
    raw: u32,
    codec: u8,
    /// crc32 of the STORED bytes. See the note in `finish` on why the field exists now and the
    /// verification policy does not.
    xsum: u32,
}

/// What a part says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartMeta {
    pub n_records: u32,
    /// Inclusive sequence range. Version resolution across parts compares `seq_hi`; a merge's output
    /// takes `min(seq_lo)`/`max(seq_hi)` of its inputs so it sorts correctly against parts it did not
    /// include.
    pub seq_lo: u64,
    pub seq_hi: u64,
}

/// One named content column declared by a revision-2 part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentMeta {
    pub name: String,
    pub occurrences: usize,
    pub dense: bool,
}

// ---------------------------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------------------------

/// Write a part from records that are already carved, in any order.
///
/// `resolve` maps a piece's content identity to its location in the fold — the caller owns that
/// mapping because the fold's dedup window and the parts' own dictionaries are both valid sources.
pub fn build(
    path: &Path,
    records: &[Record],
    seq_lo: u64,
    seq_hi: u64,
    level: i32,
    resolve: impl FnMut(&PieceHash) -> Option<Loc>,
) -> Result<PartMeta> {
    build_retaining(path, records, seq_lo, seq_hi, level, resolve, &HashMap::new())
}

/// [`build`], plus dictionary entries to keep even though no record here references them.
///
/// A part's dictionary is normally derived from what its records reference. A MERGE cannot use that
/// rule alone: the fold is append-only and never forgets, so a piece whose only referencing record was
/// superseded is still stored, still addressable, and still worth deduping against. Dropping it from
/// the dictionary would quietly do two harmful things —
///
///  1. lose dedup for content we go on paying to store forever, and
///  2. break resolvability for a record that is staged but not yet flushed, whose piece was matched
///     against the very dictionary entry the merge removed. After a crash that record can be neither
///     read nor flushed.
///
/// The dictionary is therefore the union of what is referenced and what was retained.
pub fn build_retaining(
    path: &Path,
    records: &[Record],
    seq_lo: u64,
    seq_hi: u64,
    level: i32,
    resolve: impl FnMut(&PieceHash) -> Option<Loc>,
    retain: &HashMap<PieceHash, Loc>,
) -> Result<PartMeta> {
    build_full(path, records, &[], seq_lo, seq_hi, level, resolve, retain)
}

/// [`build_retaining`], plus which of `records` are TOMBSTONES.
///
/// A tombstone is a row like any other — it occupies its id, carries its sequence, and takes part in
/// version resolution — but reading it yields nothing. It has to be a row rather than an absence,
/// because a deletion must SHADOW older versions of the same id living in older parts, and an absence
/// cannot shadow anything.
///
/// `tombs` is parallel to `records` and may be empty, meaning none.
// Eight arguments, all distinct and none derivable from the others: where to write, what to
// write, which of it is a tombstone, the sequence range the part claims, the compression level,
// how to resolve a piece to a location, and which locations to retain. Bundling them into a struct
// would move the same eight names one level down for no gain in clarity.
#[allow(clippy::too_many_arguments)]
pub fn build_full(
    path: &Path,
    records: &[Record],
    tombs: &[bool],
    seq_lo: u64,
    seq_hi: u64,
    level: i32,
    resolve: impl FnMut(&PieceHash) -> Option<Loc>,
    retain: &HashMap<PieceHash, Loc>,
) -> Result<PartMeta> {
    build_full_with_limits(
        path,
        records,
        tombs,
        seq_lo,
        seq_hi,
        level,
        resolve,
        retain,
        crate::read_limits::ReadLimits::default(),
    )
}

/// [`build_full`] with a policy that prevents publishing atomic frames this profile cannot read.
#[allow(clippy::too_many_arguments)]
pub fn build_full_with_limits(
    path: &Path,
    records: &[Record],
    tombs: &[bool],
    seq_lo: u64,
    seq_hi: u64,
    level: i32,
    resolve: impl FnMut(&PieceHash) -> Option<Loc>,
    retain: &HashMap<PieceHash, Loc>,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<PartMeta> {
    let sink = FilePartSink::create(path)?;
    let (meta, _) =
        build_full_into(sink, records, tombs, seq_lo, seq_hi, level, resolve, retain, read_limits)?;
    Ok(meta)
}

/// [`build_full`] into any sink — the seam that lets a flush assemble a part directly inside a
/// container member instead of a file of its own. Returns the sink so a member handle can be
/// carried back to the registration that names it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_full_into<S: crate::vfs::ArtifactSink>(
    sink: S,
    records: &[Record],
    tombs: &[bool],
    seq_lo: u64,
    seq_hi: u64,
    level: i32,
    mut resolve: impl FnMut(&PieceHash) -> Option<Loc>,
    retain: &HashMap<PieceHash, Loc>,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<(PartMeta, S)> {
    if !tombs.is_empty() && tombs.len() != records.len() {
        bail!("tombstone flags ({}) must be parallel to records ({})", tombs.len(), records.len());
    }
    // ---- order + uniqueness ----
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| records[a].id.cmp(&records[b].id));
    for w in order.windows(2) {
        if records[w[0]].id == records[w[1]].id {
            bail!("a part cannot hold two versions of id {:?}", records[w[0]].id);
        }
    }
    let ids: Vec<String> = order.iter().map(|&i| records[i].id.clone()).collect();

    // ---- piece dictionary: distinct locs, sorted in FOLD order ----
    let mut piece_of: HashMap<PieceHash, Loc> = HashMap::new();
    for r in records {
        crate::types::validate_contents(&r.contents)?;
        for content in &r.contents {
            for op in &content.ops {
                if let BodyOp::Piece { hash, .. } = op {
                    if !piece_of.contains_key(hash) {
                        let loc = resolve(hash).ok_or_else(|| {
                            anyhow::anyhow!("piece {hash} is referenced but not in the fold")
                        })?;
                        piece_of.insert(*hash, loc);
                    }
                }
            }
        }
    }
    for (h, l) in retain {
        piece_of.entry(*h).or_insert(*l);
    }
    let mut dict: Vec<(Loc, PieceHash)> = piece_of.iter().map(|(h, l)| (*l, *h)).collect();
    dict.sort_by_key(|(l, _)| (l.block_id, l.in_off));
    let dict_index: HashMap<PieceHash, u32> =
        dict.iter().enumerate().map(|(i, (_, h))| (*h, i as u32)).collect();

    // ---- named content and attribute columns ----
    let ordered: Vec<&Record> = order.iter().map(|&i| &records[i]).collect();
    let content = content::build(&ordered, &dict_index)?;
    let built = attrs::build(&ordered)?;

    // ---- id column ----
    let (id_stream, id_restarts) = idcol::build(&ids)?;

    // ---- lay the sections down, in a fixed order (determinism) ----
    let mut w = Writer::over(sink, level, read_limits)?;
    w.section("ids", &id_stream)?;
    w.section("ids.restart", &u32s(&id_restarts))?;
    w.section("cmeta", &content.meta)?;
    for (i, c) in content.cols.iter().enumerate() {
        w.section(&format!("con.prog.{i}"), &c.prog)?;
        w.section(&format!("con.off.{i}"), &u64s(&c.offsets))?;
        w.section(&format!("con.id.{i}"), &c.identities)?;
        if !c.dense {
            w.section(&format!("con.rid.{i}"), &c.rid)?;
        }
    }
    w.section("pdict.loc", &dict.iter().flat_map(|(l, _)| l.encode()).collect::<Vec<u8>>())?;
    w.section("pdict.hash", &dict.iter().flat_map(|(_, h)| h.0).collect::<Vec<u8>>())?;
    // The dictionary is sorted in FOLD order, which is what makes merge a gather and keeps decode
    // sequential — but it cannot be searched by content. Tier-1 dedup therefore carries a hash-sorted
    // permutation of it (4 B/piece) plus a filter (1.25 B/piece), rather than re-sorting the dictionary
    // and giving up the fold-order property. Two orders over one dictionary, not two dictionaries.
    let mut hsort: Vec<u32> = (0..dict.len() as u32).collect();
    hsort.sort_by_key(|&i| dict[i as usize].1 .0);
    w.section("pdict.hsort", &u32s(&hsort))?;
    let mut bloom = bloom::Bloom::with_capacity(dict.len());
    for (_, h) in &dict {
        bloom.insert(h);
    }
    w.section("pdict.bloom", &bloom.encode())?;
    // Tombstoned ROW ordinals, ascending, delta-varint. Usually empty and always tiny; a section that
    // is absent means "this part deletes nothing", so parts written before deletion existed read
    // correctly with no version lever.
    if !tombs.is_empty() {
        let mut tb = Vec::new();
        let mut prev = 0u64;
        let mut n = 0u64;
        for (row, &ri) in order.iter().enumerate() {
            if tombs[ri] {
                put_varint(&mut tb, row as u64 - prev);
                prev = row as u64;
                n += 1;
            }
        }
        if n > 0 {
            let mut out = Vec::with_capacity(tb.len() + 4);
            put_varint(&mut out, n);
            out.extend_from_slice(&tb);
            w.section("tomb", &out)?;
        }
    }
    w.section("layout", &built.layout)?;
    w.section("layout.off", &u64s(&built.layout_off))?;
    w.section("colmeta", &built.meta)?;
    w.section("zone", &built.zones)?;
    for (i, c) in built.cols.iter().enumerate() {
        w.section(&format!("col.val.{i}"), &c.val)?;
        if !c.rid.is_empty() {
            w.section(&format!("col.rid.{i}"), &c.rid)?;
        }
        if !c.dict.is_empty() {
            w.section(&format!("col.dict.{i}"), &c.dict)?;
        }
    }
    if ids.len() as u64 > u32::MAX as u64 {
        bail!("{} records exceeds the u32 record count a part footer can name", ids.len());
    }
    let meta = PartMeta { n_records: ids.len() as u32, seq_lo, seq_hi };
    let sink = w.finish(meta)?;
    Ok((meta, sink))
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn u64s(v: &[u64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// A part landing in a file of its own — today's only shape, and the default sink. `sync` is the
/// part's completeness barrier, exactly the fsync `finish` has always issued.
pub(crate) struct FilePartSink {
    f: File,
    path: std::path::PathBuf,
}

impl FilePartSink {
    pub(crate) fn create(path: &Path) -> Result<FilePartSink> {
        let f =
            crate::vfs::create(path).with_context(|| format!("create part {}", path.display()))?;
        Ok(FilePartSink { f, path: path.to_path_buf() })
    }
}

impl crate::vfs::ArtifactSink for FilePartSink {
    fn write_all_at(&mut self, data: &[u8], off: u64) -> std::io::Result<()> {
        crate::vfs::write_all_at(&self.f, &self.path, data, off)
    }
    fn sync(&mut self) -> std::io::Result<()> {
        crate::vfs::sync_file(&self.f, &self.path)
    }
    fn describe(&self) -> String {
        format!("part {}", self.path.display())
    }
}

pub(crate) struct Writer<S: crate::vfs::ArtifactSink = FilePartSink> {
    sink: S,
    off: u64,
    toc: Vec<(String, Section)>,
    level: i32,
    read_limits: crate::read_limits::ReadLimits,
}

impl Writer<FilePartSink> {
    #[cfg(test)]
    pub(crate) fn new(path: &Path, level: i32) -> Result<Writer> {
        Writer::over(FilePartSink::create(path)?, level, crate::read_limits::ReadLimits::default())
    }
}

impl<S: crate::vfs::ArtifactSink> Writer<S> {
    /// Assemble into any sink — a fresh file, or a member region inside a container. The part's
    /// internal offsets are sink-relative either way, which is the artifact-relative invariant
    /// every reader already depends on.
    pub(crate) fn over(
        sink: S,
        level: i32,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Writer<S>> {
        let read_limits = read_limits.validate()?;
        Ok(Writer { sink, off: 0, toc: Vec::new(), level, read_limits })
    }

    pub(crate) fn section(&mut self, name: &str, raw: &[u8]) -> Result<()> {
        // A section's `stored` and `raw` are u32 in the TOC. Truncating here would write a part that
        // reads back as a shorter section with no error anywhere — silent corruption. Refuse instead:
        // a part that cannot be written is recoverable, a part that lies is not.
        if raw.len() as u64 > u32::MAX as u64 {
            bail!("section {name} is {} bytes; the format caps a section at 4 GiB", raw.len());
        }
        self.read_limits.admit_decoded(format!("new part section {name:?}"), raw.len() as u64)?;
        let (codec, payload) = crate::fold::codec::encode(raw, None, self.level)?;
        self.read_limits
            .admit_stored(format!("new part section {name:?}"), payload.len() as u64)?;
        self.sink
            .write_all_at(&payload, self.off)
            .with_context(|| format!("write section {name:?} of {}", self.sink.describe()))?;
        self.toc.push((
            name.to_string(),
            Section {
                off: self.off,
                stored: payload.len() as u32,
                raw: raw.len() as u32,
                codec,
                xsum: crc32fast::hash(&payload),
            },
        ));
        self.off += payload.len() as u64;
        Ok(())
    }

    pub(crate) fn finish(self, meta: PartMeta) -> Result<S> {
        self.finish_version(meta, PART_VERSION)
    }

    fn finish_version(mut self, meta: PartMeta, version: u8) -> Result<S> {
        if version > PART_VERSION {
            bail!("cannot write unsupported part version {version}");
        }
        if meta.seq_lo > meta.seq_hi {
            bail!(
                "cannot write a part with inverted sequence range {}..{}",
                meta.seq_lo,
                meta.seq_hi
            );
        }
        let mut toc = Vec::new();
        put_varint(&mut toc, self.toc.len() as u64);
        for (name, s) in &self.toc {
            put_varint(&mut toc, name.len() as u64);
            toc.extend_from_slice(name.as_bytes());
            put_varint(&mut toc, s.off);
            put_varint(&mut toc, s.stored as u64);
            put_varint(&mut toc, s.raw as u64);
            toc.push(s.codec);
            // A section's own integrity, over its STORED bytes so it can be checked without
            // decompressing. Content already carries BLAKE3 per piece and is verified on every read;
            // this covers what content hashes do not — ids, attribute values, offsets, dictionaries —
            // where a flipped bit is a wrong query answer with no error anywhere.
            //
            // The FIELD is the format decision and is taken now, because adding it later costs a
            // version bump. WHEN to verify is runtime policy and is deliberately not decided here:
            // hashing a 65 MiB section on every read would be a tax worth measuring first.
            toc.extend_from_slice(&s.xsum.to_le_bytes());
        }
        let (toc_codec, toc_payload) = crate::fold::codec::encode(&toc, None, self.level)?;
        if toc.len() as u64 > u32::MAX as u64 || toc_payload.len() as u64 > u32::MAX as u64 {
            bail!(
                "part TOC is {} raw / {} stored bytes; the format caps each at 4 GiB",
                toc.len(),
                toc_payload.len()
            );
        }
        self.read_limits.admit("new part TOC", toc_payload.len() as u64, toc.len() as u64)?;
        let toc_off = self.off;
        self.sink
            .write_all_at(&toc_payload, toc_off)
            .with_context(|| format!("write TOC of {}", self.sink.describe()))?;

        let mut foot = Vec::with_capacity(FOOTER_LEN as usize);
        foot.extend_from_slice(MAGIC);
        foot.extend_from_slice(&toc_off.to_le_bytes());
        foot.extend_from_slice(&(toc_payload.len() as u32).to_le_bytes());
        foot.extend_from_slice(&(toc.len() as u32).to_le_bytes());
        foot.extend_from_slice(&meta.n_records.to_le_bytes());
        foot.extend_from_slice(&meta.seq_lo.to_le_bytes());
        foot.extend_from_slice(&meta.seq_hi.to_le_bytes());
        foot.push(toc_codec);
        foot.push(version);
        // The TOC is where every section's checksum lives, so leaving the TOC itself unchecked made
        // those checksums only as trustworthy as the bytes carrying them. This covers the STORED TOC
        // payload, and the footer's own checksum covers this — so the chain is closed: footer verifies
        // itself, footer verifies the TOC, TOC verifies each section.
        foot.extend_from_slice(&crc32fast::hash(&toc_payload).to_le_bytes());
        while foot.len() < FOOTER_LEN as usize - 4 {
            foot.push(0);
        }
        let x = blake3::hash(&foot);
        foot.extend_from_slice(&x.as_bytes()[0..4]);
        debug_assert_eq!(foot.len(), FOOTER_LEN as usize);
        // The footer lands LAST and is the completeness marker; the sink decides whether the
        // barrier is its own fsync or an enclosing commit's.
        self.sink
            .write_all_at(&foot, toc_off + toc_payload.len() as u64)
            .with_context(|| format!("write footer of {}", self.sink.describe()))?;
        self.sink.sync().with_context(|| format!("sync {}", self.sink.describe()))?;
        Ok(self.sink)
    }
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

/// One section's on-disk anatomy: `(name, stored, raw, codec)`.
pub type SectionInfo = (String, u32, u32, u8);

pub struct Part {
    f: Box<dyn ReadAt>,
    toc: HashMap<String, Section>,
    meta: PartMeta,
    /// Identity within the shared cache.
    id: u64,
    /// The format version this part declares. Decides whether optional fields are PRESENT, which is
    /// not the same question as whether they are non-zero.
    version: u8,
    /// Decompressed sections, decoded offset arrays, decoded row indices and string dictionaries — all
    /// four live here, under one BYTE budget shared with every other part.
    ///
    /// Each of them exists because dropping it made a whole-part walk quadratic (merge: 493s over
    /// 8x50k records). Unbounded, they pinned 9.5x a part's on-disk size and never let go. Bounded and
    /// shared, the asymptotics hold and the memory does not grow with part count.
    cache: Arc<SectionCache>,
    /// Admission checked before materializing any atomic TOC or section frame.
    read_limits: crate::read_limits::ReadLimits,
}

impl Drop for Part {
    /// Release this part's entries rather than leaving them to be evicted eventually. A closed part
    /// holding budget would push out entries belonging to parts that are still open.
    fn drop(&mut self) {
        self.cache.forget(self.id);
    }
}

impl Part {
    /// Open against the process-wide section-cache budget.
    ///
    /// Shared rather than private on purpose: a private budget per part would grow without bound in
    /// part count, which is the thing the budget exists to stop. Use [`Part::open_in`] to account a
    /// group of parts separately, as a `Store` does.
    pub fn open(path: &Path) -> Result<Part> {
        Part::open_with_limits(path, crate::read_limits::ReadLimits::default())
    }

    /// Open with an explicit atomic-frame admission policy.
    pub fn open_with_limits(
        path: &Path,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Part> {
        Part::open_in_with_limits(path, SectionCache::global(), read_limits)
    }

    /// Open sharing `cache` with other parts.
    pub fn open_in(path: &Path, cache: Arc<SectionCache>) -> Result<Part> {
        Part::open_in_with_limits(path, cache, crate::read_limits::ReadLimits::default())
    }

    /// Open sharing `cache` and enforcing `read_limits` before atomic frame allocations.
    pub fn open_in_with_limits(
        path: &Path,
        cache: Arc<SectionCache>,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Part> {
        let f = File::open(path).with_context(|| format!("open part {}", path.display()))?;
        Part::open_reader_with_limits(Box::new(f), cache, read_limits)
    }

    /// Open from any [`ReadAt`] — a plain file, an extent of a pack, a remote range. The format is
    /// footer-addressed precisely so that THIS is the only entry a backend needs.
    pub fn open_reader(f: Box<dyn ReadAt>, cache: Arc<SectionCache>) -> Result<Part> {
        Part::open_reader_with_limits(f, cache, crate::read_limits::ReadLimits::default())
    }

    /// Open any range-readable part with explicit atomic-frame admission.
    pub fn open_reader_with_limits(
        f: Box<dyn ReadAt>,
        cache: Arc<SectionCache>,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Part> {
        let read_limits = read_limits.validate()?;
        let len = f.len()?;
        if len < FOOTER_LEN {
            bail!("part of {len} bytes is too short to hold a footer");
        }
        let mut foot = [0u8; FOOTER_LEN as usize];
        f.read_exact_at(&mut foot, len - FOOTER_LEN)?;
        if &foot[0..8] != MAGIC {
            bail!("not a turndb part (bad magic) — or the footer never landed");
        }
        let want = blake3::hash(&foot[..FOOTER_LEN as usize - 4]);
        if want.as_bytes()[0..4] != foot[FOOTER_LEN as usize - 4..] {
            bail!("part footer checksum mismatch — torn write");
        }
        let toc_off = u64::from_le_bytes(foot[8..16].try_into().unwrap());
        let toc_stored = u32::from_le_bytes(foot[16..20].try_into().unwrap());
        let toc_raw = u32::from_le_bytes(foot[20..24].try_into().unwrap());
        let n_records = u32::from_le_bytes(foot[24..28].try_into().unwrap());
        let seq_lo = u64::from_le_bytes(foot[28..36].try_into().unwrap());
        let seq_hi = u64::from_le_bytes(foot[36..44].try_into().unwrap());
        if seq_lo > seq_hi {
            bail!("part footer has inverted sequence range {seq_lo}..{seq_hi}");
        }
        let toc_codec = foot[44];
        // The reject-forward lever, matching the fold's `flags`. A part from a newer writer is refused
        // rather than misparsed at offsets that may no longer mean what they did.
        let version = foot[45];
        if version > PART_VERSION {
            bail!(
                "part is format version {version}; this build reads up to {PART_VERSION} \
                 — refusing rather than guessing at its layout"
            );
        }

        // Reserved bytes must be ZERO, and a reader must refuse otherwise — the same rule the fold
        // applies to segment `flags` ("unknown means stop, not adapt"). Reserving bytes in a document
        // while the reader ignores them reserves nothing: a future writer could use them and this
        // build would accept the part and misread it. Enforcement is what makes a reservation real.
        if foot[50..52] != [0u8; 2] {
            bail!("part footer reserved bytes are non-zero — refusing rather than guessing at a layout this build does not know");
        }
        let toc_xsum = u32::from_le_bytes(foot[46..50].try_into().unwrap());
        if toc_off.saturating_add(toc_stored as u64) > len - FOOTER_LEN {
            bail!("part TOC runs past where the footer says the sections end");
        }
        read_limits.admit("part TOC", u64::from(toc_stored), u64::from(toc_raw))?;
        let mut tbuf = vec![0u8; toc_stored as usize];
        f.read_exact_at(&mut tbuf, toc_off)?;
        if version >= 1 && crc32fast::hash(&tbuf) != toc_xsum {
            bail!(
                "part TOC fails its checksum — every section checksum it carries is untrustworthy"
            );
        }
        let toc_bytes = crate::fold::codec::decode(toc_codec, &tbuf, toc_raw, None)?;

        let mut at = 0usize;
        let n = usize::try_from(get_varint(&toc_bytes, &mut at)?)
            .map_err(|_| anyhow::anyhow!("part TOC entry count exceeds this address space"))?;
        // An entry costs several bytes, so the byte count bounds the entry count — checked before
        // the count sizes an allocation, because `n` is exactly as trustworthy as the TOC carrying
        // it, and on a version-0 part the TOC has no checksum at all.
        let mut toc = HashMap::with_capacity(n.min(toc_bytes.len()));
        for _ in 0..n {
            let nl = usize::try_from(get_varint(&toc_bytes, &mut at)?).map_err(|_| {
                anyhow::anyhow!("part TOC entry name length exceeds this address space")
            })?;
            // `nl > len - at`, never `at + nl > len`: the sum overflows on a hostile length.
            if nl > toc_bytes.len() - at {
                bail!("part TOC entry name runs past the end of the TOC");
            }
            let name = String::from_utf8(toc_bytes[at..at + nl].to_vec())?;
            at += nl;
            let off = get_varint(&toc_bytes, &mut at)?;
            let stored = u32::try_from(get_varint(&toc_bytes, &mut at)?)
                .map_err(|_| anyhow::anyhow!("part TOC stored length exceeds its u32 field"))?;
            let raw = u32::try_from(get_varint(&toc_bytes, &mut at)?)
                .map_err(|_| anyhow::anyhow!("part TOC raw length exceeds its u32 field"))?;
            if at >= toc_bytes.len() {
                bail!("part TOC entry {name} is truncated before its codec");
            }
            let codec = toc_bytes[at];
            at += 1;
            // Presence is decided by VERSION, never by the value. crc32 can legitimately be zero, so
            // treating zero as "absent" would silently skip a real checksum roughly once in 4 billion.
            let xsum = if version >= 1 {
                if at + 4 > toc_bytes.len() {
                    bail!("part TOC entry {name} is truncated before its checksum");
                }
                let x = u32::from_le_bytes(toc_bytes[at..at + 4].try_into().unwrap());
                at += 4;
                x
            } else {
                0
            };
            // A corrupt-but-plausible TOC would otherwise send `sect` to allocate `stored` bytes and
            // read at an arbitrary offset. The footer is checksummed; the TOC is not, so every entry
            // is range-checked against the file it claims to live in.
            if off.saturating_add(stored as u64) > toc_off {
                bail!("part TOC entry {name} overlaps the TOC or footer");
            }
            if toc.insert(name.clone(), Section { off, stored, raw, codec, xsum }).is_some() {
                bail!("part TOC names section {name:?} more than once");
            }
        }
        if at != toc_bytes.len() {
            bail!("part TOC has {} trailing bytes after its last entry", toc_bytes.len() - at);
        }
        if version <= 1 {
            // In the singular-content layouts, the body offset count corroborates the footer's row
            // count and is required even for an empty part.
            match toc.get("prog.off") {
                Some(s) => {
                    if s.raw as u64 != (n_records as u64 + 1) * 8 {
                        bail!(
                            "footer claims {n_records} records but prog.off holds {} offsets",
                            s.raw / 8
                        );
                    }
                }
                None => bail!("part is missing its required prog.off section"),
            }
        } else if !toc.contains_key("cmeta") {
            bail!("revision-2-or-later part is missing its required cmeta section");
        }
        Ok(Part {
            f,
            version,
            toc,
            meta: PartMeta { n_records, seq_lo, seq_hi },
            id: cache::next_part_id(),
            cache,
            read_limits,
        })
    }

    pub fn meta(&self) -> PartMeta {
        self.meta
    }

    /// On-disk revision declared by this immutable part.
    pub fn format_version(&self) -> u8 {
        self.version
    }

    pub fn len(&self) -> usize {
        self.meta.n_records as usize
    }

    pub fn is_empty(&self) -> bool {
        self.meta.n_records == 0
    }

    /// A section's decompressed bytes, cached after first touch.
    fn sect(&self, name: &str) -> Result<Arc<Vec<u8>>> {
        let s = self
            .toc
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("part has no section {name}"))?
            .clone();
        let k = Kind::Section(name.to_string());
        if let Some(Held::Bytes(v)) = self.cache.get(self.id, &k) {
            crate::io_trace::part_section(self.id, name, true, s.stored, s.raw);
            return Ok(v);
        }
        self.read_limits.admit(
            format!("part section {name:?}"),
            u64::from(s.stored),
            u64::from(s.raw),
        )?;
        let mut buf = vec![0u8; s.stored as usize];
        self.f.read_exact_at(&mut buf, s.off)?;
        let raw = crate::fold::codec::decode(s.codec, &buf, s.raw, None)?;
        let arc = Arc::new(raw);
        self.cache.put(self.id, k, Held::Bytes(arc.clone()));
        crate::io_trace::part_section(self.id, name, false, s.stored, s.raw);
        Ok(arc)
    }

    fn has(&self, name: &str) -> bool {
        self.toc.contains_key(name)
    }

    /// All ids, in order.
    /// Every id, in order, decoded once and shared.
    ///
    /// The id column is front-coded, so reading it reconstructs prefixes and validates UTF-8 for every
    /// row. Callers that walk a whole part — visibility resolution, `ids()`, lens construction — each
    /// used to pay that in full, and more than once per operation. Returning a shared handle is what
    /// lets them all be written naturally without any of them re-decoding.
    pub fn ids(&self) -> Result<Arc<Vec<String>>> {
        if let Some(Held::Strings(v)) = self.cache.get(self.id, &Kind::Ids) {
            return Ok(v);
        }
        let stream = self.sect("ids")?;
        let restarts = self.restarts()?;
        let c = IdCol::new(&stream, &restarts, self.len());
        let v: Vec<String> =
            c.iter()?.into_iter().map(|b| Ok(String::from_utf8(b)?)).collect::<Result<_>>()?;
        let a = Arc::new(v);
        self.cache.put(self.id, Kind::Ids, Held::Strings(a.clone()));
        Ok(a)
    }

    /// Decode one id by row without materialising the whole id column.
    ///
    /// Front-coding restarts every 16 rows, so this reads at most one restart group. Range pagers use
    /// it to stop with the requested page rather than decoding every id after the range boundary.
    pub fn id(&self, row: usize) -> Result<String> {
        if row >= self.len() {
            bail!("row {row} out of range");
        }
        let stream = self.sect("ids")?;
        let restarts = self.restarts()?;
        Ok(String::from_utf8(IdCol::new(&stream, &restarts, self.len()).get(row)?)?)
    }

    /// The id column's restart offsets, widened to u32.
    ///
    /// NOT cached, deliberately. Caching it looked obviously right — three call sites rebuild the
    /// array — but measured against 20,000 point lookups it made no difference at all (38.5ms
    /// uncached, 39.1ms cached), so it would have been a cache entry per part bought with nothing.
    /// This exists to keep the three call sites from repeating the widening, not for speed.
    fn restarts(&self) -> Result<Vec<u32>> {
        Ok(self.nums("ids.restart", 4)?.iter().map(|&x| x as u32).collect())
    }

    /// Row ordinals whose ids fall in `[from, to)`, ascending — the part's half of a range scan.
    ///
    /// `from`/`to` are open-ended when `None`. Costs a binary search plus a walk of exactly the
    /// matching run, rather than decoding the whole id column.
    pub fn rows_in_range(
        &self,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<std::ops::Range<usize>> {
        let stream = self.sect("ids")?;
        let restarts = self.restarts()?;
        let c = IdCol::new(&stream, &restarts, self.len());
        let lo = match from {
            Some(f) => c.lower_bound(f.as_bytes())?,
            None => 0,
        };
        let hi = match to {
            Some(t) => c.lower_bound(t.as_bytes())?,
            None => self.len(),
        };
        Ok(lo..hi.max(lo))
    }

    /// Row index of `id`, or `None`.
    pub fn find(&self, id: &str) -> Result<Option<usize>> {
        let stream = self.sect("ids")?;
        let restarts = self.restarts()?;
        IdCol::new(&stream, &restarts, self.len()).find(id.as_bytes())
    }

    /// The piece dictionary entry at `i`.
    pub fn piece(&self, i: usize) -> Result<(Loc, PieceHash)> {
        let l = self.sect("pdict.loc")?;
        let h = self.sect("pdict.hash")?;
        // Compared by division, not `(i + 1) * WIDTH`: `i` arrives from a content-program varint and a
        // hostile value overflows the multiplication.
        if i >= l.len() / Loc::WIDTH || i >= h.len() / 32 {
            bail!("piece dictionary index {i} out of range");
        }
        let loc = Loc::decode(&l[i * Loc::WIDTH..])?;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&h[i * 32..i * 32 + 32]);
        Ok((loc, PieceHash(hash)))
    }

    /// Check every section against its recorded checksum.
    ///
    /// Explicitly NOT done on the read path. Content is already verified per piece on every read; this
    /// covers the columnar metadata, where the cost is proportional to the whole part rather than to
    /// what a query touches. Offering it as a deliberate call keeps that a caller's choice — a
    /// consistency check, a repair tool, an ingest gate — instead of a tax every scan pays.
    pub fn verify_sections(&self) -> Result<usize> {
        self.verify_sections_with_control(&crate::control::OperationControl::default())
    }

    /// [`Part::verify_sections`] with cooperative checkpoints during bounded section reads.
    pub fn verify_sections_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<usize> {
        let mut checked = 0usize;
        if self.version < 1 {
            return Ok(0); // predates per-section checksums; nothing to check, and that is not an error
        }
        for (name, s) in &self.toc {
            control.check("part verification")?;
            let mut remaining = u64::from(s.stored);
            let mut offset = s.off;
            let mut hasher = crc32fast::Hasher::new();
            let mut buf = vec![0u8; (1 << 20).min(remaining.max(1) as usize)];
            while remaining > 0 {
                control.check("part verification")?;
                let take = buf.len().min(remaining as usize);
                self.f.read_exact_at(&mut buf[..take], offset)?;
                hasher.update(&buf[..take]);
                offset += take as u64;
                remaining -= take as u64;
            }
            let got = hasher.finalize();
            if got != s.xsum {
                bail!("section {name} fails its checksum ({got:#010x} != {:#010x})", s.xsum);
            }
            checked += 1;
        }
        Ok(checked)
    }

    /// Rows this part deletes, ascending. Empty for a part that deletes nothing, and for every part
    /// written before deletion existed.
    pub fn tombstones(&self) -> Result<Arc<Vec<u64>>> {
        if !self.has("tomb") {
            return Ok(Arc::new(Vec::new()));
        }
        let k = cache::Kind::Nums("tomb".into());
        if let Some(Held::Nums(v)) = self.cache.get(self.id, &k) {
            return Ok(v);
        }
        let b = self.sect("tomb")?;
        let mut at = 0usize;
        let n = get_varint(&b, &mut at)? as usize;
        let mut out = Vec::with_capacity(n.min(b.len()));
        let mut cur = 0u64;
        for _ in 0..n {
            cur += get_varint(&b, &mut at)?;
            out.push(cur);
        }
        let a = Arc::new(out);
        self.cache.put(self.id, k, Held::Nums(a.clone()));
        Ok(a)
    }

    /// Is row `r` a deletion?
    pub fn is_tombstone(&self, r: usize) -> Result<bool> {
        Ok(self.tombstones()?.binary_search(&(r as u64)).is_ok())
    }

    /// Whether this part carries any attribute columns at all.
    pub fn has_columns(&self) -> bool {
        self.has("colmeta")
    }

    /// A column's raw fixed-width value section — the columnar read path, one section per call.
    pub fn column_values(&self, c: usize) -> Result<std::sync::Arc<Vec<u8>>> {
        self.sect(&format!("col.val.{c}"))
    }

    pub fn piece_count(&self) -> Result<usize> {
        Ok(self.sect("pdict.loc")?.len() / Loc::WIDTH)
    }

    /// **Tier-1 dedup.** Does this part already hold `h`, and if so, where in the fold?
    ///
    /// Filter first — that is the whole point, since at high duplication almost every write asks this
    /// question of every part and nearly all answers are "no". Only on a filter hit does the sorted
    /// permutation get searched, and only then is the answer definitive.
    ///
    /// Parts written before this section existed simply answer `None`: an older part is allowed to
    /// decline to participate in dedup, because a miss costs bytes and never correctness.
    pub fn lookup_piece(&self, h: &PieceHash) -> Result<Option<Loc>> {
        if !self.has("pdict.bloom") || !self.has("pdict.hsort") {
            return Ok(None);
        }
        if !bloom::probe_encoded(&self.sect("pdict.bloom")?, h) {
            return Ok(None);
        }
        let ord = self.nums("pdict.hsort", 4)?;
        let hashes = self.sect("pdict.hash")?;
        // Binary search the permutation; each probe is a random read into the (cached) hash column.
        let mut lo = 0usize;
        let mut hi = ord.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let at = ord[mid] as usize * 32;
            if at + 32 > hashes.len() {
                bail!("piece dictionary permutation points outside the hash column");
            }
            match hashes[at..at + 32].cmp(&h.0[..]) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let l = self.sect("pdict.loc")?;
                    let o = ord[mid] as usize * Loc::WIDTH;
                    return Ok(Some(Loc::decode(&l[o..])?));
                }
            }
        }
        Ok(None) // filter false positive
    }

    /// The named content columns this part declares, in canonical UTF-8 byte order.
    pub fn content_meta(&self) -> Result<Arc<Vec<ContentMeta>>> {
        if let Some(Held::ContentMeta(v)) = self.cache.get(self.id, &Kind::ContentMeta) {
            return Ok(v);
        }
        if self.version <= 1 {
            let out = Arc::new(vec![ContentMeta {
                name: BODY_CONTENT.to_string(),
                occurrences: self.len(),
                dense: true,
            }]);
            self.cache.put(self.id, Kind::ContentMeta, Held::ContentMeta(out.clone()));
            return Ok(out);
        }
        let meta = self.sect("cmeta")?;
        let mut at = 0usize;
        let n = usize::try_from(get_varint(&meta, &mut at)?)
            .context("content-column count exceeds this platform's address space")?;
        let mut out = Vec::with_capacity(n.min(meta.len()));
        let mut previous: Option<Vec<u8>> = None;
        for i in 0..n {
            let name_len = usize::try_from(get_varint(&meta, &mut at)?)
                .context("content name length exceeds this platform's address space")?;
            if name_len == 0 {
                bail!("content column {i} has an empty name");
            }
            if name_len > meta.len() - at {
                bail!("content column {i}'s name runs past cmeta");
            }
            let name_bytes = meta[at..at + name_len].to_vec();
            at += name_len;
            if previous.as_deref().is_some_and(|p| p >= name_bytes.as_slice()) {
                bail!("content column names are duplicated or out of canonical order");
            }
            previous = Some(name_bytes.clone());
            let name = String::from_utf8(name_bytes)?;
            let occurrences = usize::try_from(get_varint(&meta, &mut at)?)
                .context("content occurrence count exceeds this platform's address space")?;
            if occurrences > self.len() {
                bail!(
                    "content {name:?} has {occurrences} occurrences across only {} rows",
                    self.len()
                );
            }
            let rid_kind = *meta
                .get(at)
                .ok_or_else(|| anyhow::anyhow!("content {name:?} is missing its row-id kind"))?;
            at += 1;
            let dense = match rid_kind {
                content::RID_DENSE => {
                    if occurrences != self.len() {
                        bail!(
                            "dense content {name:?} has {occurrences} occurrences for {} rows",
                            self.len()
                        );
                    }
                    true
                }
                content::RID_DELTA => false,
                k => bail!("content {name:?} has unknown row-id kind {k}"),
            };
            let prog = format!("con.prog.{i}");
            let off = format!("con.off.{i}");
            let rid = format!("con.rid.{i}");
            let identity = format!("con.id.{i}");
            if !self.has(&prog) || !self.has(&off) {
                bail!("content {name:?} is missing its program or offset section");
            }
            if self.toc[&off].raw as u64 != (occurrences as u64 + 1) * 8 {
                bail!("content {name:?} has an offset count inconsistent with cmeta");
            }
            if dense && self.has(&rid) {
                bail!("dense content {name:?} must elide its row-id section");
            }
            if !dense && !self.has(&rid) {
                bail!("sparse content {name:?} is missing its row-id section");
            }
            if self.version >= 2 {
                if !self.has(&identity) {
                    bail!("content {name:?} is missing its identity section");
                }
                let expected = (occurrences as u64)
                    .checked_mul(33)
                    .ok_or_else(|| anyhow::anyhow!("content identity section size overflows"))?;
                if self.toc[&identity].raw as u64 != expected {
                    bail!("content {name:?} has an identity count inconsistent with cmeta");
                }
            }
            out.push(ContentMeta { name, occurrences, dense });
        }
        if at != meta.len() {
            bail!("cmeta has {} trailing bytes", meta.len() - at);
        }
        let out = Arc::new(out);
        self.cache.put(self.id, Kind::ContentMeta, Held::ContentMeta(out.clone()));
        Ok(out)
    }

    fn content_rids(&self, col: usize, meta: &ContentMeta) -> Result<Arc<Vec<u32>>> {
        let key = Kind::ContentRids(col);
        if let Some(Held::Rids(v)) = self.cache.get(self.id, &key) {
            return Ok(v);
        }
        let encoded = self.sect(&format!("con.rid.{col}"))?;
        let mut at = 0usize;
        let mut current = 0usize;
        let mut rows = Vec::with_capacity(meta.occurrences);
        for occurrence in 0..meta.occurrences {
            let delta = usize::try_from(get_varint(&encoded, &mut at)?)
                .context("content row delta exceeds this platform's address space")?;
            if occurrence > 0 && delta == 0 {
                bail!("content {:?} repeats row {current}", meta.name);
            }
            current = current
                .checked_add(delta)
                .ok_or_else(|| anyhow::anyhow!("content {:?} row id overflows", meta.name))?;
            if current >= self.len() {
                bail!("content {:?} names row {current} outside the part", meta.name);
            }
            rows.push(u32::try_from(current).context("content row id exceeds u32")?);
        }
        if at != encoded.len() {
            bail!("content {:?} row ids have {} trailing bytes", meta.name, encoded.len() - at);
        }
        let rows = Arc::new(rows);
        self.cache.put(self.id, key, Held::Rids(rows.clone()));
        Ok(rows)
    }

    fn content_occurrence(
        &self,
        col: usize,
        meta: &ContentMeta,
        row: usize,
    ) -> Result<Option<usize>> {
        if row >= self.len() {
            bail!("row {row} out of range");
        }
        if meta.dense {
            return Ok(Some(row));
        }
        let rows = self.content_rids(col, meta)?;
        let row = u32::try_from(row).context("content row id exceeds u32")?;
        Ok(rows.binary_search(&row).ok())
    }

    fn program(&self, prog_name: &str, off_name: &str, occurrence: usize) -> Result<Vec<BodyOp>> {
        let prog = self.sect(prog_name)?;
        let offs = self.nums(off_name, 8)?;
        if occurrence + 1 >= offs.len() {
            bail!("content occurrence {occurrence} is outside {off_name}");
        }
        let (start, end) = (offs[occurrence] as usize, offs[occurrence + 1] as usize);
        if end > prog.len() || start > end {
            bail!("{off_name} names a program outside {prog_name}");
        }
        self.decode_program(&prog[start..end])
    }

    fn decode_program(&self, program: &[u8]) -> Result<Vec<BodyOp>> {
        let mut at = 0usize;
        let n = usize::try_from(get_varint(program, &mut at)?)
            .context("content-op count exceeds this platform's address space")?;
        let mut out = Vec::with_capacity(n.min(program.len().saturating_sub(at)));
        for _ in 0..n {
            let tagged = get_varint(program, &mut at)?;
            if tagged == OP_ESCAPE_RESERVED {
                bail!(
                    "content program uses the reserved op escape — this part needs a newer build"
                );
            }
            if tagged & 1 == OP_LIT {
                let len = usize::try_from(tagged >> 1)
                    .context("literal length exceeds this platform's address space")?;
                if len > program.len() - at {
                    bail!("literal runs past the program");
                }
                out.push(BodyOp::Lit(program[at..at + len].to_vec()));
                at += len;
            } else {
                let idx = usize::try_from(tagged >> 1)
                    .context("piece index exceeds this platform's address space")?;
                let len = u32::try_from(get_varint(program, &mut at)?)
                    .context("piece length exceeds the format's u32 limit")?;
                let (_, hash) = self.piece(idx)?;
                out.push(BodyOp::Piece { hash, len });
            }
        }
        if at != program.len() {
            bail!("content program has {} trailing bytes", program.len() - at);
        }
        Ok(out)
    }

    /// One named content program at row `r`, if present.
    pub fn content(&self, r: usize, name: &str) -> Result<Option<Vec<BodyOp>>> {
        if self.version <= 1 {
            return if name == BODY_CONTENT {
                Ok(Some(self.program("prog", "prog.off", r)?))
            } else {
                Ok(None)
            };
        }
        let columns = self.content_meta()?;
        let Ok(col) = columns.binary_search_by(|c| c.name.as_bytes().cmp(name.as_bytes())) else {
            return Ok(None);
        };
        let Some(occurrence) = self.content_occurrence(col, &columns[col], r)? else {
            return Ok(None);
        };
        Ok(Some(self.program(&format!("con.prog.{col}"), &format!("con.off.{col}"), occurrence)?))
    }

    /// Exact reconstructed-byte identity for one named value, when its format carried one.
    ///
    /// `None` means either the value is absent or it came from a legacy/unidentified record; callers
    /// that need to distinguish those states first ask [`Part::content`]. No program or fold block is
    /// read.
    pub fn content_identity(&self, r: usize, name: &str) -> Result<Option<ContentHash>> {
        if r >= self.len() {
            bail!("row {r} out of range");
        }
        if self.version <= 1 {
            return Ok(None);
        }
        let columns = self.content_meta()?;
        let Ok(col) = columns.binary_search_by(|c| c.name.as_bytes().cmp(name.as_bytes())) else {
            return Ok(None);
        };
        let Some(occurrence) = self.content_occurrence(col, &columns[col], r)? else {
            return Ok(None);
        };
        let identities = self.sect(&format!("con.id.{col}"))?;
        let at = occurrence
            .checked_mul(33)
            .ok_or_else(|| anyhow::anyhow!("content identity offset overflows"))?;
        let end = at
            .checked_add(33)
            .ok_or_else(|| anyhow::anyhow!("content identity end offset overflows"))?;
        let encoded = identities
            .get(at..end)
            .ok_or_else(|| anyhow::anyhow!("content identity occurrence is truncated"))?;
        match encoded[0] {
            0 => {
                if encoded[1..].iter().any(|&byte| byte != 0) {
                    bail!("unavailable content identity has a nonzero digest");
                }
                Ok(None)
            }
            1 => Ok(Some(ContentHash(encoded[1..].try_into().unwrap()))),
            marker => bail!("content identity has unknown availability marker {marker}"),
        }
    }

    /// Every named content value at row `r`, in canonical name order.
    pub fn contents(&self, r: usize) -> Result<Vec<Content>> {
        let mut out = Vec::new();
        for meta in self.content_meta()?.iter() {
            if let Some(ops) = self.content(r, &meta.name)? {
                let mut content = Content::new(&meta.name, ops);
                content.identity = self.content_identity(r, &meta.name)?;
                out.push(content);
            }
        }
        Ok(out)
    }

    /// Named content values selected by name. Sibling program/offset/identity sections are not
    /// opened, which lets metadata queries retain content-column independence.
    pub fn contents_selected(
        &self,
        r: usize,
        names: &std::collections::HashSet<&str>,
    ) -> Result<Vec<Content>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for meta in self.content_meta()?.iter().filter(|meta| names.contains(meta.name.as_str())) {
            if let Some(ops) = self.content(r, &meta.name)? {
                let mut content = Content::new(&meta.name, ops);
                content.identity = self.content_identity(r, &meta.name)?;
                out.push(content);
            }
        }
        Ok(out)
    }

    /// Named content selected for several rows, in caller row order and canonical name order.
    ///
    /// Column metadata, sparse row ids, offsets, programs and identities are each opened once per
    /// selected column for the gather. Only the requested programs are decoded.
    pub fn contents_selected_many(
        &self,
        rows: &[usize],
        names: &std::collections::HashSet<&str>,
    ) -> Result<Vec<Vec<Content>>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        if names.is_empty() {
            return Ok(vec![Vec::new(); rows.len()]);
        }
        if self.version <= 1 {
            return rows.iter().map(|&row| self.contents_selected(row, names)).collect();
        }

        for &row in rows {
            if row >= self.len() {
                bail!("row {row} out of range");
            }
        }
        let columns = self.content_meta()?;
        let mut out = vec![Vec::new(); rows.len()];
        for (col, meta) in
            columns.iter().enumerate().filter(|(_, meta)| names.contains(meta.name.as_str()))
        {
            let sparse_rows = if meta.dense { None } else { Some(self.content_rids(col, meta)?) };
            let prog_name = format!("con.prog.{col}");
            let off_name = format!("con.off.{col}");
            let prog = self.sect(&prog_name)?;
            let offs = self.nums(&off_name, 8)?;
            let identities =
                if self.version >= 2 { Some(self.sect(&format!("con.id.{col}"))?) } else { None };

            for (output, &row) in rows.iter().enumerate() {
                let occurrence = if meta.dense {
                    Some(row)
                } else {
                    let row = u32::try_from(row).context("content row id exceeds u32")?;
                    sparse_rows
                        .as_ref()
                        .expect("sparse content has row ids")
                        .binary_search(&row)
                        .ok()
                };
                let Some(occurrence) = occurrence else { continue };
                if occurrence >= offs.len().saturating_sub(1) {
                    bail!("content occurrence {occurrence} is outside {off_name}");
                }
                let start = usize::try_from(offs[occurrence]).with_context(|| {
                    format!("content occurrence {occurrence} start exceeds this platform")
                })?;
                let end = usize::try_from(offs[occurrence + 1]).with_context(|| {
                    format!("content occurrence {occurrence} end exceeds this platform")
                })?;
                if end > prog.len() || start > end {
                    bail!("{off_name} names a program outside {prog_name}");
                }
                let identity = if let Some(identities) = &identities {
                    let at = occurrence
                        .checked_mul(33)
                        .ok_or_else(|| anyhow::anyhow!("content identity offset overflows"))?;
                    let end = at
                        .checked_add(33)
                        .ok_or_else(|| anyhow::anyhow!("content identity end offset overflows"))?;
                    let encoded = identities.get(at..end).ok_or_else(|| {
                        anyhow::anyhow!("content identity occurrence is truncated")
                    })?;
                    match encoded[0] {
                        0 => {
                            if encoded[1..].iter().any(|&byte| byte != 0) {
                                bail!("unavailable content identity has a nonzero digest");
                            }
                            None
                        }
                        1 => Some(ContentHash(encoded[1..].try_into().unwrap())),
                        marker => {
                            bail!("content identity has unknown availability marker {marker}")
                        }
                    }
                } else {
                    None
                };
                let mut content = Content::new(&meta.name, self.decode_program(&prog[start..end])?);
                content.identity = identity;
                out[output].push(content);
            }
        }
        Ok(out)
    }

    /// Compatibility body program. An absent `body` value reads as empty through this legacy API.
    pub fn body(&self, r: usize) -> Result<Vec<BodyOp>> {
        Ok(self.content(r, BODY_CONTENT)?.unwrap_or_default())
    }

    /// Row `r`'s attributes, in their exact original order, duplicates included.
    pub fn attrs(&self, r: usize) -> Result<Vec<(String, AttrValue)>> {
        attrs::read_row(self, r)
    }

    /// Selected attributes in exact row order. Only selected value columns are opened.
    pub fn attrs_selected(
        &self,
        r: usize,
        names: &std::collections::HashSet<&str>,
    ) -> Result<Vec<(String, AttrValue)>> {
        attrs::read_row_selected(self, r, names)
    }

    /// Selected attributes for several rows, sharing the column decoders across the gather.
    pub fn attrs_selected_many(
        &self,
        rows: &[usize],
        names: &std::collections::HashSet<&str>,
    ) -> Result<Vec<Vec<(String, AttrValue)>>> {
        attrs::read_rows_selected(self, rows, names)
    }

    /// Column `c`'s zone map: `(min, max)` over every value the column holds, or `None` when no
    /// pruning is possible — an older part, a string column (its sorted dictionary already bounds
    /// it), a float column that saw NaN, or a damaged section. Advisory by construction: `None`
    /// only ever costs a scan, never an answer.
    pub fn zone(&self, c: usize) -> Result<Option<(AttrValue, AttrValue)>> {
        attrs::read_zone(self, c)
    }

    /// The whole record at row `r`.
    pub fn record(&self, r: usize) -> Result<Record> {
        let ids = self.sect("ids")?;
        let restarts: Vec<u32> = self.nums("ids.restart", 4)?.iter().map(|&x| x as u32).collect();
        let id = String::from_utf8(IdCol::new(&ids, &restarts, self.len()).get(r)?)?;
        Ok(Record { id, contents: self.contents(r)?, attrs: self.attrs(r)? })
    }

    /// Reconstruct row `r`'s content byte-exactly, resolving pieces through `fold`.
    ///
    /// Piece references go through the part's own dictionary, so the fold is addressed by location and
    /// never searched. The dictionary is in fold order, so a scan walks the fold forward.
    pub fn reconstruct(&self, r: usize, fold: &Fold) -> Result<Vec<u8>> {
        self.reconstruct_content(r, BODY_CONTENT, fold).map(|v| v.unwrap_or_default())
    }

    /// Reconstruct one named content value without touching any other content column.
    pub fn reconstruct_content(
        &self,
        r: usize,
        name: &str,
        fold: &Fold,
    ) -> Result<Option<Vec<u8>>> {
        let Some(ops) = self.content(r, name)? else {
            return Ok(None);
        };
        let mut content = Content::new(name, ops);
        content.identity = self.content_identity(r, name)?;
        Ok(Some(self.reconstruct_projected_content(&content, fold)?))
    }

    /// Reconstruct a content value whose program and identity were already projected from this
    /// part. Structured scans use this to avoid decoding the selected content column a second time.
    pub(crate) fn reconstruct_projected_content(
        &self,
        content: &Content,
        fold: &Fold,
    ) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for op in &content.ops {
            match op {
                BodyOp::Lit(bytes) => out.extend_from_slice(bytes),
                BodyOp::Piece { hash, len } => {
                    let mut loc = self.lookup_piece(hash)?;
                    if loc.is_none() {
                        // Revision-0 parts may predate the optional hash-sorted dictionary index.
                        for i in 0..self.piece_count()? {
                            let (candidate_loc, candidate_hash) = self.piece(i)?;
                            if candidate_hash == *hash {
                                loc = Some(candidate_loc);
                                break;
                            }
                        }
                    }
                    let loc = loc.ok_or_else(|| {
                        anyhow::anyhow!("piece {hash} is not in the part dictionary")
                    })?;
                    if loc.raw != *len {
                        bail!("piece {hash} is {} bytes but the program says {len}", loc.raw);
                    }
                    fold.read_verified_into(loc, *hash, &mut out)?;
                }
            }
        }
        if let Some(expected) = content.identity {
            let got = ContentHash::of(&out);
            if got != expected {
                bail!(
                    "content {:?} reconstructed as {got} but its identity is {expected}",
                    content.name
                );
            }
        }
        Ok(out)
    }

    /// Reconstruct by id.
    pub fn reconstruct_id(&self, id: &str, fold: &Fold) -> Result<Option<Vec<u8>>> {
        match self.find(id)? {
            Some(r) => Ok(Some(self.reconstruct(r, fold)?)),
            None => Ok(None),
        }
    }

    /// Every section — the on-disk anatomy of this part.
    pub fn sections(&self) -> Vec<SectionInfo> {
        let mut v: Vec<SectionInfo> =
            self.toc.iter().map(|(n, s)| (n.clone(), s.stored, s.raw, s.codec)).collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.1));
        v
    }

    pub(crate) fn rid_cached(&self, c: usize) -> Option<Arc<Vec<u32>>> {
        match self.cache.get(self.id, &Kind::Rids(c)) {
            Some(Held::Rids(v)) => Some(v),
            _ => None,
        }
    }
    pub(crate) fn rid_cache_put(&self, c: usize, v: Vec<u32>) -> Arc<Vec<u32>> {
        let a = Arc::new(v);
        self.cache.put(self.id, Kind::Rids(c), Held::Rids(a.clone()));
        a
    }

    pub(crate) fn section_bytes(&self, name: &str) -> Result<std::sync::Arc<Vec<u8>>> {
        self.sect(name)
    }
    /// A fixed-width little-endian array section, decoded once and cached.
    pub(crate) fn nums(&self, name: &str, width: usize) -> Result<Arc<Vec<u64>>> {
        let k = Kind::Nums(name.to_string());
        if let Some(Held::Nums(v)) = self.cache.get(self.id, &k) {
            return Ok(v);
        }
        let b = self.sect(name)?;
        let v: Vec<u64> = match width {
            4 => b
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()) as u64)
                .collect(),
            8 => b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect(),
            w => bail!("unsupported array width {w}"),
        };
        let a = Arc::new(v);
        self.cache.put(self.id, k, Held::Nums(a.clone()));
        Ok(a)
    }

    pub(crate) fn dict_cached(&self, c: usize) -> Option<Arc<Vec<String>>> {
        match self.cache.get(self.id, &Kind::Dict(c)) {
            Some(Held::Strings(v)) => Some(v),
            _ => None,
        }
    }

    pub(crate) fn dict_put(&self, c: usize, v: Vec<String>) -> Arc<Vec<String>> {
        let a = Arc::new(v);
        self.cache.put(self.id, Kind::Dict(c), Held::Strings(a.clone()));
        a
    }

    pub(crate) fn binary_dict_cached(&self, c: usize) -> Option<Arc<Vec<Vec<u8>>>> {
        match self.cache.get(self.id, &Kind::BinaryDict(c)) {
            Some(Held::ByteStrings(v)) => Some(v),
            _ => None,
        }
    }

    pub(crate) fn binary_dict_put(&self, c: usize, v: Vec<Vec<u8>>) -> Arc<Vec<Vec<u8>>> {
        let a = Arc::new(v);
        self.cache.put(self.id, Kind::BinaryDict(c), Held::ByteStrings(a.clone()));
        a
    }

    /// Bytes this part currently pins in its caches. Decompressed sections plus decoded arrays plus
    /// string dictionaries — everything that survives a read and is never released.
    pub fn cached_bytes(&self) -> usize {
        self.cache.bytes()
    }

    /// The cache this part reads through, for a caller that wants to share or inspect it.
    pub fn cache(&self) -> &Arc<SectionCache> {
        &self.cache
    }

    pub(crate) fn section_present(&self, name: &str) -> bool {
        self.has(name)
    }
}

/// Hand-encode a genuine version-1 part: one body-centric program per row, no `cmeta`, no
/// `con.*` sections. The migration tests need real legacy bytes produced independently of the
/// current writer, which can no longer emit this layout.
#[cfg(test)]
pub(crate) fn build_revision_one_fixture(path: &Path, seq: u64, id: &str) -> Result<PartMeta> {
    let (ids, restarts) = idcol::build(&[id.to_string()])?;
    let mut prog = Vec::new();
    put_varint(&mut prog, 1);
    put_varint(&mut prog, (6u64 << 1) | OP_LIT);
    prog.extend_from_slice(b"legacy");

    let meta = PartMeta { n_records: 1, seq_lo: seq, seq_hi: seq };
    let mut writer = Writer::new(path, 3)?;
    writer.section("ids", &ids)?;
    writer.section("ids.restart", &u32s(&restarts))?;
    writer.section("prog", &prog)?;
    writer.section("prog.off", &u64s(&[0, prog.len() as u64]))?;
    writer.section("pdict.loc", &[])?;
    writer.section("pdict.hash", &[])?;
    writer.finish_version(meta, 1)?;
    Ok(meta)
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    use crate::fold::FoldCfg;

    /// The sink seam's contract: a part assembled inside a container member is byte-identical to
    /// the same part assembled in a file of its own, the member handle's in-pass BLAKE3 equals a
    /// hash of those bytes, and the part opens straight off the extent. If any of these drift,
    /// the flush that writes parts into the live file is writing a different format than every
    /// existing reader was proven against.
    #[test]
    fn a_part_assembled_into_a_member_is_byte_identical_to_its_file_form() {
        let (dir, path) = temp_part("sink-identity");
        let records = vec![
            crate::types::Record {
                id: "a:1".into(),
                contents: vec![crate::types::Content {
                    name: "body".into(),
                    ops: vec![crate::types::ContentOp::Lit(b"first body".to_vec())],
                    identity: Some(crate::types::ContentHash(
                        *blake3::hash(b"first body").as_bytes(),
                    )),
                }],
                attrs: vec![("n".into(), AttrValue::Int(1))],
            },
            crate::types::Record {
                id: "a:2".into(),
                contents: vec![crate::types::Content {
                    name: "body".into(),
                    ops: vec![crate::types::ContentOp::Lit(b"second body".to_vec())],
                    identity: Some(crate::types::ContentHash(
                        *blake3::hash(b"second body").as_bytes(),
                    )),
                }],
                attrs: vec![("n".into(), AttrValue::Int(2))],
            },
        ];
        let retain = std::collections::HashMap::new();

        let meta_file = build_full(&path, &records, &[], 1, 1, 3, |_| None, &retain).unwrap();
        let file_bytes = std::fs::read(&path).unwrap();

        let ct = dir.join("sink.turndb");
        let mut c = crate::container::Container::create(&ct).unwrap();
        let member = c.begin_member("part-00000001.part").unwrap();

        // The tail is owned: every other staging call and the commit itself refuse mid-write.
        assert!(c.put_bytes("elbow", b"x").unwrap_err().to_string().contains("in progress"));
        assert!(c.commit().unwrap_err().to_string().contains("in progress"));

        let (meta_member, member) = build_full_into(
            member,
            &records,
            &[],
            1,
            1,
            3,
            |_| None,
            &retain,
            crate::read_limits::ReadLimits::default(),
        )
        .unwrap();
        assert_eq!(meta_member.n_records, meta_file.n_records);
        let digest = c.finish_member(member).unwrap();
        c.commit().unwrap();

        let got = c.read_file_bounded("part-00000001.part", 1 << 20).unwrap();
        assert_eq!(got, file_bytes, "the member form must be byte-identical to the file form");
        assert_eq!(
            digest,
            *blake3::hash(&file_bytes).as_bytes(),
            "the in-pass pin must equal a hash of the finished bytes"
        );

        let part = Part::open_reader(
            Box::new(c.extent("part-00000001.part").unwrap()),
            SectionCache::shared(),
        )
        .unwrap();
        assert_eq!(part.len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    /// An abandoned member write leaves nothing: no entry, no free-listing, and the bytes it
    /// landed are uncommitted noise the next stage overwrites.
    #[test]
    fn an_abandoned_member_write_is_noise_not_state() {
        let (dir, _path) = temp_part("sink-abandon");
        let ct = dir.join("abandon.turndb");
        let mut c = crate::container::Container::create(&ct).unwrap();
        let mut w = c.begin_member("doomed").unwrap();
        crate::vfs::ArtifactSink::write_all_at(&mut w, b"half an artifact", 0).unwrap();
        c.abandon_member(w);

        c.put_bytes("kept", b"real").unwrap();
        c.commit().unwrap();
        drop(c);

        let r = crate::container::Container::open(&ct).unwrap();
        assert!(!r.contains("doomed"));
        assert_eq!(r.read_file_bounded("kept", 64).unwrap(), b"real");
        assert_eq!(r.verify().unwrap(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    fn temp_part(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "turndb-part-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture.part");
        (dir, path)
    }

    #[test]
    fn duplicate_section_names_are_refused_instead_of_overwritten() {
        let (dir, path) = temp_part("duplicate-section");
        let mut writer = Writer::new(&path, 3).unwrap();
        writer.section("cmeta", &[0]).unwrap();
        writer.section("cmeta", &[0]).unwrap();
        writer.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();
        let error = match Part::open(&path) {
            Ok(_) => panic!("duplicate TOC names must refuse"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("more than once"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn section_extents_must_end_before_the_toc() {
        let (dir, path) = temp_part("section-overlap");
        let mut writer = Writer::new(&path, 3).unwrap();
        writer.section("cmeta", &[0]).unwrap();
        writer.toc[0].1.off = writer.off;
        writer.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();
        let error = match Part::open(&path) {
            Ok(_) => panic!("a section overlapping the TOC must refuse"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("overlaps the TOC"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_revision_one_body_reads_as_named_content() {
        let dir = std::env::temp_dir().join(format!(
            "turndb-part-v1-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.part");
        build_revision_one_fixture(&path, 1, "legacy").unwrap();

        let part = Part::open(&path).unwrap();
        assert_eq!(
            part.content_meta().unwrap().as_ref(),
            &[ContentMeta { name: BODY_CONTENT.into(), occurrences: 1, dense: true }]
        );
        let record = part.record(0).unwrap();
        assert_eq!(
            record.contents,
            vec![Content::new(BODY_CONTENT, vec![BodyOp::Lit(b"legacy".to_vec())])]
        );
        assert_eq!(part.content_identity(0, BODY_CONTENT).unwrap(), None);
        let fold = Fold::open(&dir.join("fold"), FoldCfg::default()).unwrap();
        assert_eq!(
            part.reconstruct_content(0, BODY_CONTENT, &fold).unwrap(),
            Some(b"legacy".to_vec())
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
