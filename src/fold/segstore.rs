//! Where the fold's write side lives, behind one seam.
//!
//! Everything that MUTATES fold storage — appending a sealed frame, making the active segment
//! durable, sealing one segment and creating the next, punching a dead block's payload — goes
//! through this trait. The read side needs nothing: every segment reader is an `Arc<dyn ReadAt>`
//! from the moment it is built, which is what already lets a sealed fold be served out of a pack
//! extent or a container member.
//!
//! Two homes exist. [`DirSegments`] is the directory store's write side, byte-for-byte the code
//! that always lived in the fold: one file per segment, rename-published sidecars, fsync as the
//! durability barrier. The container-backed implementation lands with the native store's write
//! path — segments as growing members of the live file, durability deferred to the commit that
//! names them. [`NoSegments`] is a read-only fold's answer to every write: a refusal, exactly as
//! the old `active_f: None` was.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use super::segment;
use crate::read_limits::ReadLimits;
use crate::readat::ReadAt;

/// The fold's write-side home. `seg` arguments are absolute segment numbers; offsets are
/// segment-relative, exactly as `Loc` and the block directory speak them.
pub(crate) trait SegmentStore: Send + Sync {
    /// Append `frame` to segment `seg` at segment-relative `off`. `seg` is always the active
    /// segment and `off` always its current append point — the fold owns that arithmetic; the
    /// store owns where the bytes land.
    fn append(&mut self, seg: u32, off: u32, frame: &[u8]) -> Result<()>;

    /// Durability for every append to `seg` so far. The directory store fsyncs the segment file.
    /// An implementation whose durability belongs to an enclosing commit protocol may make this a
    /// no-op — and then the protocol's own barrier MUST order these bytes before anything that
    /// names them, because the fold's contract ("no part may name a Loc at or beyond a tail sync
    /// has not returned") is discharged at that barrier instead of here.
    fn sync(&mut self, seg: u32) -> Result<()>;

    /// Head-room check before a roll creates what it creates. A directory store admits two more
    /// directory entries; a store whose own layer already admits member growth may accept.
    fn admit_roll(&self) -> Result<()>;

    /// Best-effort advisory sidecar for a segment being sealed. Advisory data must never fail a
    /// roll — implementations swallow their own errors.
    fn write_sidecar(&mut self, seg: u32, tail: u32, entries: &[(u32, u32)]);

    /// Create segment `next` — header durable before this returns, where the store has its own
    /// durability — make it the active append target, and hand back its reader.
    fn create_segment(
        &mut self,
        next: u32,
        dict_id: [u8; 32],
        flags: u32,
    ) -> Result<Arc<dyn ReadAt>>;

    /// Deallocate a dead block's payload inside `seg`: `off`/`len` are segment-relative bytes.
    /// The declaration in the manifest, not this call, is the erasure authority.
    fn punch(&mut self, seg: u32, off: u64, len: u64) -> Result<()>;
}

/// The directory store's write side: one file per segment under the fold directory.
pub(crate) struct DirSegments {
    dir: PathBuf,
    active: u32,
    active_f: File,
    read_limits: ReadLimits,
}

impl DirSegments {
    pub(crate) fn new(
        dir: PathBuf,
        active: u32,
        active_f: File,
        read_limits: ReadLimits,
    ) -> DirSegments {
        DirSegments { dir, active, active_f, read_limits }
    }
}

impl SegmentStore for DirSegments {
    fn append(&mut self, seg: u32, off: u32, frame: &[u8]) -> Result<()> {
        debug_assert_eq!(seg, self.active, "appends only ever target the active segment");
        let path = segment::seg_path(&self.dir, seg);
        crate::vfs::write_all_at(&self.active_f, &path, frame, off as u64)?;
        Ok(())
    }

    fn sync(&mut self, seg: u32) -> Result<()> {
        debug_assert_eq!(seg, self.active);
        crate::vfs::sync_file(&self.active_f, &segment::seg_path(&self.dir, seg))
            .context("fsync active fold segment")?;
        Ok(())
    }

    fn admit_roll(&self) -> Result<()> {
        let entries = super::count_fold_directory_entries(&self.dir, self.read_limits)?;
        self.read_limits.admit_directory_entries(
            "fold directory during segment roll",
            entries.saturating_add(2),
        )?;
        Ok(())
    }

    fn write_sidecar(&mut self, seg: u32, tail: u32, entries: &[(u32, u32)]) {
        let _ = segment::write_dir_sidecar(&self.dir, seg, tail, entries);
    }

    fn create_segment(
        &mut self,
        next: u32,
        dict_id: [u8; 32],
        flags: u32,
    ) -> Result<Arc<dyn ReadAt>> {
        let f = segment::create_flagged(&self.dir, next, dict_id, flags)?;
        let reader = Arc::new(segment::open_rw(&self.dir, next)?);
        self.active_f = f;
        self.active = next;
        Ok(reader)
    }

    fn punch(&mut self, seg: u32, off: u64, len: u64) -> Result<()> {
        // Any segment, not only the active one — dead blocks mostly live in sealed history.
        let f = segment::open_rw(&self.dir, seg)?;
        let path = segment::seg_path(&self.dir, seg);
        segment::punch(&f, &path, off, len)
    }
}

/// The container-backed write side: segments are growing members of the live file, named
/// `{prefix}/seg-NNNNNNNN.fold` exactly as their directory forms were, and durability belongs to
/// the container commit that publishes them.
pub(crate) struct ContainerSegments {
    container: std::sync::Arc<std::sync::Mutex<crate::container::Container>>,
    /// `fold` for generation 0, `fold-NNNN` above it — the same namespace every checkpoint wrote.
    prefix: String,
}

impl ContainerSegments {
    pub(crate) fn new(
        container: std::sync::Arc<std::sync::Mutex<crate::container::Container>>,
        prefix: String,
    ) -> ContainerSegments {
        ContainerSegments { container, prefix }
    }

    fn member(&self, seg: u32) -> String {
        format!("{}/{}", self.prefix, segment::seg_name(seg))
    }
}

impl SegmentStore for ContainerSegments {
    fn append(&mut self, seg: u32, off: u32, frame: &[u8]) -> Result<()> {
        let name = self.member(seg);
        let mut c = self.container.lock().expect("container lock poisoned");
        // The fold's append point and the member's staged length are the same number by
        // construction; a disagreement means bytes would land somewhere a Loc does not point.
        let have = c
            .member_len(&name)
            .ok_or_else(|| anyhow::anyhow!("active segment member {name} is missing"))?;
        if have != u64::from(off) {
            bail!(
                "fold append at segment offset {off} but member {name} holds {have} bytes —                  the append point and the member disagree"
            );
        }
        c.append_stream(&name, frame.len() as u64, |at, into| {
            into.copy_from_slice(&frame[at as usize..at as usize + into.len()]);
            Ok(())
        })
    }

    /// Deliberately a no-op. The fold's durability contract — no part may name a Loc at or
    /// beyond a tail `sync` has not returned — is discharged by the container commit's barrier,
    /// which fsyncs every staged byte before the superblock names any of it. Nothing is durable
    /// until that flip, and nothing needs to be: acknowledged records replay from the WAL.
    fn sync(&mut self, _seg: u32) -> Result<()> {
        Ok(())
    }

    /// Member growth is admitted at the container's own layer.
    fn admit_roll(&self) -> Result<()> {
        Ok(())
    }

    fn write_sidecar(&mut self, seg: u32, tail: u32, entries: &[(u32, u32)]) {
        // Advisory: staged like any member, published by the same commit as the blocks it
        // describes, and never able to describe bytes less durable than itself because both
        // ride one barrier.
        let name = format!("{}/seg-{seg:08}.dir", self.prefix);
        let bytes = segment::encode_dir_sidecar(seg, tail, entries);
        if let Ok(mut c) = self.container.lock() {
            let _ = c.put_bytes(&name, &bytes);
        }
    }

    fn create_segment(
        &mut self,
        next: u32,
        dict_id: [u8; 32],
        flags: u32,
    ) -> Result<Arc<dyn ReadAt>> {
        let name = self.member(next);
        let header = segment::SegHeader { seg: next, flags, dict_id }.encode();
        {
            let mut c = self.container.lock().expect("container lock poisoned");
            c.put_bytes(&name, &header)?;
        }
        Ok(Arc::new(crate::container::MemberReader::new(self.container.clone(), name)))
    }

    fn punch(&mut self, seg: u32, off: u64, len: u64) -> Result<()> {
        let name = self.member(seg);
        let c = self.container.lock().expect("container lock poisoned");
        c.punch_within_member(&name, off, len)
    }
}

/// A read-only fold's write side: every call is the refusal `active_f: None` used to be.
pub(crate) struct NoSegments;

impl SegmentStore for NoSegments {
    fn append(&mut self, _seg: u32, _off: u32, _frame: &[u8]) -> Result<()> {
        bail!("read-only fold cannot append")
    }
    fn sync(&mut self, _seg: u32) -> Result<()> {
        bail!("read-only fold cannot sync")
    }
    fn admit_roll(&self) -> Result<()> {
        bail!("read-only fold cannot roll")
    }
    fn write_sidecar(&mut self, _seg: u32, _tail: u32, _entries: &[(u32, u32)]) {}
    fn create_segment(
        &mut self,
        _next: u32,
        _dict_id: [u8; 32],
        _flags: u32,
    ) -> Result<Arc<dyn ReadAt>> {
        bail!("read-only fold cannot roll")
    }
    fn punch(&mut self, _seg: u32, _off: u64, _len: u64) -> Result<()> {
        bail!("read-only fold cannot punch")
    }
}
