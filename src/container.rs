//! A store's artifacts in one **mutable** file.
//!
//! The [pack](crate::pack) is a store in one file that can only be read: its footer is at EOF and
//! is the completeness marker, which is exactly right for a sealed artifact and exactly wrong for
//! one that grows. Appending past an EOF footer leaves a window with no footer at EOF, and a crash
//! in that window leaves a file nothing can open.
//!
//! A container inverts the addressing. Two fixed superblock slots live at the head of the file and
//! are written **alternately**, so the slot a reader would choose is never the slot a writer is
//! touching. Everything else is appended beyond the last committed tail, which means an interrupted
//! write lands in bytes no committed superblock refers to. Recovery is therefore not a repair: the
//! newest slot that passes its checksum is the state, and uncommitted bytes past its tail are
//! ignored and later overwritten.
//!
//! ```text
//! [ slot 0 (4 KiB) ][ slot 1 (4 KiB) ][ member ][ member ][ directory ][ member ] ...
//!                                     ^-- region start (8192)
//! ```
//!
//! What a container holds is what a pack holds — `MANIFEST`, the parts a manifest names, and the
//! live fold generation's segments and sidecars — under the same flat `/`-joined names. Because
//! [every offset inside a part or fold segment is relative to that artifact's start](../FORMAT.md),
//! the members are byte-identical to their directory and pack forms.
//!
//! **A member is a list of extents, not one range.** A member staged whole has exactly one, but a
//! member that grows across commits — the active fold segment — gains an extent per commit that
//! extended it, with other members' bytes between. [`Container::extent`] stitches the list into
//! one logical range through [`crate::readat::Extents`], so
//! [`Part::open_reader`](crate::part::Part::open_reader) and
//! [`Fold::open_read_from`](crate::fold::Fold::open_read_from) never learn the bytes are
//! scattered. Physically adjacent extents coalesce as they are staged, so a member extended by
//! consecutive commits with nothing between them stays one extent.
//!
//! **Space is reclaimed by rewriting, not by reuse.** Freed extents are recorded — stamped with
//! the commit that freed them — so the waste is reportable and its age is provable, but allocation
//! only ever appends. Reusing a freed extent would hand a reader holding an older superblock a
//! range whose bytes are now something else — silent corruption rather than a detected fault. The
//! same posture the engine takes with `refold`.
//!
//! **A sealed container is final.** The sealed flag in the superblock refuses every further
//! commit; what remains is a single-file artifact that reads like any other container and can
//! never again be a writer's target.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::part::idcol::{get_varint, put_varint};
use crate::readat::Extents;
use crate::readat::ReadAt;

/// Head of every superblock. A file that does not start with it is not a container.
pub const MAGIC: &[u8; 8] = b"TURNCTNR";

/// The container plane's reject-forward lever, independent of the record format version. This
/// revision writes 2; revision 1 — the pre-extent-list layout — is read for upgrade and never
/// written.
pub const CONTAINER_VERSION: u8 = 2;

/// The revision the first published containers carried: single-extent members, unstamped free
/// list, no flags. Read-only; the first commit over one publishes the current revision.
const LEGACY_VERSION: u8 = 1;

/// Superblock flags, byte 50. Every undefined bit MUST be zero and a reader must refuse otherwise.
pub const SB_FLAG_SEALED: u8 = 1;

/// Each superblock slot is a whole page: a slot write is one `pwrite` that cannot straddle two.
pub const SLOT_LEN: u64 = 4096;

/// First byte a member may occupy.
pub const REGION_START: u64 = SLOT_LEN * 2;

/// Bytes of a slot the format actually defines; the rest is zero and reserved.
const SB_LEN: usize = 56;

/// Members and fresh extents start on this boundary. Hole punching deallocates whole filesystem
/// blocks, so an unaligned extent strands its edges; the padding this costs (< 4 KiB per fresh
/// extent, structural and deliberately not free-listed) is what makes a freed extent's bytes
/// actually returnable in place.
pub const ALIGN: u64 = 4096;

/// Suffix of the working directory the RETIRED checkpoint bridge kept beside a container. No
/// writer creates one any more; it is named so `reclaim` can refuse a container that still has
/// an abandoned 0.1.x session beside it, instead of rewriting under unfolded acknowledged writes
/// only that release can settle.
pub const HOT_SUFFIX: &str = "-hot";

/// Refuse a directory that claims more than this compressed, before allocating for it.
const MAX_DIR_STORED: u32 = 64 << 20;
/// Refuse a directory that claims more than this decompressed.
const MAX_DIR_RAW: u32 = 256 << 20;
/// Refuse a container claiming more members than a store could plausibly have.
const MAX_MEMBERS: u32 = 1_000_000;
/// Refuse a member scattered across more extents than commits could plausibly have staged.
const MAX_MEMBER_EXTENTS: u64 = 1 << 16;
/// Longest member name accepted.
const MAX_NAME: u64 = 16 << 10;

fn align_up(x: u64) -> u64 {
    x.div_ceil(ALIGN) * ALIGN
}

/// One member: its extents in logical order, its logical length, and crc32 over its logical bytes.
#[derive(Clone, Debug)]
struct Member {
    extents: Vec<(u64, u64)>,
    len: u64,
    xsum: u32,
}

/// One committed state of a container.
#[derive(Clone, Copy, Debug)]
struct Superblock {
    seq: u64,
    dir_off: u64,
    dir_stored: u32,
    dir_raw: u32,
    n_entries: u32,
    dir_xsum: u32,
    tail: u64,
    dir_codec: u8,
    version: u8,
    flags: u8,
}

impl Superblock {
    fn empty() -> Superblock {
        Superblock {
            seq: 0,
            dir_off: REGION_START,
            dir_stored: 0,
            dir_raw: 0,
            n_entries: 0,
            dir_xsum: 0,
            tail: REGION_START,
            dir_codec: 0,
            version: CONTAINER_VERSION,
            flags: 0,
        }
    }

    fn encode(&self) -> [u8; SLOT_LEN as usize] {
        let mut slot = [0u8; SLOT_LEN as usize];
        slot[0..8].copy_from_slice(MAGIC);
        slot[8..16].copy_from_slice(&self.seq.to_le_bytes());
        slot[16..24].copy_from_slice(&self.dir_off.to_le_bytes());
        slot[24..28].copy_from_slice(&self.dir_stored.to_le_bytes());
        slot[28..32].copy_from_slice(&self.dir_raw.to_le_bytes());
        slot[32..36].copy_from_slice(&self.n_entries.to_le_bytes());
        slot[36..40].copy_from_slice(&self.dir_xsum.to_le_bytes());
        slot[40..48].copy_from_slice(&self.tail.to_le_bytes());
        slot[48] = self.dir_codec;
        slot[49] = CONTAINER_VERSION;
        slot[50] = self.flags;
        // slot[51] reserved, already zero
        let digest = blake3::hash(&slot[0..52]);
        slot[52..56].copy_from_slice(&digest.as_bytes()[0..4]);
        slot
    }

    /// Decode one slot, distinguishing the two failures that must not be confused.
    ///
    /// `Ok(None)` is a slot that was never written or was torn mid-write: its checksum does not
    /// cover its bytes, so it carries no claim and the other slot simply wins. `Err` is a slot
    /// whose checksum *passes* but whose version this build does not know — an authentic statement
    /// from a newer writer. Falling back to the older slot there would serve a stale state while
    /// reporting success, so the whole container is refused instead. Torn means ignore; authentic
    /// and unintelligible means stop.
    fn decode(slot: &[u8]) -> Result<Option<Superblock>> {
        if slot.len() < SB_LEN || &slot[0..8] != MAGIC {
            return Ok(None);
        }
        let digest = blake3::hash(&slot[0..52]);
        if slot[52..56] != digest.as_bytes()[0..4] {
            return Ok(None);
        }
        let version = slot[49];
        if version == 0 || version > CONTAINER_VERSION {
            bail!(
                "container superblock declares version {version}, and this build reads up to \
                 {CONTAINER_VERSION}"
            );
        }
        let flags = slot[50];
        if version == LEGACY_VERSION {
            // The legacy revision defined no flags; a nonzero byte is a claim it could not make.
            if slot[50..52] != [0, 0] {
                bail!("container superblock sets reserved bits that must be zero");
            }
        } else {
            if flags & !SB_FLAG_SEALED != 0 {
                bail!("container superblock sets flags this build does not know: {flags:#04x}");
            }
            if slot[51] != 0 {
                bail!("container superblock sets reserved bits that must be zero");
            }
        }
        Ok(Some(Superblock {
            seq: u64::from_le_bytes(slot[8..16].try_into()?),
            dir_off: u64::from_le_bytes(slot[16..24].try_into()?),
            dir_stored: u32::from_le_bytes(slot[24..28].try_into()?),
            dir_raw: u32::from_le_bytes(slot[28..32].try_into()?),
            n_entries: u32::from_le_bytes(slot[32..36].try_into()?),
            dir_xsum: u32::from_le_bytes(slot[36..40].try_into()?),
            tail: u64::from_le_bytes(slot[40..48].try_into()?),
            dir_codec: slot[48],
            version,
            flags,
        }))
    }
}

/// A store's artifacts in one mutable file.
pub struct Container {
    f: Arc<File>,
    path: PathBuf,
    dir: BTreeMap<String, Member>,
    /// `(off, len, freed_seq)` — extents nothing names any more, stamped with the commit that
    /// freed them. Recorded, reported, never reused.
    free: Vec<(u64, u64, u64)>,
    /// The committed state this handle grew from; `seq`/`tail`/directory pointer live here.
    sb: Superblock,
    /// The staging cursor — first byte past everything written, committed or staged.
    tail: u64,
    /// The slot the live state was read from; the next commit writes the other one.
    slot: u8,
    /// Staged members exist in the file but in no committed superblock until `commit`.
    staged: bool,
    sealed: bool,
    /// Whether a [`MemberWrite`] handle is outstanding — it owns the tail while it lives.
    member_open: bool,
}

/// Read-only container directory over an arbitrary positioned source.
///
/// This is the browser/object-store door: it parses the same checksummed superblocks and member
/// directory as [`Container`], but owns no filesystem handle and exposes no mutation.
pub struct ContainerReader {
    source: Arc<dyn ReadAt>,
    label: String,
    dir: BTreeMap<String, Member>,
    seq: u64,
    sealed: bool,
}

impl ContainerReader {
    pub fn open(source: Arc<dyn ReadAt>, label: impl Into<String>) -> Result<ContainerReader> {
        let label = label.into();
        let len = source.len()?;
        if len < REGION_START {
            bail!("not a container: {label} is shorter than its superblocks");
        }
        let mut a = [0u8; SLOT_LEN as usize];
        let mut b = [0u8; SLOT_LEN as usize];
        source.read_exact_at(&mut a, 0)?;
        source.read_exact_at(&mut b, SLOT_LEN)?;
        let sa = Superblock::decode(&a).with_context(|| format!("container {label} slot 0"))?;
        let sb = Superblock::decode(&b).with_context(|| format!("container {label} slot 1"))?;
        let live = match (sa, sb) {
            (Some(x), Some(y)) if y.seq > x.seq => y,
            (Some(x), _) => x,
            (None, Some(y)) => y,
            (None, None) => bail!("not a container, or both superblocks are unreadable: {label}"),
        };
        if live.tail > len {
            // Same race as [`Container::open`]: over a live file a commit can land between the
            // length query and the slot reads, and a committed tail legitimately exceeds the
            // stale length. Containers only grow, so one fresh query decides; a tail still
            // beyond it is genuine truncation. Static sources (object stores, browser range
            // caches) answer the same length twice and reach the refusal unchanged.
            let len = source.len()?;
            if live.tail > len {
                bail!(
                    "container {label} is truncated: committed tail {} exceeds length {len}",
                    live.tail
                );
            }
        }
        let (dir, _) = read_directory(&source, Path::new(&label), &live)?;
        Ok(ContainerReader {
            source,
            label,
            dir,
            seq: live.seq,
            sealed: live.flags & SB_FLAG_SEALED != 0,
        })
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn sealed(&self) -> bool {
        self.sealed
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.dir.keys().map(String::as_str)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.dir.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.dir.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dir.is_empty()
    }

    pub fn extent(&self, name: &str) -> Option<Extents<Arc<dyn ReadAt>>> {
        let member = self.dir.get(name)?;
        Some(Extents::new(self.source.clone(), &member.extents))
    }

    pub fn read_file_bounded(&self, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let member = self.dir.get(name).ok_or_else(|| {
            anyhow::anyhow!("container member not found in {}: {name}", self.label)
        })?;
        if member.len > max_bytes {
            bail!(
                "container member {name} is {} bytes, over the {max_bytes} byte ceiling",
                member.len
            );
        }
        let reader = Extents::new(self.source.clone(), &member.extents);
        let len = usize::try_from(member.len).context("container member exceeds this platform")?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len)?;
        bytes.resize(len, 0);
        reader.read_exact_at(&mut bytes, 0)?;
        Ok(bytes)
    }
}

impl Container {
    /// Create an empty container. Refuses an existing path — publication is the caller's to
    /// sequence, exactly as it is for a pack.
    pub fn create(path: &Path) -> Result<Container> {
        let f = crate::vfs::create_new(path)
            .with_context(|| format!("create container {}", path.display()))?;
        // Both slots are written so the file has a defined shape from its first byte: slot 0 holds
        // the empty state, slot 1 is zeroed and will never be chosen until the first commit.
        let sb = Superblock::empty();
        crate::vfs::write_all_at(&f, path, &sb.encode(), 0)?;
        crate::vfs::write_all_at(&f, path, &[0u8; SLOT_LEN as usize], SLOT_LEN)?;
        crate::vfs::sync_file(&f, path)?;
        if let Some(parent) = path.parent() {
            // The container's NAME is what this makes durable; a failed directory sync means the
            // store may not exist after a crash, and creation must say so rather than succeed.
            crate::vfs::sync_dir(parent).with_context(|| {
                format!("sync {} after creating {}", parent.display(), path.display())
            })?;
        }
        Ok(Container {
            f: Arc::new(f),
            path: path.to_path_buf(),
            dir: BTreeMap::new(),
            free: Vec::new(),
            sb,
            tail: REGION_START,
            slot: 0,
            staged: false,
            sealed: false,
            member_open: false,
        })
    }

    /// Finish a birth a crash interrupted: the file exists but is shorter than the superblock
    /// region, so it provably names no member byte — nothing durable can live below
    /// [`REGION_START`]. The writer rewrites both slots exactly as [`Container::create`] would
    /// and the store is born after all. Deliberately NOT part of [`Container::open`]: a reader
    /// has no create-if-absent contract, and finishing someone else's birth is a writer's move.
    pub(crate) fn recreate_interrupted(path: &Path) -> Result<Container> {
        let f = crate::vfs::open_rw(path)
            .with_context(|| format!("reopen interrupted container {}", path.display()))?;
        let sb = Superblock::empty();
        crate::vfs::write_all_at(&f, path, &sb.encode(), 0)?;
        crate::vfs::write_all_at(&f, path, &[0u8; SLOT_LEN as usize], SLOT_LEN)?;
        crate::vfs::sync_file(&f, path)?;
        if let Some(parent) = path.parent() {
            // The container's NAME is what this makes durable; a failed directory sync means the
            // store may not exist after a crash, and creation must say so rather than succeed.
            crate::vfs::sync_dir(parent).with_context(|| {
                format!("sync {} after creating {}", parent.display(), path.display())
            })?;
        }
        Ok(Container {
            f: Arc::new(f),
            path: path.to_path_buf(),
            dir: BTreeMap::new(),
            free: Vec::new(),
            sb,
            tail: REGION_START,
            slot: 0,
            staged: false,
            sealed: false,
            member_open: false,
        })
    }

    /// Open an existing container at its newest committed state.
    pub fn open(path: &Path) -> Result<Container> {
        // Plain open, never create: this is a question about an existing store, and an absent
        // path must refuse TYPED — a NotFound a caller can classify — without ever putting a
        // transient file at the queried name.
        let f = crate::vfs::open_rw(path)
            .with_context(|| format!("open container {}", path.display()))?;
        let len = f.metadata()?.len();
        if len < REGION_START {
            bail!("not a container: {} is shorter than its superblocks", path.display());
        }

        let mut a = [0u8; SLOT_LEN as usize];
        let mut b = [0u8; SLOT_LEN as usize];
        crate::sys::read_exact_at(&f, &mut a, 0)?;
        crate::sys::read_exact_at(&f, &mut b, SLOT_LEN)?;
        let sa = Superblock::decode(&a)
            .with_context(|| format!("container {} slot 0", path.display()))?;
        let sb = Superblock::decode(&b)
            .with_context(|| format!("container {} slot 1", path.display()))?;
        if sa.is_none() && sb.is_none() {
            bail!("not a container, or both superblocks are unreadable: {}", path.display());
        }
        // Highest sequence wins. A torn slot decodes to None and simply loses, which is the whole
        // point of writing them alternately.
        let (live, slot) = match (sa, sb) {
            (Some(x), Some(y)) if y.seq > x.seq => (y, 1u8),
            (Some(x), _) => (x, 0u8),
            (None, Some(y)) => (y, 1u8),
            (None, None) => unreachable!("checked above"),
        };

        if live.tail > len {
            // `len` was measured before the slots were read, and a lock-free open races the
            // writer: a commit lands its bytes past the old tail, fsyncs, then flips a slot —
            // so a freshly committed tail can legitimately exceed a stale measurement. The
            // file never shrinks (reclamation punches holes in place), which makes one fresh
            // measurement decisive: any slot we managed to read was committed by then, so its
            // tail is covered by the file's length from that moment on. A tail still beyond a
            // re-measurement is genuine truncation, and the refusal stands.
            let len = f.metadata()?.len();
            if live.tail > len {
                bail!(
                    "container {} is truncated: committed tail {} exceeds file length {len}",
                    path.display(),
                    live.tail
                );
            }
        }

        let f = Arc::new(f);
        let (dir, free) = read_directory(&f, path, &live)?;
        Ok(Container {
            f,
            path: path.to_path_buf(),
            dir,
            free,
            tail: live.tail,
            slot,
            staged: false,
            sealed: live.flags & SB_FLAG_SEALED != 0,
            member_open: false,
            sb: live,
        })
    }

    /// Take the single-writer role on this container: an exclusive advisory lock on the file
    /// itself, exactly where SQLite puts it. The kernel releases it when the descriptor closes —
    /// including on a crash — so a stale lock cannot outlive its owner. On `wasm32-wasip1` the
    /// call succeeds unconditionally and gates nothing; the single-writer invariant is the
    /// embedder's to keep, unchanged from the directory store's statement of the same caveat.
    pub fn lock_writer(&self) -> Result<()> {
        if !crate::sys::lock_exclusive(&self.f)
            .with_context(|| format!("locking {}", self.path.display()))?
        {
            // The TYPED refusal, same as the fold's lock file carried: contention is a state a
            // consumer retries, and it must classify as one — never as an internal failure.
            return Err(crate::fold::WriterLocked { path: self.path.clone() }.into());
        }
        Ok(())
    }

    /// The committed sequence this handle is reading.
    pub fn seq(&self) -> u64 {
        self.sb.seq
    }

    /// Whether the committed state carries the sealed flag. Sealed is final: every staging or
    /// commit call on this handle refuses.
    pub fn sealed(&self) -> bool {
        self.sealed
    }

    /// Member names in sorted order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.dir.keys().map(|k| k.as_str())
    }

    /// Whether a member is present in the committed directory.
    pub fn contains(&self, name: &str) -> bool {
        self.dir.contains_key(name)
    }

    /// Number of committed members.
    pub fn len(&self) -> usize {
        self.dir.len()
    }

    /// Whether the container holds no members.
    pub fn is_empty(&self) -> bool {
        self.dir.is_empty()
    }

    /// Bytes occupied by superseded extents and alignment padding — reclaimable only by rewriting
    /// the container, or in place where the platform can punch holes.
    pub fn free_bytes(&self) -> u64 {
        self.free.iter().map(|(_, len, _)| *len).sum()
    }

    /// Bytes the members themselves occupy.
    pub fn member_bytes(&self) -> u64 {
        self.dir.values().map(|m| m.len).sum()
    }

    /// A member's physical layout: its `(offset, length)` extents in logical order. Diagnostic —
    /// readers go through [`Container::extent`], which hides exactly this.
    pub fn member_extents(&self, name: &str) -> Option<Vec<(u64, u64)>> {
        self.dir.get(name).map(|m| m.extents.clone())
    }

    /// A member's logical length, staged view included.
    pub fn member_len(&self, name: &str) -> Option<u64> {
        self.dir.get(name).map(|m| m.len)
    }

    /// Deallocate a logical byte range of a member in place: each physical run it maps to is
    /// hole-punched, offsets unmoved, then the file is fsynced so the destruction is not left
    /// pending behind checksummed bytes a reader would still trust. The caller owes the same
    /// truth `Fold::punch_blocks` owes — the range must be declared dead by the manifest before
    /// any byte goes.
    pub fn punch_within_member(&self, name: &str, off: u64, len: u64) -> Result<()> {
        let m = self
            .dir
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("container member not found: {name}"))?;
        if off.checked_add(len).is_none_or(|end| end > m.len) {
            bail!("punch of {len} bytes at {off} exceeds member {name}'s {} bytes", m.len);
        }
        let mut remaining = len;
        let mut at = off;
        let mut logical = 0u64;
        for &(phys, elen) in &m.extents {
            if remaining == 0 {
                break;
            }
            let start = logical;
            logical += elen;
            if at >= logical {
                continue;
            }
            let within = at - start;
            let take = remaining.min(elen - within);
            crate::vfs::punch_hole(&self.f, &self.path, phys + within, take).with_context(
                || {
                    format!(
                        "punching {take} bytes of member {name}; this filesystem may not support                          hole punching — re-fold instead"
                    )
                },
            )?;
            at += take;
            remaining -= take;
        }
        crate::vfs::sync_file(&self.f, &self.path)?;
        Ok(())
    }

    /// A member as a positioned reader over its logical bytes. This is the seam: a part or fold
    /// segment opens from it with no translation and no idea whether its bytes are one extent or
    /// many.
    pub fn extent(&self, name: &str) -> Option<Extents<Arc<File>>> {
        let m = self.dir.get(name)?;
        Some(Extents::new(self.f.clone(), &m.extents))
    }

    /// Read a small member whole, refusing anything larger than `max_bytes` before allocating.
    pub fn read_file_bounded(&self, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let m = self
            .dir
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("container member not found: {name}"))?;
        if m.len > max_bytes {
            bail!("container member {name} is {} bytes, over the {max_bytes} byte ceiling", m.len);
        }
        let reader = Extents::new(self.f.clone(), &m.extents);
        let mut buf = Vec::new();
        buf.try_reserve_exact(m.len as usize)?;
        buf.resize(m.len as usize, 0);
        crate::readat::ReadAt::read_exact_at(&reader, &mut buf, 0)?;
        Ok(buf)
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.sealed {
            bail!("container {} is sealed; sealed is final", self.path.display());
        }
        if self.member_open {
            bail!(
                "container {} has a member write in progress; finish or abandon it first",
                self.path.display()
            );
        }
        Ok(())
    }

    /// Begin staging one member incrementally. The returned handle owns the write position and
    /// hashes the member in the same pass that writes it — crc32 for the directory entry, BLAKE3
    /// for whatever pin the caller keeps — so nothing is reread to register it.
    ///
    /// Exactly one member write may be open at a time, and every other staging call — including
    /// `commit` — refuses while it is: the handle owns the tail, and a second writer would land
    /// bytes inside the member being assembled. [`Container::finish_member`] registers the entry;
    /// [`Container::abandon_member`] releases the tail and leaves the bytes as uncommitted noise,
    /// which is exactly what they already are.
    pub fn begin_member(&mut self, name: &str) -> Result<MemberWrite> {
        self.ensure_writable()?;
        validate_name(name)?;
        let off = self.aligned_start();
        self.member_open = true;
        Ok(MemberWrite {
            f: self.f.clone(),
            path: self.path.clone(),
            name: name.to_string(),
            off,
            written: 0,
            crc: crc32fast::Hasher::new(),
            b3: blake3::Hasher::new(),
        })
    }

    /// Register the finished member and return the BLAKE3 of its bytes, computed while they were
    /// written. Visible only after [`Container::commit`], durable only after that commit's
    /// barrier — finishing a member fsyncs nothing.
    pub fn finish_member(&mut self, w: MemberWrite) -> Result<[u8; 32]> {
        if !self.member_open {
            bail!("container {} has no member write in progress", self.path.display());
        }
        self.member_open = false;
        let digest = *w.b3.finalize().as_bytes();
        self.stage_entry(&w.name, w.off, w.written, w.crc.finalize());
        Ok(digest)
    }

    /// Release an in-progress member write without registering it. Its bytes sit past the last
    /// committed tail where no directory names them — the container's ordinary uncommitted noise,
    /// overwritten by whatever stages next.
    pub fn abandon_member(&mut self, w: MemberWrite) {
        drop(w);
        self.abandon_open_member();
    }

    /// Throw away every staged change and return to the committed state — the in-memory
    /// equivalent of dropping the handle and reopening. For a failed multi-member staging run (a
    /// refold stages a whole generation), this is the unwind: the bytes written stay where they
    /// are as uncommitted noise, and the directory view snaps back to what the superblock says.
    pub fn discard_staged(&mut self) -> Result<()> {
        let (dir, free) = read_directory(&self.f, &self.path, &self.sb)?;
        self.dir = dir;
        self.free = free;
        self.tail = self.sb.tail;
        self.staged = false;
        self.member_open = false;
        Ok(())
    }

    /// [`Container::abandon_member`] for the caller whose handle was consumed by the failure —
    /// an assembly that errored owns no `MemberWrite` to hand back, only the duty to release
    /// the tail. Single-writer makes this unambiguous: an open member write is always ours.
    pub fn abandon_open_member(&mut self) {
        self.member_open = false;
    }

    /// Align the staging cursor for a fresh extent. The padding this skips is structural — a
    /// rewrite would recreate it — so it is deliberately NOT free-listed: `free_bytes` reports
    /// what a reclaim can return, and alignment padding is not that.
    fn aligned_start(&mut self) -> u64 {
        align_up(self.tail)
    }

    /// Stage a member from bytes. Visible only after [`Container::commit`].
    pub fn put_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        validate_name(name)?;
        let off = self.aligned_start();
        crate::vfs::write_all_at(&self.f, &self.path, bytes, off)?;
        self.stage_entry(name, off, bytes.len() as u64, crc32fast::hash(bytes));
        Ok(())
    }

    /// Stage a member of known length, filled a window at a time by `fill(offset, into)`.
    ///
    /// The primitive both [`Container::ingest`] and [`reclaim`] are built on: a member is as large
    /// as the largest part a store holds, so nothing here may assume one fits in memory.
    pub fn put_stream(
        &mut self,
        name: &str,
        len: u64,
        mut fill: impl FnMut(u64, &mut [u8]) -> std::io::Result<()>,
    ) -> Result<()> {
        self.ensure_writable()?;
        validate_name(name)?;
        let off = self.aligned_start();
        let mut hasher = crc32fast::Hasher::new();
        let mut buf = vec![0u8; (1 << 20).min(len.max(1)) as usize];
        let mut at = 0u64;
        while at < len {
            let take = buf.len().min((len - at) as usize);
            fill(at, &mut buf[..take])?;
            crate::vfs::write_all_at(&self.f, &self.path, &buf[..take], off + at)?;
            hasher.update(&buf[..take]);
            at += take as u64;
        }
        self.stage_entry(name, off, len, hasher.finalize());
        Ok(())
    }

    /// Stage a member by streaming a file in. Returns the byte count ingested.
    pub fn ingest(&mut self, name: &str, from: &Path) -> Result<u64> {
        self.ensure_writable()?;
        validate_name(name)?;
        let mut src = crate::vfs::open_read(from)
            .with_context(|| format!("ingest source {}", from.display()))?;
        let off = self.aligned_start();
        let mut hasher = crc32fast::Hasher::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut written = 0u64;
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crate::vfs::write_all_at(&self.f, &self.path, &buf[..n], off + written)?;
            hasher.update(&buf[..n]);
            written += n as u64;
        }
        self.stage_entry(name, off, written, hasher.finalize());
        Ok(written)
    }

    /// Extend an existing member, filled a window at a time by `fill(offset_within_delta, into)`.
    ///
    /// This is how a member grows across commits without ever being copied: the delta lands at
    /// the staging cursor and becomes the member's next extent — or, when the member's last extent
    /// already ends at the cursor, no extent at all, because physically adjacent runs coalesce.
    /// The member's checksum extends by CRC combination; nothing already written is reread.
    pub fn append_stream(
        &mut self,
        name: &str,
        len: u64,
        mut fill: impl FnMut(u64, &mut [u8]) -> std::io::Result<()>,
    ) -> Result<()> {
        self.ensure_writable()?;
        if len == 0 {
            return Ok(());
        }
        let Some(m) = self.dir.get(name) else {
            bail!("container member not found: {name}");
        };
        // Coalesce when the member's last extent physically ends at the staging cursor — the
        // common case of consecutive extensions with nothing staged between them.
        let coalesce = matches!(m.extents.last(), Some(&(off, l)) if off + l == self.tail);
        let write_off = if coalesce { self.tail } else { self.aligned_start() };

        let mut delta = crc32fast::Hasher::new();
        let mut buf = vec![0u8; (1 << 20).min(len) as usize];
        let mut at = 0u64;
        while at < len {
            let take = buf.len().min((len - at) as usize);
            fill(at, &mut buf[..take])?;
            crate::vfs::write_all_at(&self.f, &self.path, &buf[..take], write_off + at)?;
            delta.update(&buf[..take]);
            at += take as u64;
        }

        let m = self.dir.get_mut(name).expect("presence checked above");
        if coalesce {
            let last = m.extents.last_mut().expect("coalesce implies a last extent");
            last.1 += len;
        } else {
            m.extents.push((write_off, len));
        }
        let mut whole = crc32fast::Hasher::new_with_initial_len(m.xsum, m.len);
        whole.combine(&delta);
        m.xsum = whole.finalize();
        m.len += len;
        self.tail = write_off + len;
        self.staged = true;
        Ok(())
    }

    /// Stage a removal. The member's extents are recorded as free but never reused by this handle.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        self.ensure_writable()?;
        match self.dir.remove(name) {
            Some(m) => {
                let freed_seq = self.sb.seq + 1;
                self.free.extend(m.extents.iter().map(|&(off, len)| (off, len, freed_seq)));
                self.staged = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn stage_entry(&mut self, name: &str, off: u64, len: u64, xsum: u32) {
        let extents = if len == 0 { Vec::new() } else { vec![(off, len)] };
        if let Some(old) = self.dir.insert(name.to_string(), Member { extents, len, xsum }) {
            let freed_seq = self.sb.seq + 1;
            self.free.extend(old.extents.iter().map(|&(o, l)| (o, l, freed_seq)));
        }
        self.tail = off + len;
        self.staged = true;
    }

    /// Publish every staged change as one atomic state.
    ///
    /// The order is the whole crash-safety argument: members and the directory are durable before
    /// any superblock names them, and the superblock lands in the slot the current state is *not*
    /// read from. A crash before the slot write leaves the previous state entire; a torn slot write
    /// fails its checksum and loses to the previous slot on the next open.
    pub fn commit(&mut self) -> Result<u64> {
        self.ensure_writable()?;
        if !self.staged {
            return Ok(self.sb.seq);
        }
        self.commit_with_flags(0)
    }

    /// Publish and seal in one flip. With staged changes they are committed sealed; without any,
    /// the new superblock re-points at the committed directory and adds only the flag. Either way
    /// this handle — and every handle after it — refuses further writes.
    pub fn commit_sealed(&mut self) -> Result<u64> {
        self.ensure_writable()?;
        let seq = if self.staged {
            self.commit_with_flags(SB_FLAG_SEALED)?
        } else {
            // Nothing staged: the committed directory is already durable, so the flip needs no
            // new directory and no ordering fsync — only the slot write and its barrier.
            let sb = Superblock { seq: self.sb.seq + 1, flags: SB_FLAG_SEALED, ..self.sb };
            self.flip(sb)?
        };
        self.sealed = true;
        Ok(seq)
    }

    fn commit_with_flags(&mut self, flags: u8) -> Result<u64> {
        // The committed directory is superseded by the one this commit writes; its extent joins
        // the free list so dead space from past commits stays answerable.
        if self.sb.dir_stored > 0 {
            self.free.push((self.sb.dir_off, u64::from(self.sb.dir_stored), self.sb.seq + 1));
        }
        let payload = encode_directory(&self.dir, &self.free);
        let (dir_codec, stored) = crate::fold::codec::encode(&payload, None, 3)?;
        if stored.len() as u64 > u64::from(MAX_DIR_STORED) {
            bail!("container directory is {} bytes, over the ceiling", stored.len());
        }
        let dir_off = self.tail;
        crate::vfs::write_all_at(&self.f, &self.path, &stored, dir_off)?;
        let tail = dir_off + stored.len() as u64;

        // Everything the next superblock will point at must be durable before it points at it.
        crate::vfs::sync_file(&self.f, &self.path)?;

        let sb = Superblock {
            seq: self.sb.seq + 1,
            dir_off,
            dir_stored: stored.len() as u32,
            dir_raw: payload.len() as u32,
            n_entries: self.dir.len() as u32,
            dir_xsum: crc32fast::hash(&stored),
            tail,
            dir_codec,
            version: CONTAINER_VERSION,
            flags,
        };
        self.flip(sb)
    }

    /// Write `sb` into the slot the live state was not read from, make it durable, adopt it.
    fn flip(&mut self, sb: Superblock) -> Result<u64> {
        let next_slot = 1 - self.slot;
        crate::vfs::write_all_at(
            &self.f,
            &self.path,
            &sb.encode(),
            u64::from(next_slot) * SLOT_LEN,
        )?;
        crate::vfs::sync_file(&self.f, &self.path)?;
        self.sb = sb;
        self.tail = sb.tail;
        self.slot = next_slot;
        self.staged = false;
        Ok(sb.seq)
    }

    /// Deallocate the aligned interior of every free extent old enough that no supported reader
    /// can still be holding a superblock that names its bytes — `freed_seq + grace <= seq`, with
    /// the grace window measured in commits, matching the manifest retention window's meaning.
    ///
    /// This is physical-only: the free list is already committed state, nothing in the committed
    /// present references these bytes, and no declaration is needed the way block erasure needs
    /// one — a reader old enough to resolve into a punched extent reads zeros, fails the member
    /// or frame checksums above, and reports detected corruption, never silent wrong data. The
    /// file is fsynced once after the last hole so the destruction is not left pending.
    ///
    /// Linux only, exactly as the fold's block punch is; everywhere else the first extent refuses
    /// with `Unsupported` and a rewrite (`reclaim`) is the road that exists on every platform.
    pub fn punch_free_extents(&self, grace: u64) -> Result<FreePunchStats> {
        let mut stats = FreePunchStats::default();
        let mut punched_any = false;
        for &(off, len, freed_seq) in &self.free {
            stats.examined += 1;
            if freed_seq.saturating_add(grace) > self.sb.seq {
                stats.deferred_extents += 1;
                continue;
            }
            let lo = align_up(off);
            let hi = (off + len) / ALIGN * ALIGN;
            stats.edge_bytes += (lo - off).min(len) + (off + len).saturating_sub(hi.max(lo));
            if hi <= lo {
                continue; // smaller than one block: edges only, a rewrite's job
            }
            crate::vfs::punch_hole(&self.f, &self.path, lo, hi - lo).with_context(|| {
                format!(
                    "punching {} free bytes at {lo} in {}; this filesystem may not support hole                      punching — reclaim (rewrite) instead",
                    hi - lo,
                    self.path.display()
                )
            })?;
            stats.punched_extents += 1;
            stats.punched_bytes += hi - lo;
            punched_any = true;
        }
        if punched_any {
            crate::vfs::sync_file(&self.f, &self.path)?;
        }
        Ok(stats)
    }

    /// Re-read every member's logical bytes and check them against the checksum recorded for it.
    pub fn verify(&self) -> Result<usize> {
        let mut buf = vec![0u8; 1 << 20];
        for (name, m) in &self.dir {
            let reader = Extents::new(self.f.clone(), &m.extents);
            let mut hasher = crc32fast::Hasher::new();
            let mut at = 0u64;
            while at < m.len {
                let take = std::cmp::min(buf.len() as u64, m.len - at) as usize;
                crate::readat::ReadAt::read_exact_at(&reader, &mut buf[..take], at)?;
                hasher.update(&buf[..take]);
                at += take as u64;
            }
            let got = hasher.finalize();
            if got != m.xsum {
                bail!("container member {name} fails its checksum: {got:08x} != {:08x}", m.xsum);
            }
        }
        Ok(self.dir.len())
    }
}

/// One member being staged incrementally: the write position, and the member's checksums
/// accumulated in the same pass. Created by [`Container::begin_member`], consumed by
/// [`Container::finish_member`] or [`Container::abandon_member`].
///
/// The handle owns no borrow of the container — exclusivity is enforced by the container
/// refusing every other staging call while one is outstanding — so an artifact builder can hold
/// it across its whole assembly.
pub struct MemberWrite {
    f: Arc<File>,
    path: PathBuf,
    name: String,
    off: u64,
    written: u64,
    crc: crc32fast::Hasher,
    b3: blake3::Hasher,
}

impl MemberWrite {
    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.written
    }
}

impl crate::vfs::ArtifactSink for MemberWrite {
    fn write_all_at(&mut self, data: &[u8], off: u64) -> std::io::Result<()> {
        // Sequential by contract: the hashers below are only meaningful if every byte passes
        // through exactly once, in order.
        if off != self.written {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "member {} write at {off} but {} bytes are written — sinks are sequential",
                    self.name, self.written
                ),
            ));
        }
        crate::vfs::write_all_at(&self.f, &self.path, data, self.off + off)?;
        self.crc.update(data);
        self.b3.update(data);
        self.written += data.len() as u64;
        Ok(())
    }

    /// Deliberately a no-op: a member's durability belongs to the container commit that names
    /// it — the fsync before the superblock flip is the barrier, and an artifact-level fsync here
    /// would be a second, redundant one per part.
    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn describe(&self) -> String {
        format!("member {} of container {}", self.name, self.path.display())
    }
}

/// A member read through a writer's own staged view.
///
/// The committed directory cannot serve the fold's active segment: the writer reads blocks it
/// appended moments ago, and those extents exist only in the staged state until the next commit.
/// This reader resolves the member's extents at every read, under the same lock the writer
/// stages through — so it always sees exactly what has been appended, and nothing that has not.
pub struct MemberReader {
    container: std::sync::Arc<std::sync::Mutex<Container>>,
    name: String,
}

impl MemberReader {
    pub fn new(
        container: std::sync::Arc<std::sync::Mutex<Container>>,
        name: String,
    ) -> MemberReader {
        MemberReader { container, name }
    }
}

impl crate::readat::ReadAt for MemberReader {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> std::io::Result<()> {
        // Resolve under the lock, read outside it: extents are never reused or moved once
        // staged, and the file handle outlives the lock, so a resolved view stays valid.
        let reader = {
            let c = self.container.lock().expect("container lock poisoned");
            let m = c.dir.get(&self.name).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("container member not found: {}", self.name),
                )
            })?;
            Extents::new(c.f.clone(), &m.extents)
        };
        crate::readat::ReadAt::read_exact_at(&reader, buf, off)
    }

    fn len(&self) -> std::io::Result<u64> {
        let c = self.container.lock().expect("container lock poisoned");
        Ok(c.dir.get(&self.name).map(|m| m.len).unwrap_or(0))
    }
}

impl Container {
    /// Clear the sealed flag on a restore's staging copy — the one deliberate exception to
    /// "sealed is final". Finality binds the ARTIFACT: the backup stays sealed forever, and a
    /// verified copy being restored is a different file being born writable, which is the whole
    /// point of restoring. Crate-private and reachable only from the restore path, so nothing
    /// else can talk itself into unsealing in place.
    pub(crate) fn clear_seal_for_restore(&mut self) -> Result<u64> {
        if !self.sealed {
            return Ok(self.sb.seq);
        }
        self.sealed = false;
        let sb = Superblock { seq: self.sb.seq + 1, flags: 0, ..self.sb };
        self.flip(sb)
    }
}

/// What one [`Container::punch_free_extents`] returned in place, and what it left.
#[derive(Clone, Copy, Debug, Default)]
pub struct FreePunchStats {
    /// Free extents examined.
    pub examined: usize,
    /// Extents whose aligned interior was deallocated this call. Re-punching an already-punched
    /// extent is indistinguishable from punching it — the filesystem call is idempotent — so a
    /// second call reports the same extents again rather than pretending to know better.
    pub punched_extents: usize,
    /// Bytes deallocated: the sum of aligned interiors.
    pub punched_bytes: u64,
    /// Extents younger than the grace window, left untouched for readers that may still hold a
    /// superblock predating their freeing.
    pub deferred_extents: usize,
    /// Bytes stranded at unaligned extent edges — returnable only by a rewrite.
    pub edge_bytes: u64,
}

/// What one [`reclaim`] recovered.
#[derive(Clone, Copy, Debug)]
pub struct ReclaimStats {
    /// Members carried across; the logical content is unchanged.
    pub members: usize,
    /// File size before.
    pub bytes_before: u64,
    /// File size after.
    pub bytes_after: u64,
    /// Difference — superseded extents, plus whatever the directory rewrite saved.
    pub reclaimed: u64,
}

/// Rewrite a container without the extents nothing names any more.
///
/// A container only grows. Restaging a member supersedes its predecessor rather than overwriting
/// it, and freed extents are deliberately never reused — a reader that resolved an older
/// superblock still holds offsets into them, so handing those bytes to a new member would be
/// silent corruption rather than a detected fault. The cost of that guarantee is that a container
/// checkpointed daily accumulates dead space forever, and this is the only thing that returns it
/// whole. Every member comes out a single aligned extent.
///
/// The rewrite is a copy to a fresh file and an atomic rename, not an edit: at no point is the
/// container being read half-rewritten, and a crash leaves the original untouched. A reader
/// holding the old file keeps reading it — the inode outlives the name.
///
/// Refused while a writer's working directory exists beside the file, because that directory holds
/// state the container has not been told about and rewriting would publish a version of the
/// container that is about to be superseded by a checkpoint of writes it never saw. Refused for a
/// sealed container, whose bytes are final — copy it instead.
/// The names reclaim uses beside `<store>`. Each is a full path built from the store's own, so
/// a store's recovery material is always found next to it and never mistaken for another's.
pub(crate) fn reclaim_names(path: &Path) -> ReclaimNames {
    let with = |suffix: &str| {
        let mut p = path.as_os_str().to_os_string();
        p.push(suffix);
        PathBuf::from(p)
    };
    ReclaimNames {
        staging: with(".reclaiming"),
        anchor: with(".reclaimed"),
        candidate_tmp: with(".reclaim-candidate.tmp"),
        candidate: with(".reclaim-candidate"),
    }
}

pub(crate) struct ReclaimNames {
    /// The fresh container while it is being written: unpublished, no crash meaning.
    pub staging: PathBuf,
    /// The ANCHOR: the fresh, verified container under a write-through-published name. Never the
    /// source of the uncertain replace, so no crash state consumes it. Recovery starts here.
    pub anchor: PathBuf,
    /// A byte copy of the anchor while it is being written.
    pub candidate_tmp: PathBuf,
    /// The CANDIDATE: the copy under a write-through-published name, writer-locked, and then
    /// replaced over `<store>`. A crash during that replace may lose it; the anchor remains.
    pub candidate: PathBuf,
}

/// Copy `from` to `to` through the vfs seam (so the crash model sees every byte), fsynced.
pub(crate) fn copy_container_bytes_pub(from: &Path, to: &Path) -> Result<u64> {
    copy_container_bytes(from, to)
}

fn copy_container_bytes(from: &Path, to: &Path) -> Result<u64> {
    let src = File::open(from).with_context(|| format!("open {}", from.display()))?;
    let len = src.metadata()?.len();
    let _ = crate::vfs::unlink(to);
    let dst = crate::vfs::create_new(to).with_context(|| format!("create {}", to.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut at = 0u64;
    while at < len {
        let take = buf.len().min((len - at) as usize);
        crate::sys::read_exact_at(&src, &mut buf[..take], at)?;
        crate::vfs::write_all_at(&dst, to, &buf[..take], at)?;
        at += take as u64;
    }
    crate::vfs::sync_file(&dst, to)?;
    Ok(len)
}

/// Rewrite a live container as one aligned extent per member and put the result at its name.
///
/// The publication protocol is the crash-safety argument, and it is the same on every platform
/// so the deterministic simulator proves it under both durability models:
///
/// 1. the fresh container is written at `<store>.reclaiming`, committed and verified;
/// 2. it is published as the **anchor**, `<store>.reclaimed`, by a write-through no-replace
///    rename — a durable name for verified bytes that nothing below touches;
/// 3. a byte copy of the anchor is fsynced and published as the **candidate**,
///    `<store>.reclaim-candidate`, the same way; the candidate is then opened and the writer lock
///    is taken on that handle *before* it is published at the store's name — the name a second
///    writer would open;
/// 4. the candidate is renamed over `<store>` (`vfs::rename_replace_open`) — `rename(2)` on Unix;
///    on Windows the write-through `MoveFileExW`, which refuses because this function holds the
///    original open and locked, then `std`'s POSIX-semantics fallback, which is **not**
///    write-through (`sys::rename`) and which no later documented barrier promotes. The crash
///    model therefore carries old / new / neither for this step through every later crash point,
///    including after the cleanup below and after this function returns. In every one of those
///    states the anchor is intact, and an absent `<store>` beside a whole anchor is what a writer
///    open recovers (`Store::open_file`);
/// 5. the new store at its name is reopened and verified, the locked candidate handle — now the
///    handle of `<store>` itself — is held until this function returns so no second writer can
///    enter between the replace and the return, and the anchor is unlinked (laggable: a stale
///    anchor beside a present store is removed by the next writer open, never promoted).
///
/// Cost: one extra copy of the fresh, compacted container on every platform, and on Windows two
/// write-through renames more than before. No format bytes change.
pub fn reclaim(path: &Path) -> Result<ReclaimStats> {
    reclaim_with_hook(path, AnchorConstruction::for_host(), |_| {})
}

/// How reclaim obtains the second durable name its uncertain replace recovers from.
///
/// The protocol is identical either way — a durable anchor naming verified bytes exists before
/// the replace is attempted, and survives every state the replace can leave. Only the anchor's
/// construction differs, and with it the cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorConstruction {
    /// A second directory entry for the staged inode (`link(2)`), made durable by the directory
    /// sync the protocol already performs. One write of the compacted container.
    ///
    /// Sound only where a linked name can be made durable — see
    /// [`sys::LINK_GIVES_A_DURABLE_NAME`](crate::sys). On a platform whose namespace publishes
    /// only through write-through renames, a linked anchor is not durable and does not survive
    /// the replace, which is what the simulator's Windows model says and what its sweep shows.
    Link,
    /// A byte copy of the staged container, published under its own name by a durable rename.
    /// Two writes of the compacted container, and correct under every model.
    Copy,
}

impl AnchorConstruction {
    /// What this host will attempt. `Link` still falls back to `Copy` if the filesystem refuses
    /// the link — the capability is probed by using it, not declared here.
    pub fn for_host() -> Self {
        if crate::sys::LINK_GIVES_A_DURABLE_NAME {
            Self::Link
        } else {
            Self::Copy
        }
    }
}

/// [`reclaim`] with the anchor construction chosen by the caller.
///
/// A seam for the deterministic simulator, which must exercise BOTH constructions on ONE host:
/// the cheap one is only claimed under the POSIX model, and demonstrating that it fails under the
/// Windows model is what proves the platform split is necessary rather than merely tidy. Not API:
/// production calls [`reclaim`], which selects by capability and falls back by probe.
#[cfg(feature = "dst")]
pub fn reclaim_with_construction(
    path: &Path,
    anchor: AnchorConstruction,
) -> Result<ReclaimStats> {
    reclaim_with_hook(path, anchor, |_| {})
}

/// [`reclaim`] with a hook called between the replace and the return, while the new store's
/// writer lock is held: the window a competing writer must be refused in. Crate-private — a test
/// seam, not API.
pub(crate) fn reclaim_with_hook(
    path: &Path,
    anchor: AnchorConstruction,
    mut after_replace: impl FnMut(&Path),
) -> Result<ReclaimStats> {
    let mut hot = path.as_os_str().to_os_string();
    hot.push(HOT_SUFFIX);
    if Path::new(&hot).exists() {
        bail!(
            "{} has a writer's working directory beside it; settle or close that writer first",
            path.display()
        );
    }
    let source = Container::open(path)?;
    if source.sealed() {
        bail!("container {} is sealed; sealed is final", path.display());
    }
    source.lock_writer()?;
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    if std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) != 0 {
        bail!(
            "{} has an unsettled write-ahead log beside it; open the store and settle it first",
            path.display()
        );
    }
    let bytes_before = std::fs::metadata(path)?.len();
    if source.free_bytes() == 0 {
        return Ok(ReclaimStats {
            members: source.len(),
            bytes_before,
            bytes_after: bytes_before,
            reclaimed: 0,
        });
    }
    let names = reclaim_names(path);
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    // Debris from an earlier crash: the store is present, so it is authority and these are not.
    for stale in [&names.staging, &names.anchor, &names.candidate_tmp, &names.candidate] {
        let _ = crate::vfs::unlink(stale);
    }

    // 1. The fresh container.
    let mut fresh = Container::create(&names.staging)?;
    for name in source.names().map(String::from).collect::<Vec<_>>() {
        let extent = source
            .extent(&name)
            .ok_or_else(|| anyhow::anyhow!("container lost member {name} mid-reclaim"))?;
        let len = crate::readat::ReadAt::len(&extent)?;
        fresh.put_stream(&name, len, |at, into| {
            crate::readat::ReadAt::read_exact_at(&extent, into, at)
        })?;
    }
    let members = fresh.len();
    fresh.commit()?;
    fresh.verify()?;
    drop(fresh);

    // 2-3. The anchor, and the name that is replaced over the store.
    //
    // What the protocol needs is a second DURABLE NAME for the verified bytes, so that the
    // uncertain replace below always has something whole behind it. A second name is the
    // requirement; a second copy of the bytes is one way to obtain it.
    //
    // Where a hard link gives a durable name (`sys::LINK_GIVES_A_DURABLE_NAME` — POSIX, and only
    // where the filesystem actually provides links, which is why this PROBES rather than
    // declares), the anchor is one directory entry and the staging file itself is what gets
    // replaced over the store. One write of the compacted container.
    //
    // Otherwise — Windows, or a filesystem without links — the anchor is published by rename and
    // the replaced name is a byte copy of it, exactly as before. Two writes. Nothing about the
    // crash states differs between the two routes: in both, a durable anchor names verified bytes
    // before the replace is attempted, and every state the replace can leave has it intact.
    // The fallback is for a MISSING CAPABILITY, and only that. `AlreadyExists` means the anchor
    // survived the debris sweep above, which is a protocol violation rather than a filesystem
    // without links: it propagates instead of quietly buying the slow path and publishing beside
    // state nobody accounted for.
    let linked = match anchor {
        AnchorConstruction::Copy => false,
        AnchorConstruction::Link => match crate::vfs::link(&names.staging, &names.anchor) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(e).with_context(|| {
                    format!(
                        "{} exists after the reclaim debris sweep removed it",
                        names.anchor.display()
                    )
                });
            }
            Err(_) => false,
        },
    };
    let replaced_name = if linked {
        crate::vfs::sync_dir(&parent)?;
        names.staging.clone()
    } else {
        crate::vfs::rename_noreplace(&names.staging, &names.anchor)?;
        crate::vfs::sync_dir(&parent)?;
        copy_container_bytes(&names.anchor, &names.candidate_tmp)?;
        crate::vfs::sync_dir(&parent)?;
        crate::vfs::rename_noreplace(&names.candidate_tmp, &names.candidate)?;
        crate::vfs::sync_dir(&parent)?;
        names.candidate.clone()
    };
    let bytes_after = std::fs::metadata(&replaced_name)?.len();
    let new_store = crate::vfs::open_rw(&replaced_name)?;
    if !crate::sys::lock_exclusive(&new_store)
        .with_context(|| format!("locking {}", replaced_name.display()))?
    {
        return Err(crate::fold::WriterLocked { path: replaced_name.clone() }.into());
    }
    Container::open(&replaced_name)?.verify()?;

    // 4. The uncertain replace — recorded as its own operation for the crash model. The anchor
    //    is not involved.
    crate::vfs::rename_replace_open(&replaced_name, path)?;
    crate::vfs::sync_dir(&parent)?;

    // 5. Reopen at the name, hand off, clean up.
    Container::open(path)?.verify()?;
    after_replace(path);
    drop(source);
    // Cleanup, and still reported: the store is complete either way, but "the anchor is gone" is
    // this operation's result, and it is not durable if the directory sync failed.
    let _ = crate::vfs::unlink(&names.anchor);
    crate::vfs::sync_dir(&parent)
        .with_context(|| format!("sync {} after removing the reclaim anchor", parent.display()))?;
    drop(new_store);
    Ok(ReclaimStats {
        members,
        bytes_before,
        bytes_after,
        reclaimed: bytes_before.saturating_sub(bytes_after),
    })
}

/// A member name is one or more normal path components joined by `/` — the same namespace a pack
/// TOC uses, and the same shape `safe_part_file_name` already guarantees for manifest entries.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("container member name is empty");
    }
    if name.len() as u64 > MAX_NAME {
        bail!("container member name is {} bytes, over the ceiling", name.len());
    }
    if name.contains('\\') {
        bail!("container member name contains a backslash: {name}");
    }
    let p = Path::new(name);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("container member name is not a relative path of normal components: {name}");
    }
    Ok(())
}

fn encode_directory(dir: &BTreeMap<String, Member>, free: &[(u64, u64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, dir.len() as u64);
    for (name, m) in dir {
        put_varint(&mut out, name.len() as u64);
        out.extend_from_slice(name.as_bytes());
        put_varint(&mut out, m.extents.len() as u64);
        for &(off, len) in &m.extents {
            put_varint(&mut out, off);
            put_varint(&mut out, len);
        }
        out.extend_from_slice(&m.xsum.to_le_bytes());
    }
    put_varint(&mut out, free.len() as u64);
    for &(off, len, freed_seq) in free {
        put_varint(&mut out, off);
        put_varint(&mut out, len);
        put_varint(&mut out, freed_seq);
    }
    out
}

type Directory = (BTreeMap<String, Member>, Vec<(u64, u64, u64)>);

fn read_directory<R: ReadAt + ?Sized>(
    f: &Arc<R>,
    path: &Path,
    sb: &Superblock,
) -> Result<Directory> {
    if sb.n_entries == 0 && sb.dir_stored == 0 {
        return Ok((BTreeMap::new(), Vec::new()));
    }
    if sb.dir_stored > MAX_DIR_STORED || sb.dir_raw > MAX_DIR_RAW || sb.n_entries > MAX_MEMBERS {
        bail!("container {} declares a directory over the admission ceilings", path.display());
    }
    if sb.dir_off < REGION_START || sb.dir_off + u64::from(sb.dir_stored) > sb.tail {
        bail!("container {} directory lies outside its committed region", path.display());
    }
    let mut stored = Vec::new();
    stored.try_reserve_exact(sb.dir_stored as usize)?;
    stored.resize(sb.dir_stored as usize, 0);
    f.read_exact_at(&mut stored, sb.dir_off)?;
    if crc32fast::hash(&stored) != sb.dir_xsum {
        bail!("container {} directory fails its checksum", path.display());
    }
    let payload = crate::fold::codec::decode(sb.dir_codec, &stored, sb.dir_raw, None)?;

    let mut at = 0usize;
    let mut dir = BTreeMap::new();
    let n = get_varint(&payload, &mut at)?;
    if n != u64::from(sb.n_entries) {
        bail!(
            "container {} directory holds {n} entries, superblock says {}",
            path.display(),
            sb.n_entries
        );
    }
    for _ in 0..n {
        let name_len = get_varint(&payload, &mut at)? as usize;
        let end = at.checked_add(name_len).context("container directory name truncated")?;
        let name = std::str::from_utf8(
            payload.get(at..end).context("container directory name truncated")?,
        )?
        .to_string();
        at = end;
        validate_name(&name)?;

        let m = if sb.version == LEGACY_VERSION {
            // The legacy revision stores one `(off, len)` pair per member, no extent count.
            let off = get_varint(&payload, &mut at)?;
            let len = get_varint(&payload, &mut at)?;
            let extents = if len == 0 { Vec::new() } else { vec![(off, len)] };
            let xsum = read_xsum(&payload, &mut at)?;
            Member { extents, len, xsum }
        } else {
            let n_extents = get_varint(&payload, &mut at)?;
            if n_extents > MAX_MEMBER_EXTENTS {
                bail!(
                    "container {} member {name} claims {n_extents} extents, over the ceiling",
                    path.display()
                );
            }
            let mut extents = Vec::with_capacity(n_extents as usize);
            let mut len = 0u64;
            for _ in 0..n_extents {
                let off = get_varint(&payload, &mut at)?;
                let ext_len = get_varint(&payload, &mut at)?;
                if ext_len == 0 {
                    bail!("container {} member {name} carries an empty extent", path.display());
                }
                extents.push((off, ext_len));
                len = len
                    .checked_add(ext_len)
                    .with_context(|| format!("container member {name} length overflows"))?;
            }
            let xsum = read_xsum(&payload, &mut at)?;
            Member { extents, len, xsum }
        };
        // Every extent must lie inside the committed region: a directory that points past the
        // tail is corruption, and it must be refused before anything reads through it.
        for &(off, len) in &m.extents {
            let end = off
                .checked_add(len)
                .with_context(|| format!("container member {name} extent overflows"))?;
            if off < REGION_START || end > sb.tail {
                bail!(
                    "container {} member {name} lies outside its committed region",
                    path.display()
                );
            }
        }
        if dir.insert(name.clone(), m).is_some() {
            bail!("container {} names {name} twice", path.display());
        }
    }

    // The free list must round-trip or space accounting silently resets to zero on every open,
    // and a container would report itself compact however much waste it carries.
    let mut free = Vec::new();
    let n_free = get_varint(&payload, &mut at)?;
    if n_free > u64::from(MAX_MEMBERS) {
        bail!("container {} declares {n_free} free extents, over the ceiling", path.display());
    }
    for _ in 0..n_free {
        let off = get_varint(&payload, &mut at)?;
        let len = get_varint(&payload, &mut at)?;
        let freed_seq =
            if sb.version == LEGACY_VERSION { 0 } else { get_varint(&payload, &mut at)? };
        if freed_seq > sb.seq {
            bail!(
                "container {} free extent claims it was freed by commit {freed_seq}, \
                 which has not happened",
                path.display()
            );
        }
        let end = off.checked_add(len).context("container free extent overflows")?;
        if len == 0 || off < REGION_START || end > sb.tail {
            bail!("container {} free extent lies outside its committed region", path.display());
        }
        free.push((off, len, freed_seq));
    }

    // No byte may be claimed twice. Every committed range — member extents, free extents, and the
    // directory itself — must be disjoint, or a reader can be served bytes that are
    // simultaneously someone else's. A checksum-valid directory can still lie about this, so it
    // is validated as meaning, not trusted as bytes.
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for m in dir.values() {
        ranges.extend(m.extents.iter().copied());
    }
    ranges.extend(free.iter().map(|&(off, len, _)| (off, len)));
    ranges.push((sb.dir_off, u64::from(sb.dir_stored)));
    ranges.sort_unstable();
    for w in ranges.windows(2) {
        let (a_off, a_len) = w[0];
        if a_off + a_len > w[1].0 {
            bail!("container {} claims overlapping extents", path.display());
        }
    }

    Ok((dir, free))
}

fn read_xsum(payload: &[u8], at: &mut usize) -> Result<u32> {
    let bytes = payload.get(*at..*at + 4).context("container directory checksum truncated")?;
    let xsum = u32::from_le_bytes(bytes.try_into()?);
    *at += 4;
    Ok(xsum)
}

#[cfg(test)]
mod reclaim_handoff_tests {
    //! The handoff window: after the candidate has replaced `<store>` and before `reclaim`
    //! returns, the new store's handle holds the writer lock, so a writer opening the name must
    //! receive the typed refusal — not a store.
    use super::*;
    use crate::fold::FoldCfg;
    use crate::store::{Span, Store};

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "turndb-handoff-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn is_writer_locked(e: &anyhow::Error) -> bool {
        e.chain().any(|c| c.downcast_ref::<crate::fold::WriterLocked>().is_some())
    }

    #[test]
    fn a_competing_writer_in_the_handoff_window_is_refused_with_the_typed_error() {
        let root = tmp("window");
        let ct = root.join("s.turndb");
        let cfg = FoldCfg { block_target: 4 * 1024, ..Default::default() };
        for round in 0..6u8 {
            let mut s = Store::open_file(&ct, cfg).unwrap();
            s.put(&format!("r:{round}"), &[Span::Piece(&[round; 1200])], vec![]).unwrap();
            s.sync().unwrap();
            s.flush().unwrap();
            s.close().unwrap();
        }
        let mut saw: Option<String> = None;
        let stats = reclaim_with_hook(&ct, AnchorConstruction::for_host(), |p| {
            saw = Some(match Store::open_file(p, cfg) {
                Ok(_) => "a store".to_string(),
                Err(e) if is_writer_locked(&e) => "WriterLocked".to_string(),
                Err(e) => format!("other: {e:#}"),
            });
        })
        .unwrap();
        assert!(stats.reclaimed > 0);
        assert_eq!(saw.as_deref(), Some("WriterLocked"));
        // Released at return: a writer opens now.
        Store::open_file(&ct, cfg).expect("writer opens after reclaim returned").close().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }
}
