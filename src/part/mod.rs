//! A **part**: an immutable, self-contained, id-sorted columnar slice of the store.
//!
//! A part holds record identity, the flat body programs that reconstruct content out of the fold, the
//! piece dictionary those programs reference, and the typed attribute columns. It holds no content —
//! content lives in the fold and is shared by every part. That is what makes merging parts cheap:
//! a merge rewrites references and columns, never bytes.
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
pub mod cache;
pub mod idcol;
pub mod builder;
pub mod merge;

use crate::fold::{Fold, Loc};
use crate::readat::ReadAt;
use crate::types::{AttrValue, BodyOp, PieceHash, Record};
use anyhow::{bail, Context, Result};
use idcol::{get_varint, put_varint, IdCol};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use cache::{Held, Kind, SectionCache};
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
pub const PART_VERSION: u8 = 1;

/// Body-program op tags, packed into the low bit of a varint.
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
pub fn build_full(
    path: &Path,
    records: &[Record],
    tombs: &[bool],
    seq_lo: u64,
    seq_hi: u64,
    level: i32,
    mut resolve: impl FnMut(&PieceHash) -> Option<Loc>,
    retain: &HashMap<PieceHash, Loc>,
) -> Result<PartMeta> {
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
        for op in &r.body {
            if let BodyOp::Piece { hash, .. } = op {
                if !piece_of.contains_key(hash) {
                    let loc = resolve(hash)
                        .ok_or_else(|| anyhow::anyhow!("piece {hash} is referenced but not in the fold"))?;
                    piece_of.insert(*hash, loc);
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

    // ---- body programs ----
    let mut prog = Vec::new();
    let mut prog_off: Vec<u64> = Vec::with_capacity(ids.len() + 1);
    for &ri in &order {
        prog_off.push(prog.len() as u64);
        let r = &records[ri];
        // An EMPTY literal would encode as tagged == 0, which is the reserved escape codepoint. It
        // also contributes nothing to the body, so dropping it preserves byte-exactness exactly — but
        // the op COUNT is written before the ops, so it must be the count of what is actually emitted.
        let emitted = r.body.iter().filter(|op| !matches!(op, BodyOp::Lit(b) if b.is_empty())).count();
        put_varint(&mut prog, emitted as u64);
        for op in &r.body {
            match op {
                BodyOp::Lit(b) => {
                    if b.is_empty() {
                        continue;
                    }
                    put_varint(&mut prog, ((b.len() as u64) << 1) | OP_LIT);
                    prog.extend_from_slice(b);
                }
                BodyOp::Piece { hash, len } => {
                    let idx = dict_index[hash];
                    put_varint(&mut prog, ((idx as u64) << 1) | OP_PIECE);
                    put_varint(&mut prog, *len as u64);
                }
            }
        }
    }
    prog_off.push(prog.len() as u64);

    // ---- attribute columns ----
    let ordered: Vec<&Record> = order.iter().map(|&i| &records[i]).collect();
    let built = attrs::build(&ordered)?;

    // ---- id column ----
    let (id_stream, id_restarts) = idcol::build(&ids)?;

    // ---- lay the sections down, in a fixed order (determinism) ----
    let mut w = Writer::new(path, level)?;
    w.section("ids", &id_stream)?;
    w.section("ids.restart", &u32s(&id_restarts))?;
    w.section("prog", &prog)?;
    w.section("prog.off", &u64s(&prog_off))?;
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
    w.finish(meta)?;
    Ok(meta)
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn u64s(v: &[u64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub(crate) struct Writer {
    f: File,
    path: std::path::PathBuf,
    off: u64,
    toc: Vec<(String, Section)>,
    level: i32,
}

impl Writer {
    pub(crate) fn new(path: &Path, level: i32) -> Result<Writer> {
        let f = crate::vfs::create(path).with_context(|| format!("create part {}", path.display()))?;
        Ok(Writer { f, path: path.to_path_buf(), off: 0, toc: Vec::new(), level })
    }

    pub(crate) fn section(&mut self, name: &str, raw: &[u8]) -> Result<()> {
        // A section's `stored` and `raw` are u32 in the TOC. Truncating here would write a part that
        // reads back as a shorter section with no error anywhere — silent corruption. Refuse instead:
        // a part that cannot be written is recoverable, a part that lies is not.
        if raw.len() as u64 > u32::MAX as u64 {
            bail!("section {name} is {} bytes; the format caps a section at 4 GiB", raw.len());
        }
        let (codec, payload) = crate::fold::codec::encode(raw, None, self.level)?;
        crate::vfs::write_all_at(&self.f, &self.path, &payload, self.off)?;
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

    pub(crate) fn finish(self, meta: PartMeta) -> Result<()> {
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
        let toc_off = self.off;
        crate::vfs::write_all_at(&self.f, &self.path, &toc_payload, toc_off)?;

        let mut foot = Vec::with_capacity(FOOTER_LEN as usize);
        foot.extend_from_slice(MAGIC);
        foot.extend_from_slice(&toc_off.to_le_bytes());
        foot.extend_from_slice(&(toc_payload.len() as u32).to_le_bytes());
        foot.extend_from_slice(&(toc.len() as u32).to_le_bytes());
        foot.extend_from_slice(&meta.n_records.to_le_bytes());
        foot.extend_from_slice(&meta.seq_lo.to_le_bytes());
        foot.extend_from_slice(&meta.seq_hi.to_le_bytes());
        foot.push(toc_codec);
        foot.push(PART_VERSION);
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
        // The footer lands LAST and is the completeness marker.
        crate::vfs::write_all_at(&self.f, &self.path, &foot, toc_off + toc_payload.len() as u64)?;
        crate::vfs::sync_file(&self.f, &self.path)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

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
        Part::open_in(path, SectionCache::global())
    }

    /// Open sharing `cache` with other parts.
    pub fn open_in(path: &Path, cache: Arc<SectionCache>) -> Result<Part> {
        let f = File::open(path).with_context(|| format!("open part {}", path.display()))?;
        Part::open_reader(Box::new(f), cache)
    }

    /// Open from any [`ReadAt`] — a plain file, an extent of a pack, a remote range. The format is
    /// footer-addressed precisely so that THIS is the only entry a backend needs.
    pub fn open_reader(f: Box<dyn ReadAt>, cache: Arc<SectionCache>) -> Result<Part> {
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
        let mut tbuf = vec![0u8; toc_stored as usize];
        f.read_exact_at(&mut tbuf, toc_off)?;
        if version >= 1 && crc32fast::hash(&tbuf) != toc_xsum {
            bail!("part TOC fails its checksum — every section checksum it carries is untrustworthy");
        }
        let toc_bytes = crate::fold::codec::decode(toc_codec, &tbuf, toc_raw, None)?;

        let mut at = 0usize;
        let n = get_varint(&toc_bytes, &mut at)? as usize;
        // An entry costs several bytes, so the byte count bounds the entry count — checked before
        // the count sizes an allocation, because `n` is exactly as trustworthy as the TOC carrying
        // it, and on a version-0 part the TOC has no checksum at all.
        let mut toc = HashMap::with_capacity(n.min(toc_bytes.len()));
        for _ in 0..n {
            let nl = get_varint(&toc_bytes, &mut at)? as usize;
            // `nl > len - at`, never `at + nl > len`: the sum overflows on a hostile length.
            if nl > toc_bytes.len() - at {
                bail!("part TOC entry name runs past the end of the TOC");
            }
            let name = String::from_utf8(toc_bytes[at..at + nl].to_vec())?;
            at += nl;
            let off = get_varint(&toc_bytes, &mut at)?;
            let stored = get_varint(&toc_bytes, &mut at)? as u32;
            let raw = get_varint(&toc_bytes, &mut at)? as u32;
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
            if off.saturating_add(stored as u64) > len {
                bail!("part TOC entry {name} runs past the end of the file");
            }
            toc.insert(name, Section { off, stored, raw, codec, xsum });
        }
        if at != toc_bytes.len() {
            bail!("part TOC has {} trailing bytes after its last entry", toc_bytes.len() - at);
        }
        // The footer's n_records is load-bearing everywhere — row bounds, dense-column synthesis —
        // and it is a bare integer a flipped bit can inflate to anything. `prog.off` is REQUIRED
        // and its RAW size is (n_records + 1) u64s, so the two must agree; after this check the
        // count is as trustworthy as the section sizes, which are range-checked above.
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
        Ok(Part {
            f,
            version,
            toc,
            meta: PartMeta { n_records, seq_lo, seq_hi },
            id: cache::next_part_id(),
            cache,
        })
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

    /// A section's decompressed bytes, cached after first touch.
    fn sect(&self, name: &str) -> Result<Arc<Vec<u8>>> {
        let k = Kind::Section(name.to_string());
        if let Some(Held::Bytes(v)) = self.cache.get(self.id, &k) {
            return Ok(v);
        }
        let s = self
            .toc
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("part has no section {name}"))?
            .clone();
        let mut buf = vec![0u8; s.stored as usize];
        self.f.read_exact_at(&mut buf, s.off)?;
        let raw = crate::fold::codec::decode(s.codec, &buf, s.raw, None)?;
        let arc = Arc::new(raw);
        self.cache.put(self.id, k, Held::Bytes(arc.clone()));
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

    /// The id column's restart offsets, widened to u32.
    ///
    /// NOT cached, deliberately. Caching it looked obviously right — three call sites rebuild the
    /// array — but measured against 20,000 point lookups it made no difference at all (38.5ms
    /// uncached, 39.1ms cached), so it would have been a cache entry per part bought with nothing.
    /// This exists to keep the three call sites from repeating the widening, not for speed.
    fn restarts(&self) -> Result<Vec<u32>> {
        Ok(self.nums("ids.restart", 4)?.iter().map(|&x| x as u32).collect())
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
        // Compared by division, not `(i + 1) * WIDTH`: `i` arrives from a body-program varint and a
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
        let mut checked = 0usize;
        if self.version < 1 {
            return Ok(0); // predates per-section checksums; nothing to check, and that is not an error
        }
        for (name, s) in &self.toc {
            let mut buf = vec![0u8; s.stored as usize];
            self.f.read_exact_at(&mut buf, s.off)?;
            let got = crc32fast::hash(&buf);
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

    /// The body program of row `r`, with piece references resolved to content identity.
    pub fn body(&self, r: usize) -> Result<Vec<BodyOp>> {
        let prog = self.sect("prog")?;
        let offs = self.nums("prog.off", 8)?;
        if r + 1 >= offs.len() {
            bail!("row {r} out of range");
        }
        let (mut at, end) = (offs[r] as usize, offs[r + 1] as usize);
        if end > prog.len() || at > end {
            bail!("prog.off names a program outside the prog section");
        }
        let n = get_varint(&prog, &mut at)? as usize;
        let mut out = Vec::with_capacity(n.min(end.saturating_sub(at)));
        for _ in 0..n {
            let tagged = get_varint(&prog, &mut at)?;
            if tagged == OP_ESCAPE_RESERVED {
                bail!("body program uses the reserved op escape — this part needs a newer build");
            }
            if tagged & 1 == OP_LIT {
                let len = (tagged >> 1) as usize;
                if at > end || len > end - at {
                    bail!("literal runs past the program");
                }
                out.push(BodyOp::Lit(prog[at..at + len].to_vec()));
                at += len;
            } else {
                let idx = (tagged >> 1) as usize;
                let len = get_varint(&prog, &mut at)? as u32;
                let (_, hash) = self.piece(idx)?;
                out.push(BodyOp::Piece { hash, len });
            }
        }
        Ok(out)
    }

    /// Row `r`'s attributes, in their exact original order, duplicates included.
    pub fn attrs(&self, r: usize) -> Result<Vec<(String, AttrValue)>> {
        attrs::read_row(self, r)
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
        Ok(Record { id, body: self.body(r)?, attrs: self.attrs(r)? })
    }

    /// Reconstruct row `r`'s content byte-exactly, resolving pieces through `fold`.
    ///
    /// Piece references go through the part's own dictionary, so the fold is addressed by location and
    /// never searched. The dictionary is in fold order, so a scan walks the fold forward.
    pub fn reconstruct(&self, r: usize, fold: &Fold) -> Result<Vec<u8>> {
        let prog = self.sect("prog")?;
        let offs = self.nums("prog.off", 8)?;
        if r + 1 >= offs.len() {
            bail!("row {r} out of range");
        }
        let (mut at, end) = (offs[r] as usize, offs[r + 1] as usize);
        if end > prog.len() || at > end {
            bail!("prog.off names a program outside the prog section");
        }
        let n = get_varint(&prog, &mut at)? as usize;
        let mut out = Vec::new();
        for _ in 0..n {
            let tagged = get_varint(&prog, &mut at)?;
            if tagged == OP_ESCAPE_RESERVED {
                bail!("body program uses the reserved op escape — this part needs a newer build");
            }
            if tagged & 1 == OP_LIT {
                let len = (tagged >> 1) as usize;
                if at > end || len > end - at {
                    bail!("literal runs past the program");
                }
                out.extend_from_slice(&prog[at..at + len]);
                at += len;
            } else {
                let idx = (tagged >> 1) as usize;
                let len = get_varint(&prog, &mut at)? as u32;
                let (loc, hash) = self.piece(idx)?;
                if loc.raw != len {
                    bail!("piece {hash} is {} bytes but the program says {len}", loc.raw);
                }
                fold.read_verified_into(loc, hash, &mut out)?;
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

    /// Every section: `(name, stored, raw, codec)` — the on-disk anatomy of this part.
    pub fn sections(&self) -> Vec<(String, u32, u32, u8)> {
        let mut v: Vec<(String, u32, u32, u8)> =
            self.toc.iter().map(|(n, s)| (n.clone(), s.stored, s.raw, s.codec)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
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
            4 => b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap()) as u64).collect(),
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

