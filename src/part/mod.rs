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

pub const MAGIC: &[u8; 8] = b"TDBPRT01";
pub const FOOTER_LEN: u64 = 56;

/// The one draft part layout this build writes and reads. Earlier development layouts use different
/// magic and are not compatibility inputs.
pub const PART_DRAFT_EPOCH: u8 = 1;

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

/// One named content column declared by a part.
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
/// rule alone: ordinary fold writes append and do not forget pieces, so a piece whose only referencing
/// record was superseded is still stored and addressable until an explicit content punch or refold.
/// While it remains available, dropping it from
/// the dictionary would quietly do two harmful things —
///
///  1. lose dedup for content we continue paying to store, and
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
    mut resolve: impl FnMut(&PieceHash) -> Option<Loc>,
    retain: &HashMap<PieceHash, Loc>,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<PartMeta> {
    validate_build_input(records, tombs, seq_lo, seq_hi)?;
    read_limits.validate()?;
    let piece_of = collect_piece_dictionary(records, &mut resolve, retain)?;
    let sink = FilePartSink::create(path)?;
    let (meta, _) = build_full_resolved_into(
        sink,
        records,
        tombs,
        seq_lo,
        seq_hi,
        level,
        piece_of,
        read_limits,
    )?;
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
    validate_build_input(records, tombs, seq_lo, seq_hi)?;
    let piece_of = collect_piece_dictionary(records, &mut resolve, retain)?;
    build_full_resolved_into(sink, records, tombs, seq_lo, seq_hi, level, piece_of, read_limits)
}

fn collect_piece_dictionary(
    records: &[Record],
    resolve: &mut impl FnMut(&PieceHash) -> Option<Loc>,
    retain: &HashMap<PieceHash, Loc>,
) -> Result<HashMap<PieceHash, Loc>> {
    let mut expected_lengths = HashMap::new();
    for record in records {
        crate::types::validate_contents(&record.contents)?;
        for content in &record.contents {
            for op in &content.ops {
                let BodyOp::Piece { hash, len } = op else { continue };
                if let Some(previous) = expected_lengths.insert(*hash, *len) {
                    if previous != *len {
                        bail!(
                            "piece {hash} is referenced with conflicting lengths {previous} and {len}"
                        );
                    }
                }
            }
        }
    }

    let mut piece_of = HashMap::with_capacity(expected_lengths.len().saturating_add(retain.len()));
    for (hash, expected) in expected_lengths {
        let loc = resolve(&hash)
            .ok_or_else(|| anyhow::anyhow!("piece {hash} is referenced but not in the fold"))?;
        if loc.raw != expected {
            bail!(
                "piece {hash} is referenced as {expected} bytes but its fold location says {}",
                loc.raw
            );
        }
        piece_of.insert(hash, loc);
    }
    for (hash, loc) in retain {
        if loc.raw == 0 {
            bail!("retained piece {hash} has a zero-length fold location");
        }
        if let Some(resolved) = piece_of.get(hash) {
            if resolved != loc {
                bail!("retained piece {hash} disagrees with its resolved fold location");
            }
        } else {
            piece_of.insert(*hash, *loc);
        }
    }
    let mut location_of = std::collections::BTreeMap::new();
    for (hash, loc) in &piece_of {
        if let Some(previous) = location_of.insert(*loc, *hash) {
            if previous != *hash {
                bail!("fold location {loc:?} is assigned to both {previous} and {hash}");
            }
        }
    }
    u32::try_from(piece_of.len()).context("piece dictionary exceeds the u32 ordinal domain")?;
    Ok(piece_of)
}

#[allow(clippy::too_many_arguments)]
fn build_full_resolved_into<S: crate::vfs::ArtifactSink>(
    sink: S,
    records: &[Record],
    tombs: &[bool],
    seq_lo: u64,
    seq_hi: u64,
    level: i32,
    piece_of: HashMap<PieceHash, Loc>,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<(PartMeta, S)> {
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
    let mut bloom = bloom::Bloom::try_with_capacity(dict.len())?;
    for (_, h) in &dict {
        bloom.insert(h);
    }
    w.section("pdict.bloom", &bloom.try_encode()?)?;
    // Tombstoned ROW ordinals, ascending, delta-varint. Usually empty and always tiny; absence means
    // exactly "this part deletes nothing."
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
    if !built.cols.is_empty() {
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
    }
    if ids.len() as u64 > u32::MAX as u64 {
        bail!("{} records exceeds the u32 record count a part footer can name", ids.len());
    }
    let meta = PartMeta { n_records: ids.len() as u32, seq_lo, seq_hi };
    let sink = w.finish(meta)?;
    Ok((meta, sink))
}

fn validate_build_input(
    records: &[Record],
    tombs: &[bool],
    seq_lo: u64,
    seq_hi: u64,
) -> Result<()> {
    u32::try_from(records.len()).context("record count exceeds the u32 part domain")?;
    if seq_lo > seq_hi {
        bail!("part sequence interval is inverted: {seq_lo}..{seq_hi}");
    }
    if !tombs.is_empty() && tombs.len() != records.len() {
        bail!("tombstone flags ({}) must be parallel to records ({})", tombs.len(), records.len());
    }
    let mut ids = std::collections::BTreeSet::new();
    for record in records {
        if record.id.is_empty() {
            bail!("record id must not be empty");
        }
        if !ids.insert(record.id.as_str()) {
            bail!("a part cannot hold two versions of id {:?}", record.id);
        }
        crate::types::validate_contents(&record.contents)?;
        if let Some((_, _)) = record.attrs.iter().find(|(key, _)| key.is_empty()) {
            bail!("attribute name must not be empty");
        }
    }
    Ok(())
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
    /// The part's completeness barrier: its bytes, and then its NAME. `build_full` is public
    /// and promises a part file at `path` when it returns; on Windows a created name is durable
    /// (and reachable by anyone else) only once its directory is synced, which publishes it, so
    /// the sink syncs the directory itself rather than leaving the name to a later commit. On
    /// POSIX that is one directory fsync per part, in addition to the commit's.
    fn sync(&mut self) -> std::io::Result<()> {
        crate::vfs::sync_file(&self.f, &self.path)?;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        crate::vfs::sync_dir(parent)
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
        let next_off =
            self.off.checked_add(payload.len() as u64).context("part section end overflows")?;
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
        self.off = next_off;
        Ok(())
    }

    pub(crate) fn finish(mut self, meta: PartMeta) -> Result<S> {
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
            // physical identity rotation. WHEN to verify is runtime policy and is deliberately not
            // decided here:
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
        let footer_off =
            toc_off.checked_add(toc_payload.len() as u64).context("part TOC end overflows")?;
        footer_off.checked_add(FOOTER_LEN).context("part footer end overflows")?;
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
        foot.push(PART_DRAFT_EPOCH);
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
            .write_all_at(&foot, footer_off)
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

fn scalar_order(left: &AttrValue, right: &AttrValue) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (AttrValue::Str(left), AttrValue::Str(right)) => Some(left.cmp(right)),
        (AttrValue::Int(left), AttrValue::Int(right)) => Some(left.cmp(right)),
        (AttrValue::Float(left), AttrValue::Float(right)) => left.partial_cmp(right),
        (AttrValue::Bool(left), AttrValue::Bool(right)) => Some(left.cmp(right)),
        (AttrValue::UInt(left), AttrValue::UInt(right)) => Some(left.cmp(right)),
        (AttrValue::Bytes(left), AttrValue::Bytes(right)) => Some(left.cmp(right)),
        (AttrValue::TimestampNs(left), AttrValue::TimestampNs(right)) => Some(left.cmp(right)),
        (AttrValue::Null, AttrValue::Null) => Some(std::cmp::Ordering::Equal),
        _ => None,
    }
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
        let f =
            crate::vfs::open_read(path).with_context(|| format!("open part {}", path.display()))?;
        Part::open_reader_with_limits(Box::new(f), cache, read_limits)
    }

    /// Open from any [`ReadAt`] — a plain file, a container-member extent, or a remote range. The format is
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
        if version != PART_DRAFT_EPOCH {
            bail!(
                "part declares draft epoch {version}; this build accepts exactly {PART_DRAFT_EPOCH}"
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
        let toc_end = toc_off
            .checked_add(u64::from(toc_stored))
            .ok_or_else(|| anyhow::anyhow!("part TOC end overflows"))?;
        if toc_end != len - FOOTER_LEN {
            bail!("part TOC is not immediately adjacent to its footer");
        }
        read_limits.admit("part TOC", u64::from(toc_stored), u64::from(toc_raw))?;
        let mut tbuf = vec![0u8; toc_stored as usize];
        f.read_exact_at(&mut tbuf, toc_off)?;
        if crc32fast::hash(&tbuf) != toc_xsum {
            bail!(
                "part TOC fails its checksum — every section checksum it carries is untrustworthy"
            );
        }
        let toc_bytes = crate::fold::codec::decode(toc_codec, &tbuf, toc_raw, None)?;

        let mut at = 0usize;
        let n = usize::try_from(get_varint(&toc_bytes, &mut at)?)
            .map_err(|_| anyhow::anyhow!("part TOC entry count exceeds this address space"))?;
        // An entry costs several bytes, so the byte count bounds the entry count — checked before
        // the count sizes an allocation.
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
            if name.is_empty() {
                bail!("part TOC carries an empty section name");
            }
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
            if codec > 1 {
                bail!("part TOC entry {name} has unknown codec {codec}");
            }
            if codec == 0 && stored != raw {
                bail!("stored part section {name} has different stored and raw lengths");
            }
            if at + 4 > toc_bytes.len() {
                bail!("part TOC entry {name} is truncated before its checksum");
            }
            let xsum = u32::from_le_bytes(toc_bytes[at..at + 4].try_into().unwrap());
            at += 4;
            // An authentic-but-semantically-invalid TOC could otherwise send `sect` to allocate
            // `stored` bytes and read at an arbitrary offset. Its checksum proves bytes, not range
            // meaning, so every entry is checked against the part it claims to describe.
            let end = off
                .checked_add(u64::from(stored))
                .ok_or_else(|| anyhow::anyhow!("part TOC entry {name} end overflows"))?;
            if end > toc_off {
                bail!("part TOC entry {name} overlaps the TOC or footer");
            }
            if toc.insert(name.clone(), Section { off, stored, raw, codec, xsum }).is_some() {
                bail!("part TOC names section {name:?} more than once");
            }
        }
        if at != toc_bytes.len() {
            bail!("part TOC has {} trailing bytes after its last entry", toc_bytes.len() - at);
        }
        for required in ["ids", "ids.restart", "cmeta", "pdict.loc", "pdict.hash"] {
            if !toc.contains_key(required) {
                bail!("part is missing its required {required} section");
            }
        }
        let mut ranges: Vec<_> = toc
            .iter()
            .filter(|(_, section)| section.stored != 0)
            .map(|(name, section)| {
                let end = section
                    .off
                    .checked_add(u64::from(section.stored))
                    .expect("section end checked while parsing");
                (section.off, end, name.as_str())
            })
            .collect();
        ranges.sort_unstable_by_key(|&(start, _, _)| start);
        for adjacent in ranges.windows(2) {
            if adjacent[0].1 > adjacent[1].0 {
                bail!("part sections {:?} and {:?} overlap", adjacent[0].2, adjacent[1].2);
            }
        }
        let part = Part {
            f,
            toc,
            meta: PartMeta { n_records, seq_lo, seq_hi },
            id: cache::next_part_id(),
            cache,
            read_limits,
        };
        part.validate_current_schema()?;
        // Schema validation is an open-time integrity gate, not query prefetch. Do not let the
        // sections it inspected make the first operation look warm or consume shared cache budget.
        part.cache.forget(part.id);
        Ok(part)
    }

    pub fn meta(&self) -> PartMeta {
        self.meta
    }

    pub fn len(&self) -> usize {
        self.meta.n_records as usize
    }

    pub fn is_empty(&self) -> bool {
        self.meta.n_records == 0
    }

    /// Validate the closed current section schema. A section outside the exact singleton names and
    /// metadata-indexed families below is not part of this draft identity and is refused.
    fn validate_current_schema(&self) -> Result<()> {
        let restart_bytes = u64::from(self.meta.n_records.div_ceil(16))
            .checked_mul(4)
            .context("id restart size overflows")?;
        if u64::from(self.toc["ids.restart"].raw) != restart_bytes {
            bail!("ids.restart length does not match the part's record count");
        }
        let ids = self.sect("ids")?;
        let restarts = self.restarts()?;
        IdCol::new(&ids, &restarts, self.len()).validate()?;

        let loc_bytes = usize::try_from(self.toc["pdict.loc"].raw)
            .context("piece-location length exceeds this platform")?;
        if loc_bytes % Loc::WIDTH != 0 {
            bail!("pdict.loc ends with a partial {}-byte location", Loc::WIDTH);
        }
        let pieces = loc_bytes / Loc::WIDTH;
        let hash_bytes = pieces.checked_mul(32).context("piece-hash section length overflows")?;
        if usize::try_from(self.toc["pdict.hash"].raw).ok() != Some(hash_bytes) {
            bail!("pdict.hash is not parallel to pdict.loc");
        }
        let locations = self.sect("pdict.loc")?;
        let mut previous = None;
        for encoded in locations.chunks_exact(Loc::WIDTH) {
            let location = Loc::decode(encoded)?;
            if location.raw == 0 {
                bail!("pdict.loc contains a zero-length piece");
            }
            if previous.is_some_and(|prior| prior >= location) {
                bail!("pdict.loc is duplicated or out of canonical fold order");
            }
            previous = Some(location);
        }
        let hashes = self.sect("pdict.hash")?;
        let mut unique_hashes = std::collections::HashSet::new();
        unique_hashes
            .try_reserve(pieces)
            .context("reserve piece-identity uniqueness validation")?;
        for encoded in hashes.chunks_exact(32) {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(encoded);
            if !unique_hashes.insert(PieceHash(hash)) {
                bail!("pdict.hash repeats a piece identity");
            }
        }
        let has_hash_order = self.has("pdict.hsort");
        let has_bloom = self.has("pdict.bloom");
        if has_hash_order != has_bloom {
            bail!("pdict.hsort and pdict.bloom must either both be present or both be absent");
        }
        if let Some(section) = self.toc.get("pdict.hsort") {
            let expected = pieces.checked_mul(4).context("piece permutation size overflows")?;
            if usize::try_from(section.raw).ok() != Some(expected) {
                bail!("pdict.hsort is not parallel to the piece dictionary");
            }
            let ordinals = self.nums("pdict.hsort", 4)?;
            let hashes = self.sect("pdict.hash")?;
            let mut seen = vec![false; pieces];
            for (position, &ordinal) in ordinals.iter().enumerate() {
                let ordinal = usize::try_from(ordinal)
                    .context("piece permutation ordinal exceeds this platform")?;
                if ordinal >= pieces {
                    bail!("piece permutation points outside the piece dictionary");
                }
                if std::mem::replace(&mut seen[ordinal], true) {
                    bail!("piece permutation repeats ordinal {ordinal}");
                }
                if position > 0 {
                    let prior = ordinals[position - 1] as usize;
                    let prior_hash = &hashes[prior * 32..prior * 32 + 32];
                    let hash = &hashes[ordinal * 32..ordinal * 32 + 32];
                    if prior_hash >= hash {
                        bail!("piece permutation is not in strict piece-identity order");
                    }
                }
            }
        }
        if has_bloom {
            bloom::Bloom::validate_current(&self.sect("pdict.bloom")?, &self.sect("pdict.hash")?)?;
        }

        let contents = self.content_meta()?;
        for (column, content) in contents.iter().enumerate() {
            let program_name = format!("con.prog.{column}");
            let offset_name = format!("con.off.{column}");
            let offsets = self.nums(&offset_name, 8)?;
            if offsets.first().copied() != Some(0) {
                bail!("content {:?} program offsets must begin at zero", content.name);
            }
            let program_len = u64::from(self.toc[&program_name].raw);
            if offsets.windows(2).any(|pair| pair[0] > pair[1]) {
                bail!("content {:?} program offsets are not monotonic", content.name);
            }
            if offsets.last().copied() != Some(program_len) {
                bail!(
                    "content {:?} final program offset does not equal its program extent",
                    content.name
                );
            }
        }
        self.reject_dangling_family("con.prog.", contents.len())?;
        self.reject_dangling_family("con.off.", contents.len())?;
        self.reject_dangling_family("con.id.", contents.len())?;
        self.reject_dangling_family("con.rid.", contents.len())?;

        let shared = ["layout", "layout.off", "colmeta"];
        let shared_count = shared.iter().filter(|name| self.has(name)).count();
        if shared_count != 0 && shared_count != shared.len() {
            bail!("attribute layout, offsets, and metadata must be present or absent together");
        }
        let attribute_count = if shared_count == 0 {
            0
        } else {
            let expected_offsets = u64::from(self.meta.n_records)
                .checked_add(1)
                .and_then(|count| count.checked_mul(8))
                .context("attribute layout-offset size overflows")?;
            if u64::from(self.toc["layout.off"].raw) != expected_offsets {
                bail!("layout.off length does not match the part's record count");
            }
            let meta = attrs::read_meta(self)?;
            if meta.is_empty() {
                bail!("attribute sections are present but declare zero columns");
            }
            attrs::validate_layout(self, &meta)?;
            for (column, (_, tag, occurrences, kind)) in meta.iter().enumerate() {
                let values = format!("col.val.{column}");
                let rids = format!("col.rid.{column}");
                let dictionary = format!("col.dict.{column}");
                if !self.has(&values) {
                    bail!("attribute column {column} is missing {values}");
                }
                let expected_values = occurrences
                    .checked_mul(attrs::width(*tag))
                    .context("attribute value-section size overflows")?;
                if usize::try_from(self.toc[&values].raw).ok() != Some(expected_values) {
                    bail!("attribute column {column} value length disagrees with colmeta");
                }
                match *kind {
                    attrs::RID_DENSE if *occurrences == self.len() => {
                        if self.has(&rids) {
                            bail!("dense attribute column {column} must not carry {rids}");
                        }
                    }
                    attrs::RID_DENSE => {
                        bail!("dense attribute column {column} does not cover every record")
                    }
                    attrs::RID_DELTA => {
                        if !self.has(&rids) {
                            bail!("sparse attribute column {column} is missing {rids}");
                        }
                    }
                    other => bail!("attribute column {column} has unknown row-id kind {other}"),
                }
                if matches!(*tag, 0 | 5) != self.has(&dictionary) {
                    bail!("attribute column {column} has an invalid dictionary complement");
                }
            }
            meta.len()
        };
        self.reject_dangling_family("col.val.", attribute_count)?;
        self.reject_dangling_family("col.rid.", attribute_count)?;
        self.reject_dangling_family("col.dict.", attribute_count)?;
        if shared_count == 0 && self.has("zone") {
            bail!("an attribute-free part must not carry an attribute zone section");
        }

        for name in self.toc.keys() {
            let singleton = matches!(
                name.as_str(),
                "ids"
                    | "ids.restart"
                    | "cmeta"
                    | "pdict.loc"
                    | "pdict.hash"
                    | "pdict.hsort"
                    | "pdict.bloom"
                    | "tomb"
                    | "layout"
                    | "layout.off"
                    | "colmeta"
                    | "zone"
            );
            let indexed = ["con.prog.", "con.off.", "con.id.", "con.rid."]
                .iter()
                .any(|prefix| name.starts_with(prefix))
                || ["col.val.", "col.rid.", "col.dict."]
                    .iter()
                    .any(|prefix| name.starts_with(prefix));
            if !singleton && !indexed {
                bail!("part carries unknown section {name:?} outside the current draft schema");
            }
        }

        // Decode the optional compact list now so partial entries, duplicate ordinals, overflow,
        // and out-of-range rows cannot remain latent behind a seemingly successful open.
        let _ = self.tombstones()?;
        Ok(())
    }

    fn reject_dangling_family(&self, prefix: &str, count: usize) -> Result<()> {
        for name in self.toc.keys().filter(|name| name.starts_with(prefix)) {
            let suffix = &name[prefix.len()..];
            let ordinal = suffix.parse::<usize>().with_context(|| {
                format!("known section family {prefix} has invalid member {name}")
            })?;
            if suffix != ordinal.to_string() || ordinal >= count {
                bail!("known section {name} has no corresponding metadata entry");
            }
        }
        Ok(())
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
        let got = crc32fast::hash(&buf);
        if got != s.xsum {
            bail!("part section {name:?} fails its checksum: {got:08x} != {:08x}", s.xsum);
        }
        let raw = crate::fold::codec::decode(s.codec, &buf, s.raw, None)?;
        let arc = Arc::new(raw);
        self.cache.put(self.id, k, Held::Bytes(arc.clone()));
        crate::io_trace::part_section(self.id, name, false, s.stored, s.raw);
        Ok(arc)
    }

    /// Decode advisory bytes only after their stored checksum is proved. A damaged advisory
    /// section is absence, never evidence for excluding rows. Admission and I/O failures still
    /// propagate because they are not statements about the advisory bytes' truth.
    pub(crate) fn verified_advisory_section(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let s = self.toc.get(name).ok_or_else(|| anyhow::anyhow!("part has no section {name}"))?;
        self.read_limits.admit(
            format!("part advisory section {name:?}"),
            u64::from(s.stored),
            u64::from(s.raw),
        )?;
        let mut stored = vec![0u8; s.stored as usize];
        self.f.read_exact_at(&mut stored, s.off)?;
        if crc32fast::hash(&stored) != s.xsum {
            return Ok(None);
        }
        Ok(crate::fold::codec::decode(s.codec, &stored, s.raw, None).ok())
    }

    fn has(&self, name: &str) -> bool {
        self.toc.contains_key(name)
    }

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
    /// Every uncached section read verifies that section's stored checksum, and open-time schema
    /// validation reads the structural sections it needs. This method is the explicit whole-part
    /// sweep: it touches every section whether or not a query needs it, making the proportional
    /// whole-part cost a deliberate verification choice rather than a tax on every scan.
    pub fn verify_sections(&self) -> Result<usize> {
        self.verify_sections_with_control(&crate::control::OperationControl::default())
    }

    /// [`Part::verify_sections`] with cooperative checkpoints during bounded section reads.
    pub fn verify_sections_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<usize> {
        let mut checked = 0usize;
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

    /// Decode the complete logical grammar of every physical row and dictionary entry.
    ///
    /// This is deliberately stronger than resolving the current record set: superseded rows are
    /// still current-format bytes, and backup, reclaim, and retained-history verification must not
    /// preserve an authenticated part whose latent row program or attribute value is malformed.
    pub fn verify_semantics_with_control(
        &self,
        control: &crate::control::OperationControl,
    ) -> Result<usize> {
        let piece_count = self.piece_count()?;
        for ordinal in 0..piece_count {
            control.check("part semantic verification")?;
            let _ = self.piece(ordinal)?;
        }

        for row in 0..self.len() {
            control.check("part semantic verification")?;
            let _ = self.record(row)?;
        }
        Ok(self.len())
    }

    /// Verify every operational piece-dictionary mapping against the fold bytes it names.
    ///
    /// Entries in declared-punched blocks are historical lookup residue and are deliberately
    /// unavailable; callers must not use them for dedup. Every other hash/location pair is
    /// authority-bearing even when no current record program references it.
    pub(crate) fn verify_piece_dictionary_with_control(
        &self,
        fold: &Fold,
        control: &crate::control::OperationControl,
    ) -> Result<usize> {
        let count = self.piece_count()?;
        let mut verified = 0usize;
        for ordinal in 0..count {
            control.check("piece dictionary verification")?;
            let (location, hash) = self.piece(ordinal)?;
            fold.verify_location_shape(location).with_context(|| {
                format!("piece dictionary ordinal {ordinal} has an invalid fold location")
            })?;
            if fold.is_punched(location.block_id) {
                continue;
            }
            fold.read_verified(location, hash).with_context(|| {
                format!("piece dictionary ordinal {ordinal} maps {hash} to invalid fold bytes")
            })?;
            verified = verified
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("verified piece count overflow"))?;
        }
        Ok(verified)
    }

    /// Rows this part deletes, ascending. Empty exactly when this part deletes nothing.
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
        let n = usize::try_from(get_varint(&b, &mut at)?)
            .context("tombstone count exceeds this platform's address space")?;
        let mut out = Vec::with_capacity(n.min(b.len()));
        let mut cur = 0u64;
        for index in 0..n {
            let delta = get_varint(&b, &mut at)?;
            if index > 0 && delta == 0 {
                bail!("tombstone ordinals are not strictly increasing");
            }
            cur = cur.checked_add(delta).context("tombstone ordinal overflows")?;
            if cur >= self.len() as u64 {
                bail!("tombstone ordinal {cur} is outside this part");
            }
            out.push(cur);
        }
        if at != b.len() {
            bail!("tombstone section has {} trailing bytes", b.len() - at);
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
        let locations = self.sect("pdict.loc")?;
        if locations.len() % Loc::WIDTH != 0 {
            bail!("piece-location section has a partial {}-byte entry", Loc::WIDTH);
        }
        Ok(locations.len() / Loc::WIDTH)
    }

    /// **Tier-1 dedup.** Does this part already hold `h`, and if so, where in the fold?
    ///
    /// Filter first — that is the whole point, since at high duplication almost every write asks this
    /// question of every part and nearly all answers are "no". Only on a filter hit does the sorted
    /// permutation get searched, and only then is the answer definitive.
    ///
    /// These indexes are advisory in the current format. A part that omits either declines to
    /// participate in hash-indexed dedup; a miss costs work and bytes, never correctness.
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

    /// Authoritative piece-dictionary lookup. Advisory indexes accelerate the common case; their
    /// absence widens to a linear scan and can never make stored content disappear.
    pub(crate) fn find_piece(&self, h: &PieceHash) -> Result<Option<Loc>> {
        if let Some(loc) = self.lookup_piece(h)? {
            return Ok(Some(loc));
        }
        for ordinal in 0..self.piece_count()? {
            let (loc, candidate) = self.piece(ordinal)?;
            if candidate == *h {
                return Ok(Some(loc));
            }
        }
        Ok(None)
    }

    /// The named content columns this part declares, in canonical UTF-8 byte order.
    pub fn content_meta(&self) -> Result<Arc<Vec<ContentMeta>>> {
        if let Some(Held::ContentMeta(v)) = self.cache.get(self.id, &Kind::ContentMeta) {
            return Ok(v);
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
            if occurrences == 0 {
                bail!("content column {i} has no occurrences");
            }
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
            if !self.has(&identity) {
                bail!("content {name:?} is missing its identity section");
            }
            let expected = (occurrences as u64)
                .checked_mul(32)
                .ok_or_else(|| anyhow::anyhow!("content identity section size overflows"))?;
            if self.toc[&identity].raw as u64 != expected {
                bail!("content {name:?} has an identity count inconsistent with cmeta");
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
        if occurrence >= offs.len().saturating_sub(1) {
            bail!("content occurrence {occurrence} is outside {off_name}");
        }
        let start = usize::try_from(offs[occurrence])
            .context("content program start exceeds this platform")?;
        let end = usize::try_from(offs[occurrence + 1])
            .context("content program end exceeds this platform")?;
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
                if len == 0 {
                    bail!("content program piece length must be non-zero");
                }
                let (location, hash) = self.piece(idx)?;
                if location.raw != len {
                    bail!(
                        "content program says piece {hash} is {len} bytes but its dictionary says {}",
                        location.raw
                    );
                }
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
        let columns = self.content_meta()?;
        let Ok(col) = columns.binary_search_by(|c| c.name.as_bytes().cmp(name.as_bytes())) else {
            return Ok(None);
        };
        let Some(occurrence) = self.content_occurrence(col, &columns[col], r)? else {
            return Ok(None);
        };
        Ok(Some(self.program(&format!("con.prog.{col}"), &format!("con.off.{col}"), occurrence)?))
    }

    /// Exact reconstructed-byte identity for one named value. `None` means the value is absent.
    /// No program or fold block is read.
    pub fn content_identity(&self, r: usize, name: &str) -> Result<Option<ContentHash>> {
        if r >= self.len() {
            bail!("row {r} out of range");
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
            .checked_mul(32)
            .ok_or_else(|| anyhow::anyhow!("content identity offset overflows"))?;
        let end = at
            .checked_add(32)
            .ok_or_else(|| anyhow::anyhow!("content identity end offset overflows"))?;
        let encoded = identities
            .get(at..end)
            .ok_or_else(|| anyhow::anyhow!("content identity occurrence is truncated"))?;
        Ok(Some(ContentHash(encoded.try_into().unwrap())))
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
            let identities = self.sect(&format!("con.id.{col}"))?;

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
                let at = occurrence
                    .checked_mul(32)
                    .ok_or_else(|| anyhow::anyhow!("content identity offset overflows"))?;
                let identity_end = at
                    .checked_add(32)
                    .ok_or_else(|| anyhow::anyhow!("content identity end offset overflows"))?;
                let encoded = identities
                    .get(at..identity_end)
                    .ok_or_else(|| anyhow::anyhow!("content identity occurrence is truncated"))?;
                let identity = Some(ContentHash(encoded.try_into().unwrap()));
                let mut content = Content::new(&meta.name, self.decode_program(&prog[start..end])?);
                content.identity = identity;
                out[output].push(content);
            }
        }
        Ok(out)
    }

    /// Convenience access to the conventional `body` content value.
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

    /// Whether this part can possibly contain an occurrence satisfying one typed attribute
    /// predicate. `false` is a proof from dictionary/zone metadata; every absent, unknown, NaN, or
    /// malformed advisory fact widens to `true` so pruning can never change an answer.
    pub fn attr_predicate_may_match(
        &self,
        name: &str,
        op: crate::scan::Compare,
        value: &AttrValue,
    ) -> Result<bool> {
        let wanted_tag = value.type_tag();
        let meta = attrs::read_meta(self)?;
        let columns: Vec<_> = meta
            .iter()
            .enumerate()
            .filter(|(_, (column_name, tag, _, _))| column_name == name && *tag == wanted_tag)
            .map(|(ordinal, _)| ordinal)
            .collect();
        if columns.is_empty() {
            return Ok(false);
        }
        for column in columns {
            if wanted_tag == 0 {
                let AttrValue::Str(needle) = value else { unreachable!() };
                let dictionary = attrs::read_dict(self, column)?;
                let may = match op {
                    crate::scan::Compare::Eq => dictionary.binary_search(needle).is_ok(),
                    crate::scan::Compare::Ne => {
                        dictionary.len() != 1 || dictionary.first() != Some(needle)
                    }
                    crate::scan::Compare::Lt => {
                        dictionary.first().is_some_and(|first| first < needle)
                    }
                    crate::scan::Compare::LtEq => {
                        dictionary.first().is_some_and(|first| first <= needle)
                    }
                    crate::scan::Compare::Gt => dictionary.last().is_some_and(|last| last > needle),
                    crate::scan::Compare::GtEq => {
                        dictionary.last().is_some_and(|last| last >= needle)
                    }
                };
                if may {
                    return Ok(true);
                }
                continue;
            }
            let Some((minimum, maximum)) = self.zone(column)? else {
                return Ok(true);
            };
            let may = match op {
                crate::scan::Compare::Eq => {
                    scalar_order(&minimum, value).is_some_and(|order| !order.is_gt())
                        && scalar_order(&maximum, value).is_some_and(|order| !order.is_lt())
                }
                // A zone cannot prove `ne` for floats because ±0 have equal numeric order but
                // distinct contract equality. Other types can prune a constant equal column.
                crate::scan::Compare::Ne if wanted_tag == 2 => true,
                crate::scan::Compare::Ne => minimum != *value || maximum != *value,
                crate::scan::Compare::Lt => {
                    scalar_order(&minimum, value).is_none_or(|order| order.is_lt())
                }
                crate::scan::Compare::LtEq => {
                    scalar_order(&minimum, value).is_none_or(|order| !order.is_gt())
                }
                crate::scan::Compare::Gt => {
                    scalar_order(&maximum, value).is_none_or(|order| order.is_gt())
                }
                crate::scan::Compare::GtEq => {
                    scalar_order(&maximum, value).is_none_or(|order| !order.is_lt())
                }
            };
            if may {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn has_attribute_name(&self, name: &str) -> Result<bool> {
        Ok(attrs::read_meta(self)?.iter().any(|(column, _, _, _)| column == name))
    }

    pub fn has_content_name(&self, name: &str) -> Result<bool> {
        Ok(self.content_meta()?.iter().any(|content| content.name == name))
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
                    let loc = self.find_piece(hash)?;
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

    /// Validate one projected content value through this Part's own piece dictionary while
    /// hashing incrementally. The complete value is never materialized; Fold frames and Part
    /// sections remain independently bounded by the caller's read profile.
    pub(crate) fn verify_projected_content_with_control(
        &self,
        content: &Content,
        fold: &Fold,
        control: &crate::control::OperationControl,
    ) -> Result<u64> {
        let (bytes, complete) =
            self.verify_projected_content_inner(content, fold, control, false)?;
        debug_assert!(complete);
        Ok(bytes)
    }

    /// Validate as much of a retained content value as the current punch declaration leaves
    /// readable. A declared-punched piece has already ended that older view's readability, so its
    /// complete value identity cannot be recomputed; every surviving piece and all locator
    /// geometry remain mandatory.
    pub(crate) fn verify_retained_projected_content_with_control(
        &self,
        content: &Content,
        fold: &Fold,
        control: &crate::control::OperationControl,
    ) -> Result<u64> {
        self.verify_projected_content_inner(content, fold, control, true).map(|(bytes, _)| bytes)
    }

    fn verify_projected_content_inner(
        &self,
        content: &Content,
        fold: &Fold,
        control: &crate::control::OperationControl,
        allow_declared_punch: bool,
    ) -> Result<(u64, bool)> {
        let mut hasher = blake3::Hasher::new();
        let mut bytes = 0u64;
        let mut complete = true;
        for op in &content.ops {
            control.check("content identity verification")?;
            match op {
                BodyOp::Lit(literal) => {
                    hasher.update(literal);
                    bytes = bytes
                        .checked_add(literal.len() as u64)
                        .ok_or_else(|| anyhow::anyhow!("content byte count overflow"))?;
                }
                BodyOp::Piece { hash, len } => {
                    let location = self.find_piece(hash)?.ok_or_else(|| {
                        anyhow::anyhow!("piece {hash} is not in the owning part dictionary")
                    })?;
                    if location.raw != *len {
                        bail!("piece {hash} is {} bytes but the program says {len}", location.raw);
                    }
                    fold.verify_location_shape(location)?;
                    if allow_declared_punch && fold.is_punched(location.block_id) {
                        complete = false;
                    } else {
                        fold.visit_verified(location, *hash, |piece| {
                            hasher.update(piece);
                        })?;
                    }
                    bytes = bytes
                        .checked_add(u64::from(*len))
                        .ok_or_else(|| anyhow::anyhow!("content byte count overflow"))?;
                }
            }
        }
        let expected = content.identity.ok_or_else(|| {
            anyhow::anyhow!("content {:?} has no reconstructed-byte identity", content.name)
        })?;
        if complete {
            let got = ContentHash(hasher.finalize().into());
            if got != expected {
                bail!("content {:?} hashes to {got} but claims {expected}", content.name);
            }
        }
        Ok((bytes, complete))
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let file_bytes = crate::vfs::read_file(&path).unwrap();

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
        c.abandon_member(w).unwrap();

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

    fn empty_current_writer(path: &Path) -> Writer<FilePartSink> {
        let mut writer = Writer::new(path, 3).unwrap();
        writer.section("ids", &[]).unwrap();
        writer.section("ids.restart", &[]).unwrap();
        writer.section("cmeta", &[0]).unwrap();
        writer.section("pdict.loc", &[]).unwrap();
        writer.section("pdict.hash", &[]).unwrap();
        writer
    }

    #[test]
    fn every_required_section_and_disjoint_extent_is_enforced() {
        let (dir, missing_path) = temp_part("missing-required");
        let mut missing = Writer::new(&missing_path, 3).unwrap();
        missing.section("cmeta", &[0]).unwrap();
        missing.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();
        assert!(Part::open(&missing_path).is_err());

        let overlap_path = dir.join("overlap.part");
        let mut overlap = empty_current_writer(&overlap_path);
        let ids = overlap.toc.iter().position(|(name, _)| name == "ids").unwrap();
        let cmeta = overlap.toc.iter().position(|(name, _)| name == "cmeta").unwrap();
        overlap.toc[ids].1.stored = 1;
        overlap.toc[ids].1.raw = 1;
        overlap.toc[cmeta].1.off = overlap.toc[ids].1.off;
        overlap.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();
        let error = match Part::open(&overlap_path) {
            Ok(_) => panic!("overlapping current sections must refuse"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("overlap"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn malformed_tombstone_ordinals_are_refused_during_open() {
        let (dir, path) = temp_part("bad-tombstones");
        let mut writer = Writer::new(&path, 3).unwrap();
        writer.section("ids", &[0]).unwrap();
        writer.section("ids.restart", &0u32.to_le_bytes()).unwrap();
        writer.section("cmeta", &[0]).unwrap();
        writer.section("pdict.loc", &[]).unwrap();
        writer.section("pdict.hash", &[]).unwrap();
        writer.section("tomb", &[2, 0, 0]).unwrap();
        writer.finish(PartMeta { n_records: 2, seq_lo: 1, seq_hi: 1 }).unwrap();
        assert!(Part::open(&path).is_err(), "duplicate tombstone ordinals must refuse");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_unknown_section_is_not_a_silent_extension_of_the_current_draft() {
        let (dir, path) = temp_part("unknown-section");
        let mut writer = empty_current_writer(&path);
        writer.section("future.guess", b"not this draft").unwrap();
        writer.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();
        let error = Part::open(&path)
            .err()
            .expect("an unlisted section must require a new physical identity")
            .to_string();
        assert!(error.contains("unknown section"), "unexpected refusal: {error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_piece_hash_order_section_must_be_an_exact_sorted_permutation() {
        let (dir, path) = temp_part("bad-piece-permutation");
        let mut writer = Writer::new(&path, 3).unwrap();
        writer.section("ids", &[]).unwrap();
        writer.section("ids.restart", &[]).unwrap();
        writer.section("cmeta", &[0]).unwrap();
        let mut locations = Vec::new();
        locations.extend_from_slice(&Loc { block_id: 0, in_off: 0, raw: 1 }.encode());
        locations.extend_from_slice(&Loc { block_id: 0, in_off: 1, raw: 1 }.encode());
        writer.section("pdict.loc", &locations).unwrap();
        let mut hashes = vec![0u8; 64];
        hashes[32] = 1;
        writer.section("pdict.hash", &hashes).unwrap();
        writer.section("pdict.hsort", &u32s(&[0, 0])).unwrap();
        let mut filter = bloom::Bloom::with_capacity(2);
        let mut first = [0u8; 32];
        first.copy_from_slice(&hashes[..32]);
        let mut second = [0u8; 32];
        second.copy_from_slice(&hashes[32..]);
        filter.insert(&PieceHash(first));
        filter.insert(&PieceHash(second));
        writer.section("pdict.bloom", &filter.encode()).unwrap();
        writer.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();

        let error =
            Part::open(&path).err().expect("a repeated piece ordinal must be refused").to_string();
        assert!(error.contains("repeats ordinal"), "unexpected refusal: {error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_zero_length_piece_dictionary_entry_is_not_current_format() {
        let (dir, path) = temp_part("zero-piece");
        let mut writer = empty_current_writer(&path);
        let loc = Loc { block_id: 0, in_off: 0, raw: 0 };
        let hash = PieceHash::of(b"");
        let loc_index = writer.toc.iter().position(|(name, _)| name == "pdict.loc").unwrap();
        let hash_index = writer.toc.iter().position(|(name, _)| name == "pdict.hash").unwrap();
        writer.toc.remove(hash_index.max(loc_index));
        writer.toc.remove(hash_index.min(loc_index));
        writer.section("pdict.loc", &loc.encode()).unwrap();
        writer.section("pdict.hash", &hash.0).unwrap();
        writer.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();

        let error =
            Part::open(&path).err().expect("a zero-length physical piece must refuse").to_string();
        assert!(error.contains("zero-length piece"), "unexpected refusal: {error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn authoritative_piece_lookup_does_not_depend_on_advisory_indexes() {
        let (dir, path) = temp_part("piece-lookup-without-advisory-indexes");
        let loc = Loc { block_id: 7, in_off: 11, raw: 13 };
        let hash = PieceHash::of(b"thirteen-byte piece");
        let mut writer = Writer::new(&path, 3).unwrap();
        writer.section("ids", &[]).unwrap();
        writer.section("ids.restart", &[]).unwrap();
        writer.section("cmeta", &[0]).unwrap();
        writer.section("pdict.loc", &loc.encode()).unwrap();
        writer.section("pdict.hash", &hash.0).unwrap();
        writer.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();

        let part = Part::open(&path).unwrap();
        assert_eq!(
            part.lookup_piece(&hash).unwrap(),
            None,
            "the fast index is deliberately absent"
        );
        assert_eq!(part.find_piece(&hash).unwrap(), Some(loc));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn base_piece_dictionary_identities_are_unique_without_advisory_indexes() {
        let (dir, path) = temp_part("duplicate-piece-identity-without-indexes");
        let mut writer = Writer::new(&path, 3).unwrap();
        writer.section("ids", &[]).unwrap();
        writer.section("ids.restart", &[]).unwrap();
        writer.section("cmeta", &[0]).unwrap();
        let mut locations = Vec::new();
        locations.extend_from_slice(&Loc { block_id: 0, in_off: 0, raw: 1 }.encode());
        locations.extend_from_slice(&Loc { block_id: 0, in_off: 1, raw: 1 }.encode());
        writer.section("pdict.loc", &locations).unwrap();
        let hash = PieceHash::of(b"x");
        let mut hashes = Vec::new();
        hashes.extend_from_slice(&hash.0);
        hashes.extend_from_slice(&hash.0);
        writer.section("pdict.hash", &hashes).unwrap();
        writer.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();

        let error = Part::open(&path)
            .err()
            .expect("duplicate base piece identities must refuse without advisory indexes")
            .to_string();
        assert!(error.contains("repeats a piece identity"), "unexpected refusal: {error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn advisory_piece_indexes_are_a_complete_non_false_negative_pair() {
        let (dir, half_path) = temp_part("half-piece-index");
        let mut half = empty_current_writer(&half_path);
        half.section("pdict.hsort", &[]).unwrap();
        half.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();
        let error = Part::open(&half_path)
            .err()
            .expect("a half-present advisory index pair must refuse")
            .to_string();
        assert!(error.contains("both be present or both be absent"), "unexpected refusal: {error}");

        let false_negative_path = dir.join("false-negative.part");
        let loc = Loc { block_id: 7, in_off: 11, raw: 13 };
        let hash = PieceHash::of(b"thirteen-byte piece");
        let mut writer = Writer::new(&false_negative_path, 3).unwrap();
        writer.section("ids", &[]).unwrap();
        writer.section("ids.restart", &[]).unwrap();
        writer.section("cmeta", &[0]).unwrap();
        writer.section("pdict.loc", &loc.encode()).unwrap();
        writer.section("pdict.hash", &hash.0).unwrap();
        writer.section("pdict.hsort", &0u32.to_le_bytes()).unwrap();
        let mut bloom = bloom::Bloom::with_capacity(1).encode();
        bloom[8..].fill(0);
        writer.section("pdict.bloom", &bloom).unwrap();
        writer.finish(PartMeta { n_records: 0, seq_lo: 1, seq_hi: 1 }).unwrap();
        let error = Part::open(&false_negative_path)
            .err()
            .expect("an authenticated advisory false negative must refuse")
            .to_string();
        assert!(error.contains("false negative"), "unexpected refusal: {error}");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn full_semantic_verification_decodes_values_in_every_physical_row() {
        use std::io::{Seek, SeekFrom, Write};

        let (dir, path) = temp_part("semantic-all-rows");
        let records = vec![crate::types::Record {
            id: "superseded:1".into(),
            contents: vec![crate::types::Content::identified(
                "body",
                vec![crate::types::ContentOp::Lit(b"bytes".to_vec())],
                crate::types::ContentHash::of(b"bytes"),
            )],
            attrs: vec![("flag".into(), AttrValue::Bool(true))],
        }];
        build_full(&path, &records, &[], 1, 1, 3, |_| None, &HashMap::new()).unwrap();
        let mut part = Part::open(&path).unwrap();
        let section = part.toc["col.val.0"].clone();
        assert_eq!(section.stored, 1, "the boolean fixture is one stored byte");

        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(section.off)).unwrap();
        file.write_all(&[2]).unwrap();
        file.sync_all().unwrap();
        part.toc.get_mut("col.val.0").unwrap().xsum = crc32fast::hash(&[2]);

        part.verify_sections().expect("the simulated current bytes have a matching checksum");
        let error = part
            .verify_semantics_with_control(&crate::control::OperationControl::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid boolean"), "unexpected semantic refusal: {error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn piece_dictionary_verification_checks_unreferenced_authority_against_fold_bytes() {
        let (dir, path) = temp_part("piece-dictionary-fold-proof");
        let fold_path = dir.join("fold");
        let mut fold = Fold::open(&fold_path, crate::fold::FoldCfg::default()).unwrap();
        let actual = fold.put(b"actual").unwrap();
        fold.sync().unwrap();

        let claimed = PieceHash::of(b"claims");
        let retained = HashMap::from([(claimed, actual.loc)]);
        build_full(&path, &[], &[], 1, 1, 3, |_| None, &retained).unwrap();
        let part = Part::open(&path).unwrap();
        let error = part
            .verify_piece_dictionary_with_control(
                &fold,
                &crate::control::OperationControl::default(),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid fold bytes"), "unexpected refusal: {error}");

        let punched_shape_path = dir.join("punched-bad-shape.part");
        let punched_hash = PieceHash::of(b"z");
        let punched_shape = HashMap::from([(
            punched_hash,
            Loc { block_id: actual.loc.block_id, in_off: u32::MAX, raw: 1 },
        )]);
        build_full(&punched_shape_path, &[], &[], 1, 1, 3, |_| None, &punched_shape).unwrap();
        let punched_shape_part = Part::open(&punched_shape_path).unwrap();
        fold.declare_punched(&[(actual.loc.block_id, actual.loc.block_id)]);
        let error = punched_shape_part
            .verify_piece_dictionary_with_control(
                &fold,
                &crate::control::OperationControl::default(),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid fold location"), "{error}");

        let future_path = dir.join("future-punched-piece.part");
        let future_hash = PieceHash::of(b"future");
        let future = HashMap::from([(future_hash, Loc { block_id: 7, in_off: 0, raw: 6 })]);
        build_full(&future_path, &[], &[], 1, 1, 3, |_| None, &future).unwrap();
        let future_part = Part::open(&future_path).unwrap();
        fold.declare_punched(&[(7, 7)]);
        let error = future_part
            .verify_piece_dictionary_with_control(
                &fold,
                &crate::control::OperationControl::default(),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid fold location"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn attribute_layout_occurrences_must_match_their_column_row_ids() {
        use std::io::{Seek, SeekFrom, Write};

        let (dir, path) = temp_part("layout-rid-agreement");
        let records = vec![
            crate::types::Record {
                id: "a".into(),
                contents: Vec::new(),
                attrs: vec![
                    ("dup".into(), AttrValue::Bool(true)),
                    ("dup".into(), AttrValue::Bool(false)),
                ],
            },
            crate::types::Record { id: "b".into(), contents: Vec::new(), attrs: Vec::new() },
        ];
        build_full(&path, &records, &[], 1, 1, 3, |_| None, &HashMap::new()).unwrap();
        let mut part = Part::open(&path).unwrap();
        let section = part.toc["col.rid.0"].clone();
        assert_eq!((section.stored, section.raw, section.codec), (2, 2, 0));

        // The writer encoded row ids [0, 0] as deltas [0, 0]. Authenticate the equally framed
        // but contradictory [0, 1], while leaving the layout's two row-zero occurrences intact.
        let replacement = [0u8, 1u8];
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(section.off)).unwrap();
        file.write_all(&replacement).unwrap();
        file.sync_all().unwrap();
        part.toc.get_mut("col.rid.0").unwrap().xsum = crc32fast::hash(&replacement);
        part.cache.forget(part.id);

        let meta = attrs::read_meta(&part).unwrap();
        let error = attrs::validate_layout(&part, &meta).unwrap_err().to_string();
        assert!(error.contains("disagrees with its row ids"), "unexpected refusal: {error}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn public_part_build_refuses_unreopenable_names_before_creating_an_artifact() {
        let (dir, empty_id_path) = temp_part("writer-empty-id");
        let empty_id =
            crate::types::Record { id: String::new(), contents: Vec::new(), attrs: Vec::new() };
        let error =
            build_full(&empty_id_path, &[empty_id], &[], 1, 1, 3, |_| None, &HashMap::new())
                .unwrap_err()
                .to_string();
        assert!(error.contains("record id must not be empty"), "unexpected refusal: {error}");
        assert!(!empty_id_path.exists(), "name validation must precede artifact creation");

        let empty_key_path = dir.join("empty-key.part");
        let empty_key = crate::types::Record {
            id: "present".into(),
            contents: Vec::new(),
            attrs: vec![(String::new(), AttrValue::Null)],
        };
        let error =
            build_full(&empty_key_path, &[empty_key], &[], 1, 1, 3, |_| None, &HashMap::new())
                .unwrap_err()
                .to_string();
        assert!(error.contains("attribute name must not be empty"), "unexpected refusal: {error}");
        assert!(!empty_key_path.exists(), "name validation must precede artifact creation");

        let mismatched_path = dir.join("mismatched-piece.part");
        let hash = PieceHash::of(b"piece");
        let record = crate::types::Record {
            id: "piece-record".into(),
            contents: vec![crate::types::Content::identified(
                "body",
                vec![BodyOp::Piece { hash, len: 5 }],
                crate::types::ContentHash::of(b"piece"),
            )],
            attrs: Vec::new(),
        };
        let error = build_full(
            &mismatched_path,
            &[record],
            &[],
            1,
            1,
            3,
            |_| Some(Loc { block_id: 0, in_off: 0, raw: 4 }),
            &HashMap::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("fold location says 4"), "unexpected refusal: {error}");
        assert!(
            !mismatched_path.exists(),
            "piece mapping validation must precede artifact creation"
        );

        let retained_path = dir.join("zero-retained-piece.part");
        let retained =
            HashMap::from([(PieceHash::of(b""), Loc { block_id: 0, in_off: 0, raw: 0 })]);
        let error = build_full(&retained_path, &[], &[], 1, 1, 3, |_| None, &retained)
            .unwrap_err()
            .to_string();
        assert!(error.contains("zero-length fold location"), "unexpected refusal: {error}");
        assert!(!retained_path.exists(), "retained mapping validation must precede creation");
        std::fs::remove_dir_all(dir).ok();
    }
}
