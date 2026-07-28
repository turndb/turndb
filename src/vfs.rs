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
        Create { path: PathBuf },
        /// Positioned write.
        WriteAt { path: PathBuf, off: u64, data: Vec<u8> },
        /// `set_len` — truncation (or extension) of the file's content.
        SetLen { path: PathBuf, len: u64 },
        /// Whole-file write (create + contents), as `std::fs::write`.
        WriteFile { path: PathBuf, data: Vec<u8> },
        SyncFile { path: PathBuf },
        SyncDir { path: PathBuf },
        Rename { from: PathBuf, to: PathBuf },
        Unlink { path: PathBuf },
        Mkdir { path: PathBuf },
        /// `remove_dir_all`.
        RemoveTree { path: PathBuf },
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
    let f = std::fs::OpenOptions::new().create(true).read(true).write(true).open(path)?;
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

#[inline]
pub(crate) fn unlink(path: &Path) -> Result<()> {
    std::fs::remove_file(path)?;
    #[cfg(feature = "dst")]
    push(Op::Unlink { path: path.to_path_buf() });
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
