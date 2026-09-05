//! Where the fold's write side lives, behind one seam.
//!
//! Everything that MUTATES fold storage — appending a sealed frame, making the active segment
//! durable, sealing one segment and creating the next, punching a dead block's payload — goes
//! through this trait. The read side needs nothing: every segment reader is an `Arc<dyn ReadAt>`
//! from the moment it is built, which lets a closed fold be served from a container-member extent.
//!
//! Two homes exist. [`DirSegments`] is the fold builder's staging implementation: one file per
//! segment, rename-published sidecars, and fsync as the durability barrier. The container-backed
//! implementation writes segments as growing members of the live file, with durability deferred
//! to the commit that names them. [`NoSegments`] refuses every write for a read-only fold.

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

    /// Durability for every append to `seg` so far. File-backed staging fsyncs the segment file.
    /// An implementation whose durability belongs to an enclosing commit protocol may make this a
    /// no-op — and then the protocol's own barrier MUST order these bytes before anything that
    /// names them, because the fold's contract ("no part may name a Loc at or beyond a tail sync
    /// has not returned") is discharged at that barrier instead of here.
    fn sync(&mut self, seg: u32) -> Result<()>;

    /// Head-room check before a roll creates what it creates. File-backed staging admits two more
    /// directory entries; a container layer may instead admit member growth.
    fn admit_roll(&self) -> Result<()>;

    /// Advisory sidecar for a segment being sealed. File-backed staging is best-effort. A container
    /// propagates staging failure so a current commit never publishes a segment that ranged readers
    /// must scan merely to open.
    fn write_sidecar(&mut self, seg: u32, tail: u32, entries: &[(u32, u32)]) -> Result<()>;

    /// Publishable open metadata for the active segment at a commit boundary. File-backed staging
    /// may omit it and pay a scan on reopen. A container writer must stage it in
    /// the same commit as the segment tail: ranged readers otherwise have to fetch the active
    /// segment's block payloads merely to open the store.
    fn stage_active_sidecar(&mut self, seg: u32, tail: u32, entries: &[(u32, u32)]) -> Result<()>;

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

/// The fold builder's file-backed staging side: one file per segment under its work directory.
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

    fn write_sidecar(&mut self, seg: u32, tail: u32, entries: &[(u32, u32)]) -> Result<()> {
        let _ = segment::write_dir_sidecar(&self.dir, seg, tail, entries);
        Ok(())
    }

    fn stage_active_sidecar(
        &mut self,
        _seg: u32,
        _tail: u32,
        _entries: &[(u32, u32)],
    ) -> Result<()> {
        Ok(())
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
    /// `fold` for generation 0 and `fold-NNNN` above it: the manifest-selected generation namespace.
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

    fn write_sidecar(&mut self, seg: u32, tail: u32, entries: &[(u32, u32)]) -> Result<()> {
        // Staged like any member, published by the same commit as the blocks it describes, and
        // never able to describe bytes less durable than itself because both ride one barrier.
        let name = format!("{}/seg-{seg:08}.dir", self.prefix);
        let bytes = segment::encode_dir_sidecar(seg, tail, entries);
        self.container.lock().expect("container lock poisoned").put_bytes(&name, &bytes)
    }

    fn stage_active_sidecar(&mut self, seg: u32, tail: u32, entries: &[(u32, u32)]) -> Result<()> {
        let name = format!("{}/seg-{seg:08}.dir", self.prefix);
        let bytes = segment::encode_dir_sidecar(seg, tail, entries);
        self.container.lock().expect("container lock poisoned").put_bytes(&name, &bytes)
    }

    fn create_segment(
        &mut self,
        next: u32,
        dict_id: [u8; 32],
        flags: u32,
    ) -> Result<Arc<dyn ReadAt>> {
        if flags != 0 {
            bail!("current fold segments require zero flags");
        }
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
    fn write_sidecar(&mut self, _seg: u32, _tail: u32, _entries: &[(u32, u32)]) -> Result<()> {
        bail!("read-only fold cannot write a sidecar")
    }
    fn stage_active_sidecar(
        &mut self,
        _seg: u32,
        _tail: u32,
        _entries: &[(u32, u32)],
    ) -> Result<()> {
        bail!("read-only fold cannot stage an active-segment sidecar")
    }
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
