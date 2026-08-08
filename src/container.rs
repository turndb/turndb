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
//! the members are byte-identical to their directory and pack forms, and
//! [`Container::extent`] hands them to [`Part::open_reader`](crate::part::Part::open_reader) and
//! [`Fold::open_read_from`](crate::fold::Fold::open_read_from) with no translation.
//!
//! **Space is reclaimed by rewriting, not by reuse.** Freed extents are recorded so the waste is
//! reportable, but allocation only ever appends. Reusing a freed extent would hand a reader holding
//! an older superblock a range whose bytes are now something else — silent corruption rather than a
//! detected fault — and nothing here tracks reader generations yet. The same posture the engine
//! takes with `refold`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::part::idcol::{get_varint, put_varint};
use crate::readat::Slice;

/// Head of every superblock. A file that does not start with it is not a container.
pub const MAGIC: &[u8; 8] = b"TURNCTNR";

/// The container plane's reject-forward lever, independent of the record format version.
pub const CONTAINER_VERSION: u8 = 1;

/// Each superblock slot is a whole page: a slot write is one `pwrite` that cannot straddle two.
pub const SLOT_LEN: u64 = 4096;

/// First byte a member may occupy.
pub const REGION_START: u64 = SLOT_LEN * 2;

/// Bytes of a slot the format actually defines; the rest is zero and reserved.
const SB_LEN: usize = 56;

/// Suffix of the working directory a writer keeps beside a container, mirroring SQLite's `-wal`.
/// Named here because file-level operations on a container have to know whether one exists.
pub const HOT_SUFFIX: &str = "-hot";

/// Refuse a directory that claims more than this compressed, before allocating for it.
const MAX_DIR_STORED: u32 = 64 << 20;
/// Refuse a directory that claims more than this decompressed.
const MAX_DIR_RAW: u32 = 256 << 20;
/// Refuse a container claiming more members than a store could plausibly have.
const MAX_MEMBERS: u32 = 1_000_000;
/// Longest member name accepted.
const MAX_NAME: u64 = 16 << 10;

/// `(offset, length, crc32)` — the same shape a pack TOC entry carries.
type Entry = (u64, u64, u32);

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
        slot[49] = CONTAINER_VERSION;
        // slot[50..52] reserved, already zero
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
        if slot[49] != CONTAINER_VERSION {
            bail!(
                "container superblock declares version {}, and this build reads {CONTAINER_VERSION}",
                slot[49]
            );
        }
        if slot[50..52] != [0, 0] {
            bail!("container superblock sets reserved bits that must be zero");
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
        }))
    }
}

/// A store's artifacts in one mutable file.
pub struct Container {
    f: Arc<File>,
    path: PathBuf,
    dir: BTreeMap<String, Entry>,
    free: Vec<(u64, u64)>,
    tail: u64,
    seq: u64,
    /// The slot the live state was read from; the next commit writes the other one.
    slot: u8,
    /// Staged members exist in the file but in no committed superblock until `commit`.
    staged: bool,
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
            let _ = crate::vfs::sync_dir(parent);
        }
        Ok(Container {
            f: Arc::new(f),
            path: path.to_path_buf(),
            dir: BTreeMap::new(),
            free: Vec::new(),
            tail: REGION_START,
            seq: 0,
            slot: 0,
            staged: false,
        })
    }

    /// Open an existing container at its newest committed state.
    pub fn open(path: &Path) -> Result<Container> {
        let (f, created) = crate::vfs::open_or_create(path)
            .with_context(|| format!("open container {}", path.display()))?;
        if created {
            // `open_or_create` cannot be given O_EXCL semantics here, so an absent file would
            // otherwise silently become an empty one that reports zero members.
            let _ = crate::vfs::unlink(path);
            bail!("not a container: {} does not exist", path.display());
        }
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
            bail!(
                "container {} is truncated: committed tail {} exceeds file length {len}",
                path.display(),
                live.tail
            );
        }

        let f = Arc::new(f);
        let (dir, free) = read_directory(&f, path, &live)?;
        Ok(Container {
            f,
            path: path.to_path_buf(),
            dir,
            free,
            tail: live.tail,
            seq: live.seq,
            slot,
            staged: false,
        })
    }

    /// The committed sequence this handle is reading.
    pub fn seq(&self) -> u64 {
        self.seq
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

    /// Bytes occupied by superseded members — reclaimable only by rewriting the container.
    pub fn free_bytes(&self) -> u64 {
        self.free.iter().map(|(_, len)| *len).sum()
    }

    /// Bytes the members themselves occupy.
    pub fn member_bytes(&self) -> u64 {
        self.dir.values().map(|(_, len, _)| *len).sum()
    }

    /// A member as a positioned reader. This is the seam: the returned range is exactly the
    /// artifact's bytes, so a part or fold segment opens from it with no translation.
    pub fn extent(&self, name: &str) -> Option<Slice<Arc<File>>> {
        let &(off, len, _) = self.dir.get(name)?;
        Some(Slice::new(self.f.clone(), off, len))
    }

    /// Read a small member whole, refusing anything larger than `max_bytes` before allocating.
    pub fn read_file_bounded(&self, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let &(off, len, _) = self
            .dir
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("container member not found: {name}"))?;
        if len > max_bytes {
            bail!("container member {name} is {len} bytes, over the {max_bytes} byte ceiling");
        }
        let mut buf = Vec::new();
        buf.try_reserve_exact(len as usize)?;
        buf.resize(len as usize, 0);
        crate::sys::read_exact_at(&self.f, &mut buf, off)?;
        Ok(buf)
    }

    /// Stage a member from bytes. Visible only after [`Container::commit`].
    pub fn put_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        validate_name(name)?;
        let off = self.tail;
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
        validate_name(name)?;
        let off = self.tail;
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
        validate_name(name)?;
        let mut src =
            File::open(from).with_context(|| format!("ingest source {}", from.display()))?;
        let off = self.tail;
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

    /// Stage a removal. The extent is recorded as free but never reused by this handle.
    pub fn remove(&mut self, name: &str) -> Result<bool> {
        match self.dir.remove(name) {
            Some((off, len, _)) => {
                self.free.push((off, len));
                self.staged = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn stage_entry(&mut self, name: &str, off: u64, len: u64, xsum: u32) {
        if let Some((old_off, old_len, _)) = self.dir.insert(name.to_string(), (off, len, xsum)) {
            self.free.push((old_off, old_len));
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
        if !self.staged {
            return Ok(self.seq);
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
            seq: self.seq + 1,
            dir_off,
            dir_stored: stored.len() as u32,
            dir_raw: payload.len() as u32,
            n_entries: self.dir.len() as u32,
            dir_xsum: crc32fast::hash(&stored),
            tail,
            dir_codec,
        };
        let next_slot = 1 - self.slot;
        crate::vfs::write_all_at(
            &self.f,
            &self.path,
            &sb.encode(),
            u64::from(next_slot) * SLOT_LEN,
        )?;
        crate::vfs::sync_file(&self.f, &self.path)?;

        self.seq = sb.seq;
        self.tail = tail;
        self.slot = next_slot;
        self.staged = false;
        Ok(self.seq)
    }

    /// Re-read every member and check it against the checksum recorded for it.
    pub fn verify(&self) -> Result<usize> {
        let mut buf = vec![0u8; 1 << 20];
        for (name, &(off, len, want)) in &self.dir {
            let mut hasher = crc32fast::Hasher::new();
            let mut at = 0u64;
            while at < len {
                let take = std::cmp::min(buf.len() as u64, len - at) as usize;
                crate::sys::read_exact_at(&self.f, &mut buf[..take], off + at)?;
                hasher.update(&buf[..take]);
                at += take as u64;
            }
            let got = hasher.finalize();
            if got != want {
                bail!("container member {name} fails its checksum: {got:08x} != {want:08x}");
            }
        }
        Ok(self.dir.len())
    }
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
/// checkpointed daily accumulates dead space forever, and this is the only thing that returns it.
///
/// The rewrite is a copy to a fresh file and an atomic rename, not an edit: at no point is the
/// container being read half-rewritten, and a crash leaves the original untouched. A reader
/// holding the old file keeps reading it — the inode outlives the name.
///
/// Refused while a writer's working directory exists beside the file, because that directory holds
/// state the container has not been told about and rewriting would publish a version of the
/// container that is about to be superseded by a checkpoint of writes it never saw.
pub fn reclaim(path: &Path) -> Result<ReclaimStats> {
    let mut hot = path.as_os_str().to_os_string();
    hot.push(HOT_SUFFIX);
    if Path::new(&hot).exists() {
        bail!(
            "{} has a writer's working directory beside it; settle or close that writer first",
            path.display()
        );
    }

    let source = Container::open(path)?;
    let bytes_before = std::fs::metadata(path)?.len();
    if source.free_bytes() == 0 {
        return Ok(ReclaimStats {
            members: source.len(),
            bytes_before,
            bytes_after: bytes_before,
            reclaimed: 0,
        });
    }

    let staging = path.with_extension("reclaiming");
    let _ = crate::vfs::unlink(&staging);
    let mut fresh = Container::create(&staging)?;
    for name in source.names().map(String::from).collect::<Vec<_>>() {
        let extent = source
            .extent(&name)
            .ok_or_else(|| anyhow::anyhow!("container lost member {name} mid-reclaim"))?;
        let len = crate::readat::ReadAt::len(&extent)?;
        // Streamed: a part is the largest thing a store holds, and a reclaim that has to hold one
        // in memory would fail on exactly the containers most worth reclaiming.
        fresh.put_stream(&name, len, |at, into| {
            crate::readat::ReadAt::read_exact_at(&extent, into, at)
        })?;
    }
    let members = fresh.len();
    fresh.commit()?;
    fresh.verify()?;
    drop(fresh);

    let bytes_after = std::fs::metadata(&staging)?.len();
    crate::vfs::rename(&staging, path)?;
    if let Some(parent) = path.parent() {
        let _ = crate::vfs::sync_dir(parent);
    }
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

fn encode_directory(dir: &BTreeMap<String, Entry>, free: &[(u64, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, dir.len() as u64);
    for (name, &(off, len, xsum)) in dir {
        put_varint(&mut out, name.len() as u64);
        out.extend_from_slice(name.as_bytes());
        put_varint(&mut out, off);
        put_varint(&mut out, len);
        out.extend_from_slice(&xsum.to_le_bytes());
    }
    put_varint(&mut out, free.len() as u64);
    for &(off, len) in free {
        put_varint(&mut out, off);
        put_varint(&mut out, len);
    }
    out
}

type Directory = (BTreeMap<String, Entry>, Vec<(u64, u64)>);

fn read_directory(f: &Arc<File>, path: &Path, sb: &Superblock) -> Result<Directory> {
    let mut dir = BTreeMap::new();
    if sb.n_entries == 0 && sb.dir_stored == 0 {
        return Ok((dir, Vec::new()));
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
    crate::sys::read_exact_at(f, &mut stored, sb.dir_off)?;
    if crc32fast::hash(&stored) != sb.dir_xsum {
        bail!("container {} directory fails its checksum", path.display());
    }
    let payload = crate::fold::codec::decode(sb.dir_codec, &stored, sb.dir_raw, None)?;

    let mut at = 0usize;
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
        let off = get_varint(&payload, &mut at)?;
        let len = get_varint(&payload, &mut at)?;
        let xsum_bytes =
            payload.get(at..at + 4).context("container directory checksum truncated")?;
        let xsum = u32::from_le_bytes(xsum_bytes.try_into()?);
        at += 4;
        validate_name(&name)?;
        // Every member must lie inside the committed region: a directory that points past the
        // tail is corruption, and it must be refused before anything reads through it.
        if off < REGION_START || off + len > sb.tail {
            bail!("container {} member {name} lies outside its committed region", path.display());
        }
        if dir.insert(name.clone(), (off, len, xsum)).is_some() {
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
        free.push((off, len));
    }
    Ok((dir, free))
}
