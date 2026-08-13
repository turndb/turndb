//! Every WRITE-SIDE filesystem operation, behind one thin seam.
//!
//! By default each function here is an `#[inline]` passthrough to `std::fs` — zero cost, zero
//! behavior change, and the compiler erases the indirection. With the `dst` feature, an armed
//! thread-local recorder ALSO logs every operation, bytes included, which is what lets the
//! deterministic-simulation harness reconstruct every crash state a real power loss could leave:
//! which writes were issued, which were made durable by an fsync, which dirents a directory fsync
//! had promoted, and which rename was still in flight.
//!
//! The discipline this module exists to enforce: **the store's crash-safety argument is only as
//! good as the completeness of this log.** A mutating operation that bypasses these wrappers is
//! invisible to the simulator and untested at every crash point. Reads do not come through here —
//! a read cannot change what a crash preserves.

use std::fs::File;
use std::io::Result;
use std::path::Path;

#[cfg(feature = "dst")]
pub mod record {
    //! The recorder: armed per thread by the DST harness, ignored otherwise.
    use std::cell::RefCell;
    use std::path::PathBuf;

    /// One mutating operation, bytes included. `SyncFile`/`SyncDir` are the durability barriers
    /// the simulator honours; everything else is volatile until one covers it.
    #[derive(Clone, Debug)]
    pub enum Op {
        /// `File::create`: truncate-or-create. The dirent is NOT durable until its dir syncs.
        Create {
            path: PathBuf,
        },
        /// Positioned write.
        WriteAt {
            path: PathBuf,
            off: u64,
            data: Vec<u8>,
        },
        /// `set_len` — truncation (or extension) of the file's content.
        SetLen {
            path: PathBuf,
            len: u64,
        },
        /// Whole-file write (create + contents), as `std::fs::write`.
        WriteFile {
            path: PathBuf,
            data: Vec<u8>,
        },
        SyncFile {
            path: PathBuf,
        },
        SyncDir {
            path: PathBuf,
        },
        Rename {
            from: PathBuf,
            to: PathBuf,
        },
        /// `hard_link`: atomically publish another name for an already durable file. Unlike
        /// rename, this refuses when `to` exists, which is the pack writer's no-overwrite gate.
        Link {
            from: PathBuf,
            to: PathBuf,
        },
        Unlink {
            path: PathBuf,
        },
        /// `fallocate(PUNCH_HOLE)`: deallocate a range in place. The file keeps its length and the
        /// range reads back as zeros — the one operation that destroys committed bytes without
        /// moving a name, which is exactly why the simulator must see it.
        PunchHole {
            path: PathBuf,
            off: u64,
            len: u64,
        },
        Mkdir {
            path: PathBuf,
        },
        /// `remove_dir_all`.
        RemoveTree {
            path: PathBuf,
        },
    }

    thread_local! {
        static LOG: RefCell<Option<Vec<Op>>> = const { RefCell::new(None) };
    }

    /// Start recording on this thread. Any prior log is discarded.
    pub fn arm() {
        LOG.with(|l| *l.borrow_mut() = Some(Vec::new()));
    }

    /// Stop recording and take the log.
    pub fn disarm() -> Vec<Op> {
        LOG.with(|l| l.borrow_mut().take().unwrap_or_default())
    }

    pub(super) fn push(op: Op) {
        LOG.with(|l| {
            if let Some(v) = l.borrow_mut().as_mut() {
                v.push(op);
            }
        });
    }

    /// The number of ops recorded so far — a crash-point cursor for the harness.
    pub fn len() -> usize {
        LOG.with(|l| l.borrow().as_ref().map_or(0, |v| v.len()))
    }
}

#[cfg(feature = "dst")]
use record::{push, Op};

/// Where a self-contained artifact's bytes land: a file of its own, or a member region inside a
/// container. The artifact's internal offsets stay artifact-relative either way — the sink is
/// what maps them to a place.
///
/// Writes arrive strictly sequentially (`off` always equals the bytes written so far); a sink may
/// rely on that, for example to hash the artifact in the same pass that writes it. `sync` is the
/// artifact's own completeness barrier where it has one — a sink whose durability belongs to an
/// enclosing commit protocol makes it a no-op and its documentation says so.
pub(crate) trait ArtifactSink {
    fn write_all_at(&mut self, data: &[u8], off: u64) -> std::io::Result<()>;
    fn sync(&mut self) -> std::io::Result<()>;
    /// For error context: what a human should read when a write into this sink fails.
    fn describe(&self) -> String;
}

/// Open an existing file read-write without truncation — the reopen an interrupted creation
/// needs. Records nothing: opening mutates no state the crash model tracks.
#[inline]
pub(crate) fn open_rw(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new().read(true).write(true).truncate(false).open(path)
}

#[inline]
pub(crate) fn create(path: &Path) -> Result<File> {
    let f = File::create(path)?;
    #[cfg(feature = "dst")]
    push(Op::Create { path: path.to_path_buf() });
    Ok(f)
}

/// Open read-write, creating if absent — and say whether this call CREATED it, because a caller
/// that just created a file owes its directory an fsync before anything durable can depend on the
/// file existing.
#[inline]
pub(crate) fn open_or_create(path: &Path) -> Result<(File, bool)> {
    let existed = path.exists();
    // `truncate(false)` stated explicitly: this opens an EXISTING file to keep working on it, and
    // silently truncating one here would discard a durable WAL or segment.
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    #[cfg(feature = "dst")]
    if !existed {
        push(Op::Create { path: path.to_path_buf() });
    }
    Ok((f, !existed))
}

/// `create_new` — exclusive creation, refusing a leftover file. Same recording as [`create`].
#[inline]
pub(crate) fn create_new(path: &Path) -> Result<File> {
    let f = std::fs::OpenOptions::new().create_new(true).write(true).read(true).open(path)?;
    #[cfg(feature = "dst")]
    push(Op::Create { path: path.to_path_buf() });
    Ok(f)
}

#[inline]
pub(crate) fn write_all_at(f: &File, path: &Path, buf: &[u8], off: u64) -> Result<()> {
    crate::sys::write_all_at(f, buf, off)?;
    #[cfg(feature = "dst")]
    push(Op::WriteAt { path: path.to_path_buf(), off, data: buf.to_vec() });
    #[cfg(not(feature = "dst"))]
    let _ = path;
    Ok(())
}

#[inline]
pub(crate) fn set_len(f: &File, path: &Path, len: u64) -> Result<()> {
    f.set_len(len)?;
    #[cfg(feature = "dst")]
    push(Op::SetLen { path: path.to_path_buf(), len });
    #[cfg(not(feature = "dst"))]
    let _ = path;
    Ok(())
}

#[inline]
pub(crate) fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data)?;
    #[cfg(feature = "dst")]
    push(Op::WriteFile { path: path.to_path_buf(), data: data.to_vec() });
    Ok(())
}

#[inline]
pub(crate) fn sync_file(f: &File, path: &Path) -> Result<()> {
    f.sync_all()?;
    #[cfg(feature = "dst")]
    push(Op::SyncFile { path: path.to_path_buf() });
    #[cfg(not(feature = "dst"))]
    let _ = path;
    Ok(())
}

#[inline]
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    #[cfg(feature = "dst")]
    push(Op::SyncDir { path: dir.to_path_buf() });
    Ok(())
}

#[inline]
pub(crate) fn rename(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to)?;
    #[cfg(feature = "dst")]
    push(Op::Rename { from: from.to_path_buf(), to: to.to_path_buf() });
    Ok(())
}

/// Atomic rename that refuses to replace `to`.
#[inline]
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    crate::sys::rename_noreplace(from, to)?;
    #[cfg(feature = "dst")]
    push(Op::Rename { from: from.to_path_buf(), to: to.to_path_buf() });
    Ok(())
}

#[inline]
pub(crate) fn unlink(path: &Path) -> Result<()> {
    std::fs::remove_file(path)?;
    #[cfg(feature = "dst")]
    push(Op::Unlink { path: path.to_path_buf() });
    Ok(())
}

/// Deallocate `len` bytes at `off` in place (`fallocate(PUNCH_HOLE)` where the platform has it).
/// Volatile until the file's fsync, like any other data mutation — a crash may leave the range
/// intact, fully zeroed, or partially zeroed, and the simulator models all three.
#[inline]
pub(crate) fn punch_hole(f: &File, path: &Path, off: u64, len: u64) -> Result<()> {
    crate::sys::punch_hole(f, off, len)?;
    #[cfg(feature = "dst")]
    push(Op::PunchHole { path: path.to_path_buf(), off, len });
    #[cfg(not(feature = "dst"))]
    let _ = path;
    Ok(())
}

#[inline]
pub(crate) fn mkdir_all(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(feature = "dst")]
    push(Op::Mkdir { path: path.to_path_buf() });
    Ok(())
}

#[inline]
pub(crate) fn remove_tree(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path)?;
    #[cfg(feature = "dst")]
    push(Op::RemoveTree { path: path.to_path_buf() });
    Ok(())
}
