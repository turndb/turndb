//! A store's artifacts in one **mutable** file.
//!
//! A container inverts the addressing. Two fixed superblock slots live at the head of the file and
//! are written **alternately**, so the slot a reader would choose is never the slot a writer is
//! touching. Everything else is appended beyond the last committed tail, which means an interrupted
//! write lands in bytes no published superblock refers to. Open therefore selects the newest slot
//! that passes its checksum; unreferenced bytes past its tail are ignored and later overwritten.
//!
//! ```text
//! [ slot 0 (4 KiB) ][ slot 1 (4 KiB) ][ member ][ member ][ directory ][ member ] ...
//!                                     ^-- region start (8192)
//! ```
//!
//! A container holds `MANIFEST`, the parts it names, and the selected fold generation's segments and
//! sidecars under flat `/`-joined names. Because
//! [every offset inside a part or fold segment is relative to that artifact's start](../FORMAT.md),
//! each member remains independently parseable through a bounded extent.
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
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::part::idcol::{get_varint, put_varint};
use crate::readat::Extents;
use crate::readat::ReadAt;

/// Identity of the one draft container layout this build understands. Changing the draft layout
/// changes this magic, so bytes from an earlier development iteration fail closed instead of
/// entering a compatibility path.
pub const MAGIC: &[u8; 8] = b"TDBDRFT1";

/// The current draft format epoch. This is a reject-forward field, not a compatibility range:
/// exactly this value opens.
pub const CONTAINER_DRAFT_EPOCH: u8 = 1;

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

/// Refuse a directory that claims more than this compressed, before allocating for it.
const MAX_DIR_STORED: u32 = 64 << 20;
/// Refuse a directory that claims more than this decompressed.
const MAX_DIR_RAW: u32 = 256 << 20;
/// Refuse a container claiming more members than a store could plausibly have.
const MAX_MEMBERS: u32 = 1_000_000;
/// Refuse a free list larger than the decoder will traverse.
const MAX_FREE_EXTENTS: u64 = MAX_MEMBERS as u64;
/// Refuse a member scattered across more extents than commits could plausibly have staged.
const MAX_MEMBER_EXTENTS: u64 = 1 << 16;
/// Longest member name accepted.
const MAX_NAME: u64 = 4096;

fn align_up(x: u64) -> Result<u64> {
    x.checked_add(ALIGN - 1)
        .map(|value| value / ALIGN * ALIGN)
        .ok_or_else(|| anyhow::anyhow!("container offset cannot be aligned without overflow"))
}

/// One member: its extents in logical order, its logical length, and crc32 over its logical bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Member {
    extents: Vec<(u64, u64)>,
    len: u64,
    xsum: u32,
}

/// One published state of a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Superblock {
    seq: u64,
    dir_off: u64,
    dir_stored: u32,
    dir_raw: u32,
    n_entries: u32,
    dir_xsum: u32,
    tail: u64,
    dir_codec: u8,
}

fn is_empty_birth_superblock(sb: &Superblock) -> bool {
    sb.seq == 0
        && sb.dir_off == REGION_START
        && sb.dir_stored == 0
        && sb.dir_raw == 0
        && sb.n_entries == 0
        && sb.dir_xsum == 0
        && sb.tail == REGION_START
        && sb.dir_codec == 0
}

fn select_superblock(
    first: Option<Superblock>,
    second: Option<Superblock>,
    label: &str,
) -> Result<(Superblock, u8)> {
    match (first, second) {
        (Some(left), Some(right)) if left.seq == right.seq && left != right => bail!(
            "container {label} has contradictory checksum-valid superblocks at sequence {}",
            left.seq
        ),
        (Some(left), Some(right)) if right.seq > left.seq => Ok((right, 1)),
        (Some(left), Some(_)) | (Some(left), None) => Ok((left, 0)),
        (None, Some(right)) => Ok((right, 1)),
        (None, None) => bail!("not a container, or both superblocks are unreadable: {label}"),
    }
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
        slot[49] = CONTAINER_DRAFT_EPOCH;
        // bytes 50..52 are reserved, already zero
        let digest = blake3::hash(&slot[0..52]);
        slot[52..56].copy_from_slice(&digest.as_bytes()[0..4]);
        slot
    }

    /// Decode one slot, distinguishing the two failures that must not be confused.
    ///
    /// `Ok(None)` is a slot that was never written or was torn mid-write: its checksum does not
    /// cover its bytes, so it carries no claim and the other slot simply wins. `Err` is a slot
    /// whose checksum *passes* but whose identity or epoch this build does not know — an authentic
    /// statement from another writer. Falling back to the older slot there would serve a stale
    /// state while reporting success, so the whole container is refused instead. Torn means ignore;
    /// authentic and unintelligible means stop.
    fn decode(slot: &[u8]) -> Result<Option<Superblock>> {
        if slot.len() < SB_LEN {
            return Ok(None);
        }
        let digest = blake3::hash(&slot[0..52]);
        if slot[52..56] != digest.as_bytes()[0..4] {
            return Ok(None);
        }
        if &slot[0..8] != MAGIC {
            bail!("container superblock carries an unrecognized physical identity");
        }
        let version = slot[49];
        if version != CONTAINER_DRAFT_EPOCH {
            bail!(
                "container superblock declares draft epoch {version}; this build accepts exactly \
                 {CONTAINER_DRAFT_EPOCH}"
            );
        }
        if slot[50..52] != [0, 0] {
            bail!("container superblock sets reserved bits that must be zero");
        }
        if slot[SB_LEN..].iter().any(|&byte| byte != 0) {
            bail!("container superblock reserved tail is not zero");
        }
        let sb = Superblock {
            seq: u64::from_le_bytes(slot[8..16].try_into()?),
            dir_off: u64::from_le_bytes(slot[16..24].try_into()?),
            dir_stored: u32::from_le_bytes(slot[24..28].try_into()?),
            dir_raw: u32::from_le_bytes(slot[28..32].try_into()?),
            n_entries: u32::from_le_bytes(slot[32..36].try_into()?),
            dir_xsum: u32::from_le_bytes(slot[36..40].try_into()?),
            tail: u64::from_le_bytes(slot[40..48].try_into()?),
            dir_codec: slot[48],
        };
        sb.validate_semantics()?;
        Ok(Some(sb))
    }

    /// Validate every assertion that is intrinsic to an authentic slot before slot selection.
    /// A malformed lower-sequence slot is still an authentic claim and therefore refuses the
    /// container; silently choosing the other slot would turn semantic corruption into success.
    fn validate_semantics(&self) -> Result<()> {
        if self.seq == 0 {
            if !is_empty_birth_superblock(self) {
                bail!("container sequence zero is not the canonical birth state");
            }
            return Ok(());
        }
        if self.dir_stored > MAX_DIR_STORED
            || self.dir_raw > MAX_DIR_RAW
            || self.n_entries > MAX_MEMBERS
        {
            bail!("container superblock declares a directory over the admission ceilings");
        }
        if self.dir_codec > 1 {
            bail!("container superblock declares unknown directory codec {}", self.dir_codec);
        }
        if self.dir_codec == 0 && self.dir_stored != self.dir_raw {
            bail!("container stored directory declares unequal stored and raw lengths");
        }
        let dir_end = self
            .dir_off
            .checked_add(u64::from(self.dir_stored))
            .context("container directory end overflows")?;
        if self.dir_stored == 0
            || self.dir_raw == 0
            || self.dir_off < REGION_START
            || !self.dir_off.is_multiple_of(ALIGN)
            || dir_end != self.tail
        {
            bail!("container superblock directory lies outside its committed region");
        }
        Ok(())
    }
}

fn empty_birth_image() -> [u8; REGION_START as usize] {
    let mut image = [0u8; REGION_START as usize];
    image[..SLOT_LEN as usize].copy_from_slice(&Superblock::empty().encode());
    image
}

/// A store's artifacts in one mutable file.
pub struct Container {
    f: Arc<File>,
    path: PathBuf,
    dir: BTreeMap<String, Member>,
    /// `(off, len, freed_seq)` — extents nothing names any more, stamped with the commit that
    /// freed them. Recorded, reported, never reused.
    free: Vec<(u64, u64, u64)>,
    /// The published container state this handle grew from; `seq`/`tail`/directory pointer live here.
    sb: Superblock,
    /// The staging cursor — first byte past everything written, committed or staged.
    tail: u64,
    /// The slot the live state was read from; the next commit writes the other one.
    slot: u8,
    /// Staged members exist in the file but in no committed superblock until `commit`.
    staged: bool,
    /// Token of the [`MemberWrite`] handle that currently owns the tail, if any. A per-begin
    /// token rejects both cross-container handles and stale handles invalidated by an unwind.
    open_member: Option<u64>,
    next_member_token: u64,
    /// An ambiguous failed authority barrier could not be reconciled. No later mutation may use
    /// this handle because its in-memory tail may no longer be current.
    poisoned: bool,
    /// Outcome of the most recent failed slot write/barrier, when reopening the same file proved
    /// which authority is selected. Store callers use this rather than rereading MANIFEST through
    /// a second fallible I/O path while deciding whether to adopt the attempted logical state.
    failed_publication_selected: Option<bool>,
    /// Whether the selected current state has completed a successful durability barrier. A
    /// successor can be selected after a failed final fsync; destructive maintenance must first
    /// obtain a later successful barrier so a crash cannot revive bytes it is about to destroy.
    publication_acknowledged: bool,
    /// Admission profile used whenever this handle must decode a container directory again.
    read_limits: crate::read_limits::ReadLimits,
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
    committed_empty_birth: bool,
}

impl ContainerReader {
    pub fn open(source: Arc<dyn ReadAt>, label: impl Into<String>) -> Result<ContainerReader> {
        Self::open_with_limits(source, label, crate::read_limits::ReadLimits::default())
    }

    pub fn open_with_limits(
        source: Arc<dyn ReadAt>,
        label: impl Into<String>,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<ContainerReader> {
        let read_limits = read_limits.validate()?;
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
        let (live, _) = select_superblock(sa, sb, &label)?;
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
        let (dir, _) = read_directory(&source, Path::new(&label), &live, read_limits)?;
        Ok(ContainerReader {
            source,
            label,
            dir,
            seq: live.seq,
            committed_empty_birth: is_empty_birth_superblock(&live),
        })
    }

    pub fn seq(&self) -> u64 {
        self.seq
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

    pub(crate) fn committed_is_empty_birth(&self) -> bool {
        self.committed_empty_birth
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
    /// sequence, exactly as it is for any no-replace artifact installation.
    pub fn create(path: &Path) -> Result<Container> {
        crate::store::debris::validate_store_path(path)?;
        Self::create_internal(path)
    }

    pub(crate) fn create_internal(path: &Path) -> Result<Container> {
        Self::create_internal_with_limits(path, crate::read_limits::ReadLimits::default())
    }

    pub(crate) fn create_internal_with_limits(
        path: &Path,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Container> {
        Self::create_with_limits(path, read_limits)
    }

    /// Create an unpublished protocol artifact. Once this call has exclusively created the name,
    /// every initialization failure removes that owned name best-effort; an existing collision is
    /// refused before ownership exists and is never touched.
    pub(crate) fn create_staging(path: &Path) -> Result<Container> {
        let read_limits = crate::read_limits::ReadLimits::default();
        let f = crate::vfs::create_new_staging(path)
            .with_context(|| format!("create container staging {}", path.display()))?;
        let initialized = (|| -> Result<()> {
            crate::vfs::write_all_at(&f, path, &empty_birth_image(), 0)?;
            crate::vfs::sync_file(&f, path)?;
            if let Some(parent) = path.parent() {
                crate::vfs::sync_dir(parent)?;
            }
            Ok(())
        })();
        if initialized.is_err() {
            let _ = crate::vfs::unlink(path);
            initialized?;
        }
        Ok(Self::empty_handle(path, f, read_limits))
    }

    fn create_with_limits(
        path: &Path,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Container> {
        let (staging, f) = crate::vfs::create_numbered_staging(path, "creating")
            .with_context(|| format!("create container staging beside {}", path.display()))?;
        let initialized = (|| -> Result<()> {
            crate::vfs::write_all_at(&f, &staging, &empty_birth_image(), 0)?;
            crate::vfs::sync_file(&f, &staging)?;
            crate::vfs::rename_noreplace(&staging, path)
                .with_context(|| format!("install new container at {}", path.display()))?;
            Ok(())
        })();
        if initialized.is_err() {
            let _ = crate::vfs::unlink(&staging);
            initialized?;
        }
        if let Some(parent) = path.parent() {
            crate::vfs::sync_dir(parent).with_context(|| {
                format!("sync {} after creating {}", parent.display(), path.display())
            })?;
        }
        Ok(Self::empty_handle(path, f, read_limits))
    }

    fn empty_handle(
        path: &Path,
        f: File,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Container {
        Container {
            f: Arc::new(f),
            path: path.to_path_buf(),
            dir: BTreeMap::new(),
            free: Vec::new(),
            sb: Superblock::empty(),
            tail: REGION_START,
            slot: 0,
            staged: false,
            open_member: None,
            next_member_token: 0,
            poisoned: false,
            failed_publication_selected: None,
            publication_acknowledged: true,
            read_limits,
        }
    }

    /// Open an existing container at its newest published state.
    pub fn open(path: &Path) -> Result<Container> {
        crate::store::debris::validate_store_path(path)?;
        Self::open_internal(path)
    }

    pub(crate) fn open_internal(path: &Path) -> Result<Container> {
        Self::open_internal_with_limits(path, crate::read_limits::ReadLimits::default())
    }

    pub(crate) fn open_internal_with_limits(
        path: &Path,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Container> {
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
        // Highest sequence wins. A torn slot decodes to None and simply loses, which is the whole
        // point of writing them alternately.
        let (live, slot) = select_superblock(sa, sb, &path.display().to_string())?;

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
        let (dir, free) = read_directory(&f, path, &live, read_limits)?;
        Ok(Container {
            f,
            path: path.to_path_buf(),
            dir,
            free,
            tail: live.tail,
            slot,
            staged: false,
            open_member: None,
            next_member_token: 0,
            poisoned: false,
            failed_publication_selected: None,
            // Opening proves which slot is selected, not that its final durability barrier ever
            // succeeded. Any later operation that would destroy replay input or old extents must
            // issue its own successful barrier through `acknowledge_current_state`.
            publication_acknowledged: false,
            read_limits,
            sb: live,
        })
    }

    /// Take the single-writer role on this container: an exclusive advisory lock on the file
    /// itself, exactly where SQLite puts it. The kernel releases it when the descriptor closes —
    /// including on a crash — so a stale lock cannot outlive its owner. On `wasm32-wasip1` the
    /// call succeeds unconditionally and gates nothing; the single-writer invariant is the
    /// embedder's to keep.
    pub fn lock_writer(&self) -> Result<()> {
        if !crate::sys::lock_exclusive(&self.f)
            .with_context(|| format!("locking {}", self.path.display()))?
        {
            // The typed refusal: contention is a state a
            // consumer retries, and it must classify as one — never as an internal failure.
            return Err(crate::fold::WriterLocked { path: self.path.clone() }.into());
        }
        Ok(())
    }

    /// Take the writer role, then reload the authority published at `path` while that role is
    /// held. If `path` was replaced between the original open and lock, refuse: this handle locked
    /// a displaced inode and therefore does not own the current store.
    pub(crate) fn lock_writer_current(mut self) -> Result<Self> {
        self.lock_writer()?;
        let current = Self::open_internal_with_limits(&self.path, self.read_limits)?;
        if !crate::sys::same_file(&self.f, &current.f)? {
            return Err(crate::fold::WriterLocked { path: self.path.clone() }.into());
        }
        self.dir = current.dir;
        self.free = current.free;
        self.sb = current.sb;
        self.tail = current.tail;
        self.slot = current.slot;
        self.staged = false;
        self.open_member = None;
        self.poisoned = false;
        // Reloading selected authority under the writer lock proves identity and selection only;
        // it cannot retroactively prove a prior final fsync succeeded.
        self.publication_acknowledged = false;
        Ok(self)
    }

    /// The committed sequence this handle is reading.
    pub fn seq(&self) -> u64 {
        self.sb.seq
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

    /// Whether the selected committed superblock is exactly the manifestless birth state.
    /// Staged in-memory members do not affect this predicate because no reader can see them.
    pub(crate) fn committed_is_empty_birth(&self) -> bool {
        is_empty_birth_superblock(&self.sb)
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
        let len = usize::try_from(m.len)
            .context("container member length exceeds this platform's address space")?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(len)?;
        buf.resize(len, 0);
        crate::readat::ReadAt::read_exact_at(&reader, &mut buf, 0)?;
        Ok(buf)
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.poisoned {
            bail!(
                "container {} cannot be mutated after an ambiguous publication failure; reopen it",
                self.path.display()
            );
        }
        if self.open_member.is_some() {
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
        self.preflight_replacement(name, 1)?;
        let off = self.aligned_start()?;
        self.sb
            .seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("container sequence space is exhausted"))?;
        let token = self
            .next_member_token
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("container member-write token space is exhausted"))?;
        self.next_member_token = token;
        self.open_member = Some(token);
        Ok(MemberWrite {
            f: self.f.clone(),
            path: self.path.clone(),
            token,
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
        if self.open_member != Some(w.token) || !Arc::ptr_eq(&self.f, &w.f) {
            if self.open_member.is_none() {
                bail!("container {} has no member write in progress", self.path.display());
            }
            bail!(
                "member write for {} is not the current write of container {}",
                w.path.display(),
                self.path.display()
            );
        }
        self.open_member = None;
        let digest = *w.b3.finalize().as_bytes();
        self.stage_entry(&w.name, w.off, w.written, w.crc.finalize())?;
        Ok(digest)
    }

    /// Release an in-progress member write without registering it. Its bytes sit past the last
    /// committed tail where no directory names them — the container's ordinary uncommitted noise,
    /// overwritten by whatever stages next.
    pub fn abandon_member(&mut self, w: MemberWrite) -> Result<()> {
        if self.open_member != Some(w.token) || !Arc::ptr_eq(&self.f, &w.f) {
            if self.open_member.is_none() {
                bail!("container {} has no member write in progress", self.path.display());
            }
            bail!(
                "member write for {} is not the current write of container {}",
                w.path.display(),
                self.path.display()
            );
        }
        drop(w);
        self.abandon_open_member();
        Ok(())
    }

    /// Throw away every staged change and return to the published container state — the in-memory
    /// equivalent of dropping the handle and reopening. For a failed multi-member staging run (a
    /// refold stages a whole generation), this is the unwind: the bytes written stay where they
    /// are as uncommitted noise, and the directory view snaps back to what the superblock says.
    /// Any outstanding member handle is invalidated and cannot later register its bytes.
    pub fn discard_staged(&mut self) -> Result<()> {
        let (dir, free) = read_directory(&self.f, &self.path, &self.sb, self.read_limits)?;
        self.dir = dir;
        self.free = free;
        self.tail = self.sb.tail;
        self.staged = false;
        self.open_member = None;
        Ok(())
    }

    /// [`Container::abandon_member`] for an internal assembly whose failure consumed its handle.
    /// Clearing the token invalidates that handle if it somehow survives the failed assembly.
    pub(crate) fn abandon_open_member(&mut self) {
        self.open_member = None;
    }

    /// Align the staging cursor for a fresh extent. The padding this skips is structural — a
    /// rewrite would recreate it — so it is deliberately NOT free-listed: `free_bytes` reports
    /// what a reclaim can return, and alignment padding is not that.
    fn aligned_start(&self) -> Result<u64> {
        align_up(self.tail)
    }

    /// Prove every range and commit counter a fresh member will need before writing one byte.
    fn preflight_member(&self, name: &str, len: u64) -> Result<u64> {
        self.ensure_writable()?;
        validate_name(name)?;
        let off = self.aligned_start()?;
        off.checked_add(len).context("container member end overflows")?;
        self.sb
            .seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("container sequence space is exhausted"))?;
        self.preflight_replacement(name, usize::from(len != 0))?;
        Ok(off)
    }

    fn preflight_replacement(&self, name: &str, new_extents: usize) -> Result<()> {
        if !self.dir.contains_key(name) && self.dir.len() as u64 >= u64::from(MAX_MEMBERS) {
            bail!("container member count would exceed the {MAX_MEMBERS}-member ceiling");
        }
        if new_extents as u64 > MAX_MEMBER_EXTENTS {
            bail!("container member would exceed the {MAX_MEMBER_EXTENTS}-extent ceiling");
        }
        let released = self.dir.get(name).map_or(0, |member| member.extents.len()) as u64;
        if (self.free.len() as u64)
            .checked_add(released)
            .and_then(|count| count.checked_add(u64::from(self.sb.dir_stored != 0)))
            .context("container free-extent count overflows")?
            > MAX_FREE_EXTENTS
        {
            bail!("container free-extent count would exceed the {MAX_FREE_EXTENTS}-extent ceiling");
        }
        Ok(())
    }

    /// Stage a member from bytes. Visible only after [`Container::commit`].
    pub fn put_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        let len = bytes.len() as u64;
        let off = self.preflight_member(name, len)?;
        crate::vfs::write_all_at(&self.f, &self.path, bytes, off)?;
        self.stage_entry(name, off, len, crc32fast::hash(bytes))?;
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
        let off = self.preflight_member(name, len)?;
        let mut hasher = crc32fast::Hasher::new();
        let mut buf = vec![0u8; (1 << 20).min(len.max(1)) as usize];
        let mut at = 0u64;
        while at < len {
            let take = buf.len().min((len - at) as usize);
            fill(at, &mut buf[..take])?;
            let write_at = off.checked_add(at).context("container stream offset overflows")?;
            crate::vfs::write_all_at(&self.f, &self.path, &buf[..take], write_at)?;
            hasher.update(&buf[..take]);
            at = at.checked_add(take as u64).context("container stream length overflows")?;
        }
        self.stage_entry(name, off, len, hasher.finalize())?;
        Ok(())
    }

    /// Stage a member by streaming a file in. Returns the byte count ingested.
    pub fn ingest(&mut self, name: &str, from: &Path) -> Result<u64> {
        self.ensure_writable()?;
        validate_name(name)?;
        let mut src = crate::vfs::open_read(from)
            .with_context(|| format!("ingest source {}", from.display()))?;
        let len =
            src.metadata().with_context(|| format!("stat ingest source {}", from.display()))?.len();
        self.put_stream(name, len, |_, into| src.read_exact(into))?;
        Ok(len)
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
        if (self.free.len() as u64)
            .checked_add(u64::from(self.sb.dir_stored != 0))
            .context("container free-extent count overflows")?
            > MAX_FREE_EXTENTS
        {
            bail!("container free-extent count would exceed the {MAX_FREE_EXTENTS}-extent ceiling");
        }
        self.sb
            .seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("container sequence space is exhausted"))?;
        m.len.checked_add(len).context("container member length overflows")?;
        // Coalesce when the member's last extent physically ends at the staging cursor — the
        // common case of consecutive extensions with nothing staged between them.
        let coalesce = match m.extents.last() {
            Some(&(off, extent_len)) => {
                off.checked_add(extent_len).context("container member extent end overflows")?
                    == self.tail
            }
            None => false,
        };
        if !coalesce && m.extents.len() as u64 >= MAX_MEMBER_EXTENTS {
            bail!("container member would exceed the {MAX_MEMBER_EXTENTS}-extent ceiling");
        }
        let write_off = if coalesce { self.tail } else { self.aligned_start()? };
        let write_end = write_off.checked_add(len).context("container tail overflows")?;
        if coalesce {
            m.extents
                .last()
                .expect("coalesce implies a last extent")
                .1
                .checked_add(len)
                .context("container member extent length overflows")?;
        }

        let mut delta = crc32fast::Hasher::new();
        let mut buf = vec![0u8; (1 << 20).min(len) as usize];
        let mut at = 0u64;
        while at < len {
            let take = buf.len().min((len - at) as usize);
            fill(at, &mut buf[..take])?;
            let physical =
                write_off.checked_add(at).context("container stream offset overflows")?;
            crate::vfs::write_all_at(&self.f, &self.path, &buf[..take], physical)?;
            delta.update(&buf[..take]);
            at = at.checked_add(take as u64).context("container stream length overflows")?;
        }

        let m = self.dir.get_mut(name).expect("presence checked above");
        if coalesce {
            let last = m.extents.last_mut().expect("coalesce implies a last extent");
            last.1 = last
                .1
                .checked_add(len)
                .ok_or_else(|| anyhow::anyhow!("container member length overflows"))?;
        } else {
            m.extents.push((write_off, len));
        }
        let mut whole = crc32fast::Hasher::new_with_initial_len(m.xsum, m.len);
        whole.combine(&delta);
        m.xsum = whole.finalize();
        m.len = m
            .len
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("container member length overflows"))?;
        self.tail = write_end;
        self.staged = true;
        Ok(())
    }

    /// Stage a removal. The member's extents are recorded as free but never reused by this handle.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        self.ensure_writable()?;
        let freed_seq = self
            .sb
            .seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("container sequence space is exhausted"))?;
        match self.dir.get(name) {
            Some(member) => {
                if (self.free.len() as u64)
                    .checked_add(member.extents.len() as u64)
                    .and_then(|count| count.checked_add(u64::from(self.sb.dir_stored != 0)))
                    .context("container free-extent count overflows")?
                    > MAX_FREE_EXTENTS
                {
                    bail!(
                        "container free-extent count would exceed the {MAX_FREE_EXTENTS}-extent ceiling"
                    );
                }
                let m = self.dir.remove(name).expect("presence checked above");
                self.free.extend(m.extents.iter().map(|&(off, len)| (off, len, freed_seq)));
                self.staged = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn stage_entry(&mut self, name: &str, off: u64, len: u64, xsum: u32) -> Result<()> {
        let tail = off.checked_add(len).context("container member end overflows")?;
        let next_seq = self
            .sb
            .seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("container sequence space is exhausted"))?;
        let extents = if len == 0 { Vec::new() } else { vec![(off, len)] };
        if let Some(old) = self.dir.insert(name.to_string(), Member { extents, len, xsum }) {
            self.free.extend(old.extents.iter().map(|&(o, l)| (o, l, next_seq)));
        }
        self.tail = tail;
        self.staged = true;
        Ok(())
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
            self.acknowledge_current_state()?;
            return Ok(self.sb.seq);
        }
        // Outcome evidence belongs to exactly one publication attempt. A prior selected-but-
        // unacknowledged error must never make an earlier failure in this attempt look selected.
        self.failed_publication_selected = None;
        let result = self.commit_staged();
        if result.is_err() && self.failed_publication_selected.is_none() {
            // Preparation or its pre-flip durability barrier failed before a successor could be
            // selected. Snap the directory view back to selected authority, then poison the
            // handle because an owning Store/Fold may still hold staged locations. Reopen is the
            // only boundary that can rebuild every layer from the same authority.
            let _ = self.discard_staged();
            self.poisoned = true;
            self.publication_acknowledged = false;
        }
        result
    }

    fn commit_staged(&mut self) -> Result<u64> {
        let next_seq = self
            .sb
            .seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("container sequence space is exhausted"))?;
        // The committed directory is superseded by the one this commit writes; its extent joins
        // the free list so dead space from past commits stays answerable.
        if self.dir.len() as u64 > u64::from(MAX_MEMBERS) {
            bail!("container member count exceeds the {MAX_MEMBERS}-member ceiling");
        }
        if self.dir.values().any(|member| member.extents.len() as u64 > MAX_MEMBER_EXTENTS) {
            bail!("container member exceeds the {MAX_MEMBER_EXTENTS}-extent ceiling");
        }
        let extra_free = usize::from(self.sb.dir_stored > 0);
        let next_free_capacity = self
            .free
            .len()
            .checked_add(extra_free)
            .context("container free-extent count overflows")?;
        let mut next_free = Vec::new();
        next_free.try_reserve_exact(next_free_capacity)?;
        next_free.extend_from_slice(&self.free);
        if self.sb.dir_stored > 0 {
            next_free.push((self.sb.dir_off, u64::from(self.sb.dir_stored), next_seq));
        }
        if next_free.len() as u64 > MAX_FREE_EXTENTS {
            bail!("container free-extent count exceeds the {MAX_FREE_EXTENTS}-extent ceiling");
        }
        let payload = encode_directory(&self.dir, &next_free)?;
        let (dir_codec, stored) = crate::fold::codec::encode(&payload, None, 3)?;
        if stored.len() as u64 > u64::from(MAX_DIR_STORED) {
            bail!("container directory is {} bytes, over the ceiling", stored.len());
        }
        let dir_off = align_up(self.tail)?;
        crate::vfs::write_all_at(&self.f, &self.path, &stored, dir_off)?;
        let tail = dir_off
            .checked_add(stored.len() as u64)
            .ok_or_else(|| anyhow::anyhow!("container directory end overflows"))?;
        let dir_stored = u32::try_from(stored.len())
            .context("container stored directory length exceeds its u32 field")?;
        let dir_raw =
            u32::try_from(payload.len()).context("container raw directory length exceeds u32")?;
        let n_entries =
            u32::try_from(self.dir.len()).context("container member count exceeds u32")?;

        // Everything the next superblock will point at must be durable before it points at it.
        crate::vfs::sync_file(&self.f, &self.path)?;

        let sb = Superblock {
            seq: next_seq,
            dir_off,
            dir_stored,
            dir_raw,
            n_entries,
            dir_xsum: crc32fast::hash(&stored),
            tail,
            dir_codec,
        };
        let sequence = self.flip(sb)?;
        self.free = next_free;
        Ok(sequence)
    }

    /// Write `sb` into the slot the live state was not read from, make it durable, adopt it.
    fn flip(&mut self, sb: Superblock) -> Result<u64> {
        let next_slot = 1 - self.slot;
        self.failed_publication_selected = None;
        if let Err(write_error) = crate::vfs::write_all_at(
            &self.f,
            &self.path,
            &sb.encode(),
            u64::from(next_slot) * SLOT_LEN,
        ) {
            self.reconcile_after_publication_failure(sb, next_slot);
            return Err(write_error).with_context(|| {
                format!("write container publication in {}", self.path.display())
            });
        }
        if let Err(sync_error) = crate::vfs::sync_file(&self.f, &self.path) {
            self.reconcile_after_publication_failure(sb, next_slot);
            return Err(sync_error).with_context(|| {
                format!("synchronize container publication in {}", self.path.display())
            });
        }
        self.sb = sb;
        self.tail = sb.tail;
        self.slot = next_slot;
        self.staged = false;
        self.publication_acknowledged = true;
        Ok(sb.seq)
    }

    fn reconcile_after_publication_failure(&mut self, attempted: Superblock, attempted_slot: u8) {
        self.poisoned = true;
        self.failed_publication_selected = None;
        if let Ok(current) = Self::open_internal_with_limits(&self.path, self.read_limits) {
            if crate::sys::same_file(&self.f, &current.f).unwrap_or(false) {
                let selected = current.slot == attempted_slot && current.sb == attempted;
                self.failed_publication_selected = Some(selected);
                self.dir = current.dir;
                self.free = current.free;
                self.sb = current.sb;
                self.tail = current.tail;
                self.slot = current.slot;
                self.staged = false;
                self.open_member = None;
                // If the predecessor remains selected, callers above Container may still hold
                // staged fold/index state that no longer matches this reloaded directory. The
                // container is readable, but the owning writer must reopen before any mutation.
                self.poisoned = !selected;
                self.publication_acknowledged = !selected;
            }
        }
    }

    /// Establish crash durability for the container state this handle currently selects before
    /// any operation physically deallocates bytes that a predecessor could still reference.
    pub(crate) fn acknowledge_current_state(&mut self) -> Result<()> {
        self.ensure_writable()?;
        if self.publication_acknowledged {
            return Ok(());
        }
        crate::vfs::sync_file(&self.f, &self.path).with_context(|| {
            format!("synchronize selected container state in {}", self.path.display())
        })?;
        self.publication_acknowledged = true;
        Ok(())
    }

    /// Whether the exact state attempted by the most recent failed publication is nevertheless
    /// selected now. Only `Some(true)` leaves the handle mutable; predecessor selection and an
    /// unreconciled outcome both require reopening the writer.
    pub(crate) fn failed_publication_selected(&self) -> Option<bool> {
        self.failed_publication_selected
    }

    pub(crate) fn ensure_store_writer_usable(&self) -> Result<()> {
        self.ensure_writable()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn is_current_path_file(&self, path: &Path) -> Result<bool> {
        let current = Self::open_internal(path)?;
        Ok(crate::sys::same_file(&self.f, &current.f)?)
    }

    /// Deallocate the aligned interior of every free extent old enough that no supported reader
    /// can still be holding a superblock that names its bytes — `freed_seq + grace <= seq`, with
    /// the grace window measured in container-state sequence steps, matching the retention window's
    /// bounded-reader purpose.
    ///
    /// This is physical-only: the free list is already published state, nothing in the published
    /// present references these bytes, and no declaration is needed the way block erasure needs
    /// one — a reader old enough to resolve into a punched extent reads zeros, fails the member
    /// or frame checksums above, and reports detected corruption, never silent wrong data. The
    /// file is fsynced once after the last hole so the destruction is not left pending.
    ///
    /// TurnDB implements this on Linux and Windows, exactly as it does fold block punching;
    /// everywhere else the first extent refuses with `Unsupported` and a rewrite (`reclaim`) is
    /// the road that exists on every platform.
    pub(crate) fn punch_free_extents(&self, grace: u64) -> Result<FreePunchStats> {
        let mut stats = FreePunchStats::default();
        let mut punched_any = false;
        for &(off, len, freed_seq) in &self.free {
            stats.examined += 1;
            if freed_seq.saturating_add(grace) > self.sb.seq {
                stats.deferred_extents += 1;
                continue;
            }
            let lo = align_up(off)?;
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

    /// Verify every committed member. Ordinary members are checked against their directory CRC.
    /// A fold segment carrying manifest-declared punched blocks cannot retain its pre-punch outer
    /// CRC, so those segments are instead parsed and scrubbed through the manifest-authorized fold
    /// view; undeclared erasure and every surviving frame checksum are still checked.
    pub fn verify(&self) -> Result<usize> {
        self.verify_with_store_profile_and_control(
            crate::fold::FoldCfg::default(),
            crate::read_limits::ReadLimits::default(),
            &crate::control::OperationControl::default(),
        )
    }

    pub(crate) fn verify_with_store_profile_and_control(
        &self,
        cfg: crate::fold::FoldCfg,
        read_limits: crate::read_limits::ReadLimits,
        control: &crate::control::OperationControl,
    ) -> Result<usize> {
        let selected = self.selected_state_view(read_limits)?;
        selected.verify_selected_with_store_profile_and_control(cfg, read_limits, control)
    }

    fn selected_state_view(
        &self,
        read_limits: crate::read_limits::ReadLimits,
    ) -> Result<Container> {
        let (dir, free) = read_directory(&self.f, &self.path, &self.sb, read_limits)?;
        Ok(Container {
            f: self.f.clone(),
            path: self.path.clone(),
            dir,
            free,
            sb: self.sb,
            tail: self.sb.tail,
            slot: self.slot,
            staged: false,
            open_member: None,
            next_member_token: 0,
            poisoned: false,
            failed_publication_selected: None,
            publication_acknowledged: self.publication_acknowledged,
            read_limits,
        })
    }

    fn verify_selected_with_store_profile_and_control(
        &self,
        cfg: crate::fold::FoldCfg,
        read_limits: crate::read_limits::ReadLimits,
        control: &crate::control::OperationControl,
    ) -> Result<usize> {
        let ignored = crate::store::verified_punched_fold_members(self, cfg, read_limits, control)?;
        let mut buf = vec![0u8; 1 << 20];
        for (name, m) in &self.dir {
            control.check("container member verification")?;
            if ignored.contains(name) {
                continue;
            }
            let reader = Extents::new(self.f.clone(), &m.extents);
            let mut hasher = crc32fast::Hasher::new();
            let mut at = 0u64;
            while at < m.len {
                control.check("container member verification")?;
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
    token: u64,
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
        let amount = data.len() as u64;
        let written = self.written.checked_add(amount).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "member length overflows")
        })?;
        let physical = self.off.checked_add(off).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "member offset overflows")
        })?;
        physical.checked_add(amount).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "member end overflows")
        })?;
        crate::vfs::write_all_at(&self.f, &self.path, data, physical)?;
        self.crc.update(data);
        self.b3.update(data);
        self.written = written;
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

/// What one free-extent punch returned in place, and what it left.
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

/// The names reclaim uses beside `<store>`. Each is a full path built from the store's own, so
/// a store's reclaim protocol artifacts are always found next to it and never mistaken for another's.
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
    /// source of the uncertain replace, so no crash state consumes it. Interrupted-reclaim
    /// completion starts here.
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

/// How [`reclaim`] replaces the container at the store's name. Chosen by what the platform
/// guarantees for a replace over an open destination (`sys::replace_open_durability`), never by
/// platform name — so a constraint one platform has is that platform's protocol, not everyone's.
///
/// Non-exhaustive: a platform with a third guarantee would add a third protocol.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimProtocol {
    /// One `rename(2)` over the live name, durable at the directory's fsync. The fresh container
    /// is opened and writer-locked under its staging name first — the lock is on the inode, so it
    /// is the lock on `<store>` the instant the rename lands, and no second writer can enter
    /// between the replace and the return. No copy, no anchor. Sound only where the replace is
    /// atomic and a crash leaves old or new: `ReplaceOpenDurability::Atomic`.
    Rename,
    /// A durable anchor, a locked candidate copy, the uncertain replace, and interrupted-reclaim
    /// completion from the
    /// anchor at the next writer open. One extra copy of the compacted container. Required where
    /// the replace may leave neither name: `ReplaceOpenDurability::Lagged`. Sound under either
    /// guarantee, and the simulator proves it under both models.
    Anchor,
}

impl ReclaimProtocol {
    /// The protocol this platform's guarantee selects.
    pub const fn for_this_platform() -> Self {
        match crate::sys::replace_open_durability() {
            crate::sys::ReplaceOpenDurability::Atomic => ReclaimProtocol::Rename,
            crate::sys::ReplaceOpenDurability::Lagged => ReclaimProtocol::Anchor,
        }
    }
}

/// Rewrite a container without the extents nothing names any more.
///
/// A container only grows. Restaging a member supersedes its predecessor rather than overwriting
/// it, and freed extents are deliberately never reused — a reader that resolved an older
/// superblock still holds offsets into them, so handing those bytes to a new member would be
/// silent corruption rather than a detected fault. The cost of that guarantee is that a container
/// published repeatedly accumulates dead space forever, and this is the only thing that returns it
/// whole. Every member comes out a single aligned extent.
///
/// Refused while a nonempty WAL sidecar exists because it carries a pending change set that the
/// current manifest revision does not yet publish.
///
/// # Store-path replacement
///
/// The fresh container is written at `<store>.reclaiming`, committed and verified — the same on
/// every platform. How it then reaches the store's name is [`ReclaimProtocol::for_this_platform`],
/// decided by the platform's guarantee for a replace over an open destination, and the two
/// protocols are the crash-safety argument under the two guarantees:
///
/// **Rename** — `ReplaceOpenDurability::Atomic`, every platform but Windows:
///
/// 1. the fresh container is opened under its staging name and the writer lock is taken on that
///    handle; the lock follows the inode through the rename, so `<store>` is locked the instant
///    it names the fresh bytes, and no second writer can enter before this function returns;
/// 2. `rename(2)` puts the fresh container at `<store>` — one atomic step to every observer; a
///    reader holding the old file keeps reading it, because the inode outlives the name — and the
///    directory is synced, which is what makes the new name durable; a failed sync is this
///    function's error, and a crash may then show either whole container;
/// 3. the store at its name is reopened and verified.
///
/// A crash anywhere leaves the old container or the new one, each whole. The staging name, if it
/// survives, is removed by the next writer open. No copy, no anchor, no interrupted-reclaim step.
///
/// **Anchor** — `ReplaceOpenDurability::Lagged`, Windows:
///
/// 1. the fresh container is installed as the **anchor**, `<store>.reclaimed`, by a write-through
///    no-replace rename — a durable name for verified bytes that nothing below touches;
/// 2. a byte copy of the anchor is fsynced and published as the **candidate**,
///    `<store>.reclaim-candidate`, the same way; the candidate is then opened and the writer lock
///    is taken on that handle *before* it is published at the store's name — the name a second
///    writer would open;
/// 3. the candidate is renamed over `<store>` (`vfs::rename_replace_open`): the write-through
///    `MoveFileExW`, which refuses because this function holds the original open and locked, then
///    `std`'s POSIX-semantics fallback, which is **not** write-through (`sys::rename`) and which no
///    later documented barrier promotes. The crash model therefore carries old / new / neither for
///    this step through every later crash point, including after the cleanup below and after this
///    function returns. In every one of those states the anchor is intact, and an absent `<store>`
///    beside a whole anchor is what a writer open recovers (`Store::open_file`) — on every
///    platform, because an anchor travels with the store it belongs to;
/// 4. the new store at its name is reopened and verified, the locked candidate handle — now the
///    handle of `<store>` itself — is held until this function returns so no second writer can
///    enter between the replace and the return, and the anchor is unlinked (laggable: a stale
///    anchor beside a present store is removed by the next writer open, never promoted).
///
/// The deterministic simulator proves each protocol under the durability model it is specified
/// for, on every host, and shows the rename protocol reaching a state with no store and no anchor
/// under the Windows model — the reason the choice is made by guarantee (tests/dst.rs).
///
/// Cost: the rename protocol writes the compacted container once. The anchor protocol writes it
/// twice and, on Windows, adds two write-through renames. No format bytes change.
pub fn reclaim(path: &Path) -> Result<ReclaimStats> {
    crate::store::debris::validate_store_path(path)?;
    reclaim_with(path, ReclaimProtocol::for_this_platform(), |_| {})
}

/// [`reclaim`] under an explicit protocol: the crash harness's seam, so every host proves both
/// protocols under the model each is specified for. Not for production callers — the platform
/// chooses, and forcing the rename protocol where the guarantee is `Lagged` is exactly the loss
/// the harness demonstrates.
#[cfg(feature = "dst")]
pub fn reclaim_with_protocol(path: &Path, protocol: ReclaimProtocol) -> Result<ReclaimStats> {
    reclaim_with(path, protocol, |_| {})
}

/// [`reclaim`] under an explicit protocol, with a hook called between the replace and the
/// return while the new store's writer lock is held — the window a competing writer must be
/// refused in. The hook is a unit-test seam; the protocol parameter is what both public entry
/// points narrow.
fn reclaim_with(
    path: &Path,
    protocol: ReclaimProtocol,
    mut after_replace: impl FnMut(&Path),
) -> Result<ReclaimStats> {
    let source = Container::open(path)?.lock_writer_current()?;
    crate::store::verify_container_artifact(
        path,
        crate::fold::FoldCfg::default(),
        crate::read_limits::ReadLimits::default(),
        &crate::control::OperationControl::default(),
    )
    .context("refusing to reclaim a container that is not a valid current-format store")?;
    if !source.is_current_path_file(path)? {
        bail!("{} changed identity while reclaim validated it", path.display());
    }
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    let wal_bytes = match std::fs::metadata(&wal) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect WAL replay input at {:?}", wal))
        }
    };
    if wal_bytes != 0 {
        bail!(
            "{} has WAL replay input beside it; open and close the store successfully first",
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
    // Debris from an earlier crash — of either protocol, or of a store carried here from the
    // other platform: the store is present, so it is authority and these are not.
    for stale in [&names.staging, &names.anchor, &names.candidate_tmp, &names.candidate] {
        crate::vfs::unlink_if_exists(stale)
            .with_context(|| format!("remove stale reclaim artifact {}", stale.display()))?;
    }

    // The fresh container: the same on every platform.
    let mut fresh = Container::create_staging(&names.staging)?;
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

    let bytes_after = match protocol {
        ReclaimProtocol::Rename => {
            // 1. The writer lock, taken on the fresh container under its staging name. The lock
            //    is on the inode: the rename below carries it to `<store>`.
            let new_store = crate::vfs::open_rw(&names.staging)?;
            if !crate::sys::lock_exclusive(&new_store)
                .with_context(|| format!("locking {}", names.staging.display()))?
            {
                return Err(crate::fold::WriterLocked { path: names.staging.clone() }.into());
            }
            let bytes_after = std::fs::metadata(&names.staging)?.len();
            // 2. The replace — atomic to every observer under this protocol's guarantee — and
            //    the directory sync that makes the new name durable. A failed sync is this
            //    operation's error: a crash may then show either predecessor or successor, whole.
            crate::vfs::rename_replace_open(&names.staging, path)?;
            crate::vfs::sync_dir(&parent).with_context(|| {
                format!("sync {} after replacing {}", parent.display(), path.display())
            })?;
            // 3. Reopen at the name, hand off, release.
            Container::open_internal(path)?.verify()?;
            after_replace(path);
            drop(source);
            drop(new_store);
            bytes_after
        }
        ReclaimProtocol::Anchor => {
            // 1. The anchor: a durable name for verified bytes.
            crate::vfs::rename_noreplace(&names.staging, &names.anchor)?;
            crate::vfs::sync_dir(&parent)?;

            // 2. The candidate: a copy, fsynced, published under its own name, then opened and
            //    writer-locked before it is published at the store's name.
            let bytes_after = copy_container_bytes(&names.anchor, &names.candidate_tmp)?;
            crate::vfs::sync_dir(&parent)?;
            crate::vfs::rename_noreplace(&names.candidate_tmp, &names.candidate)?;
            crate::vfs::sync_dir(&parent)?;
            let new_store = crate::vfs::open_rw(&names.candidate)?;
            if !crate::sys::lock_exclusive(&new_store)
                .with_context(|| format!("locking {}", names.candidate.display()))?
            {
                return Err(crate::fold::WriterLocked { path: names.candidate.clone() }.into());
            }
            Container::open_internal(&names.candidate)?.verify()?;

            // 3. The uncertain replace — recorded as its own operation for the crash model. The
            //    anchor is not involved.
            crate::vfs::rename_replace_open(&names.candidate, path)?;
            crate::vfs::sync_dir(&parent)?;

            // 4. Reopen at the name, hand off, clean up.
            Container::open_internal(path)?.verify()?;
            after_replace(path);
            drop(source);
            // Cleanup, and still reported: the store is complete either way, but "the anchor is
            // gone" is this operation's result, and it is not durable if the directory sync
            // failed.
            crate::vfs::unlink_if_exists(&names.anchor)
                .with_context(|| format!("remove reclaim anchor {}", names.anchor.display()))?;
            crate::vfs::sync_dir(&parent).with_context(|| {
                format!("sync {} after removing the reclaim anchor", parent.display())
            })?;
            drop(new_store);
            bytes_after
        }
    };
    Ok(ReclaimStats {
        members,
        bytes_before,
        bytes_after,
        reclaimed: bytes_before.saturating_sub(bytes_after),
    })
}

/// A member name is one or more normal path components joined by `/`, matching the shape
/// `safe_part_file_name` guarantees for manifest entries.
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
    if name
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        bail!("container member name is not a canonical `/`-joined path: {name}");
    }
    Ok(())
}

fn varint_len(value: u64) -> u64 {
    ((64 - value.leading_zeros()).max(1) as u64).div_ceil(7)
}

fn encoded_directory_len(dir: &BTreeMap<String, Member>, free: &[(u64, u64, u64)]) -> Result<u64> {
    let mut len = varint_len(dir.len() as u64);
    for (name, member) in dir {
        len = len
            .checked_add(varint_len(name.len() as u64))
            .and_then(|value| value.checked_add(name.len() as u64))
            .and_then(|value| value.checked_add(varint_len(member.extents.len() as u64)))
            .context("container directory encoded length overflows")?;
        for &(off, extent_len) in &member.extents {
            len = len
                .checked_add(varint_len(off))
                .and_then(|value| value.checked_add(varint_len(extent_len)))
                .context("container directory encoded length overflows")?;
        }
        len = len.checked_add(4).context("container directory encoded length overflows")?;
    }
    len = len
        .checked_add(varint_len(free.len() as u64))
        .context("container directory encoded length overflows")?;
    for &(off, extent_len, freed_seq) in free {
        len = len
            .checked_add(varint_len(off))
            .and_then(|value| value.checked_add(varint_len(extent_len)))
            .and_then(|value| value.checked_add(varint_len(freed_seq)))
            .context("container directory encoded length overflows")?;
    }
    Ok(len)
}

fn encode_directory(dir: &BTreeMap<String, Member>, free: &[(u64, u64, u64)]) -> Result<Vec<u8>> {
    let encoded_len = encoded_directory_len(dir, free)?;
    if encoded_len > u64::from(MAX_DIR_RAW) {
        bail!("container raw directory is {encoded_len} bytes, over the ceiling");
    }
    let capacity = usize::try_from(encoded_len)
        .context("container directory length exceeds this platform's address space")?;
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)?;
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
    debug_assert_eq!(out.len(), capacity);
    Ok(out)
}

type Directory = (BTreeMap<String, Member>, Vec<(u64, u64, u64)>);

fn read_directory<R: ReadAt + ?Sized>(
    f: &Arc<R>,
    path: &Path,
    sb: &Superblock,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<Directory> {
    if sb.seq == 0 {
        if is_empty_birth_superblock(sb) {
            return Ok((BTreeMap::new(), Vec::new()));
        }
        bail!("container {} sequence zero is not the canonical birth state", path.display());
    }
    if sb.n_entries == 0 && sb.dir_stored == 0 {
        bail!("container {} carries a non-canonical empty directory declaration", path.display());
    }
    if sb.dir_stored > MAX_DIR_STORED || sb.dir_raw > MAX_DIR_RAW || sb.n_entries > MAX_MEMBERS {
        bail!("container {} declares a directory over the admission ceilings", path.display());
    }
    read_limits.admit(
        format!("container {} member directory", path.display()),
        u64::from(sb.dir_stored),
        u64::from(sb.dir_raw),
    )?;
    read_limits.admit_directory_entries(
        format!("container {} members", path.display()),
        u64::from(sb.n_entries),
    )?;
    let dir_end = sb
        .dir_off
        .checked_add(u64::from(sb.dir_stored))
        .context("container directory end overflows")?;
    if sb.dir_stored == 0
        || sb.dir_raw == 0
        || sb.dir_off < REGION_START
        || !sb.dir_off.is_multiple_of(ALIGN)
        || dir_end != sb.tail
    {
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
    let mut previous_name: Option<String> = None;
    for _ in 0..n {
        let name_len = get_varint(&payload, &mut at)?;
        if name_len > MAX_NAME {
            bail!("container {} member name is {name_len} bytes, over the limit", path.display());
        }
        let name_len = usize::try_from(name_len)
            .context("container directory name length exceeds this platform")?;
        let end = at.checked_add(name_len).context("container directory name truncated")?;
        let name = std::str::from_utf8(
            payload.get(at..end).context("container directory name truncated")?,
        )?
        .to_string();
        at = end;
        validate_name(&name)?;
        if previous_name.as_deref().is_some_and(|previous| previous >= name.as_str()) {
            bail!("container {} member names are not in strict wire order", path.display());
        }
        previous_name = Some(name.clone());

        let n_extents = get_varint(&payload, &mut at)?;
        if n_extents > MAX_MEMBER_EXTENTS {
            bail!(
                "container {} member {name} claims {n_extents} extents, over the ceiling",
                path.display()
            );
        }
        let n_extents = usize::try_from(n_extents)
            .context("container extent count exceeds this platform's address space")?;
        let mut extents = Vec::with_capacity(n_extents.min(payload.len()));
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
        let m = Member { extents, len, xsum };
        // Every extent must lie inside the committed region: a directory that points past the
        // tail is corruption, and it must be refused before anything reads through it.
        for &(off, len) in &m.extents {
            let end = off
                .checked_add(len)
                .with_context(|| format!("container member {name} extent overflows"))?;
            if off < REGION_START || !off.is_multiple_of(ALIGN) || end > sb.dir_off {
                bail!(
                    "container {} member {name} is unaligned or outside its committed member region",
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
    if n_free > MAX_FREE_EXTENTS {
        bail!("container {} declares {n_free} free extents, over the ceiling", path.display());
    }
    read_limits
        .admit_directory_entries(format!("container {} free extents", path.display()), n_free)?;
    for _ in 0..n_free {
        let off = get_varint(&payload, &mut at)?;
        let len = get_varint(&payload, &mut at)?;
        let freed_seq = get_varint(&payload, &mut at)?;
        if freed_seq > sb.seq {
            bail!(
                "container {} free extent claims it was freed by commit {freed_seq}, \
                 which has not happened",
                path.display()
            );
        }
        let end = off.checked_add(len).context("container free extent overflows")?;
        if len == 0 || off < REGION_START || !off.is_multiple_of(ALIGN) || end > sb.dir_off {
            bail!(
                "container {} free extent is unaligned or outside its committed member region",
                path.display()
            );
        }
        free.push((off, len, freed_seq));
    }
    if at != payload.len() {
        bail!("container {} directory has {} trailing bytes", path.display(), payload.len() - at);
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
        let a_end = a_off.checked_add(a_len).context("container extent end overflows")?;
        if a_end > w[1].0 {
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
    fn sequence_exhaustion_refuses_removal_before_changing_staged_authority() {
        let root = tmp("remove-sequence-exhaustion");
        let path = root.join("store.turndb");
        let mut container = Container::create(&path).unwrap();
        container.put_bytes("member", b"still present").unwrap();
        container.commit().unwrap();
        container.sb.seq = u64::MAX;

        let directory = container.dir.clone();
        let free = container.free.clone();
        let staged = container.staged;
        let error = container.remove("member").unwrap_err().to_string();
        assert!(error.contains("sequence space"), "unexpected refusal: {error}");
        assert_eq!(container.dir, directory, "refusal must not remove the member");
        assert_eq!(container.free, free, "refusal must not free its extents");
        assert_eq!(container.staged, staged, "refusal must not create staged state");
        assert_eq!(container.read_file_bounded("member", 64).unwrap(), b"still present");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn member_handles_are_bound_to_their_exact_begin_operation() {
        use crate::vfs::ArtifactSink;

        let root = tmp("member-write-token");
        let path = root.join("store.turndb");
        let other_path = root.join("other.turndb");
        let mut container = Container::create(&path).unwrap();
        let mut other = Container::create(&other_path).unwrap();

        let mut stale = container.begin_member("stale").unwrap();
        stale.write_all_at(b"stale bytes", 0).unwrap();
        container.discard_staged().unwrap();
        let mut current = container.begin_member("current").unwrap();
        current.write_all_at(b"current bytes", 0).unwrap();
        let error = container.finish_member(stale).unwrap_err().to_string();
        assert!(error.contains("not the current write"), "unexpected refusal: {error}");
        container.finish_member(current).unwrap();

        let mut foreign = other.begin_member("foreign").unwrap();
        foreign.write_all_at(b"foreign bytes", 0).unwrap();
        let current = container.begin_member("local").unwrap();
        let error = container.finish_member(foreign).unwrap_err().to_string();
        assert!(error.contains("not the current write"), "unexpected refusal: {error}");
        container.abandon_member(current).unwrap();

        container.commit().unwrap();
        assert_eq!(container.read_file_bounded("current", 32).unwrap(), b"current bytes");
        assert!(!container.contains("stale"));
        assert!(!container.contains("foreign"));
        other.abandon_open_member();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn verification_observes_the_selected_state_not_writer_staging() {
        let root = tmp("verify-selected-only");
        let path = root.join("store.turndb");
        let mut container = Container::create(&path).unwrap();
        container.put_bytes("selected", b"published bytes").unwrap();
        container.commit().unwrap();
        container.put_bytes("staged", b"not selected yet").unwrap();

        assert_eq!(container.verify().unwrap(), 1);
        assert!(container.contains("staged"), "verification must not discard writer staging");
        container.commit().unwrap();
        assert_eq!(container.verify().unwrap(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn staging_creation_never_removes_a_name_it_did_not_create() {
        let root = tmp("staging-collision-ownership");
        let path = root.join("backup.turndb.backing-up-7-0");
        std::fs::write(&path, b"pre-existing protocol evidence").unwrap();

        let error =
            Container::create_staging(&path).err().expect("an occupied staging name must refuse");
        assert!(error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
        }));
        assert_eq!(std::fs::read(&path).unwrap(), b"pre-existing protocol evidence");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn taking_the_writer_role_reloads_authority_published_after_the_initial_open() {
        let root = tmp("writer-reloads-current-authority");
        let path = root.join("store.turndb");
        let mut initial = Container::create(&path).unwrap();
        initial.put_bytes("first", b"one").unwrap();
        initial.commit().unwrap();
        drop(initial);

        let stale = Container::open(&path).unwrap();
        let stale_seq = stale.seq();
        let mut publisher = Container::open(&path).unwrap().lock_writer_current().unwrap();
        publisher.put_bytes("second", b"two").unwrap();
        let published_seq = publisher.commit().unwrap();
        drop(publisher);

        let current = stale.lock_writer_current().unwrap();
        assert!(published_seq > stale_seq);
        assert_eq!(current.seq(), published_seq, "the locked handle must adopt current authority");
        assert_eq!(current.read_file_bounded("first", 16).unwrap(), b"one");
        assert_eq!(current.read_file_bounded("second", 16).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Unix permits the exact reclaim race: a writer can hold the old inode open while a new file
    /// replaces its pathname. The initial handle must not mistake a lock on that displaced inode
    /// for ownership of the current store.
    #[cfg(unix)]
    #[test]
    fn taking_the_writer_role_refuses_a_path_replaced_after_the_initial_open() {
        let root = tmp("writer-refuses-replaced-path");
        let path = root.join("store.turndb");
        let replacement_path = root.join("replacement.turndb");
        let mut initial = Container::create(&path).unwrap();
        initial.put_bytes("identity", b"old").unwrap();
        initial.commit().unwrap();
        drop(initial);

        let stale = Container::open(&path).unwrap();
        let mut replacement = Container::create(&replacement_path).unwrap();
        replacement.put_bytes("identity", b"current").unwrap();
        replacement.commit().unwrap();
        drop(replacement);
        crate::vfs::rename_replace_open(&replacement_path, &path).unwrap();

        let error = match stale.lock_writer_current() {
            Ok(_) => panic!("a lock on the displaced inode must not grant the writer role"),
            Err(error) => error,
        };
        assert!(is_writer_locked(&error), "wrong refusal: {error:#}");
        let current = Container::open(&path).unwrap();
        assert_eq!(current.read_file_bounded("identity", 16).unwrap(), b"current");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Both protocols, on this host: the rename protocol's lock rides the staging inode through
    /// the rename; the anchor protocol's rides the candidate's. Either way the hook, called with
    /// the fresh container at the store's name, must find it locked.
    #[test]
    fn a_competing_writer_in_the_handoff_window_is_refused_with_the_typed_error() {
        for protocol in [ReclaimProtocol::Rename, ReclaimProtocol::Anchor] {
            let root = tmp(&format!("window-{protocol:?}"));
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
            let stats = reclaim_with(&ct, protocol, |p| {
                saw = Some(match Store::open_file(p, cfg) {
                    Ok(_) => "a store".to_string(),
                    Err(e) if is_writer_locked(&e) => "WriterLocked".to_string(),
                    Err(e) => format!("other: {e:#}"),
                });
            })
            .unwrap();
            assert!(stats.reclaimed > 0, "{protocol:?}");
            assert_eq!(saw.as_deref(), Some("WriterLocked"), "{protocol:?}");
            assert_eq!(
                std::fs::metadata(&ct).unwrap().len(),
                stats.bytes_after,
                "{protocol:?}: bytes_after is the file at the store's name"
            );
            // Released at return: a writer opens now.
            Store::open_file(&ct, cfg)
                .expect("writer opens after reclaim returned")
                .close()
                .unwrap();
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// The platform fact, stated as the platform fact: the guarantee selects the protocol.
    #[test]
    fn the_platform_selects_rename_everywhere_but_windows() {
        let expected = if cfg!(target_os = "windows") {
            ReclaimProtocol::Anchor
        } else {
            ReclaimProtocol::Rename
        };
        assert_eq!(ReclaimProtocol::for_this_platform(), expected);
    }
}
