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

/// A numeric component for protocol-owned transient names.
///
/// WASI has no process identifier: `std::process::id()` traps there instead of returning a
/// sentinel. One module instance still owns a monotonically increasing serial, and exclusive
/// creation resolves collisions with another instance, so zero is the honest portable component.
#[inline]
pub(crate) fn protocol_process_id() -> u32 {
    #[cfg(target_os = "wasi")]
    {
        0
    }
    #[cfg(not(target_os = "wasi"))]
    {
        std::process::id()
    }
}

#[cfg(windows)]
mod publish {
    //! Windows has no directory fsync, so "create a file, then fsync its directory" cannot make a
    //! new name durable there. What Windows documents instead is `MoveFileExW` with
    //! `MOVEFILE_WRITE_THROUGH`: the call does not return until the move is on disk. So on this
    //! platform a new file is created under a temporary name and `sync_dir(dir)` — the same call
    //! the protocols already make at exactly the point the name must be durable — publishes every
    //! pending file in `dir` with a write-through rename. Call sites do not change; the crash
    //! model does: `SyncDir` on Windows means "publish", and a crash inside it is modelled per
    //! file as published / not yet / neither (tests/dst.rs).
    //!
    //! A pending file is addressed by its FINAL name everywhere above this module; the map here
    //! resolves that name to the temporary one for every operation that reaches the file by path
    //! before it is published — reopen for reading (`open_read`) or writing, existence, metadata,
    //! rename, unlink.
    //!
    //! Crash states, exactly as the simulator models them: before the publish, the final name
    //! does not exist and the bytes sit under a temporary name nobody will ever consult — the
    //! same outcome an unsynced dirent has on POSIX, with the debris visible; during the publish,
    //! old (unpublished) / new (published) / neither; after, published. A temporary name is never
    //! promoted, never read, and never overrides a final name: it is garbage from the moment the
    //! process that created it is gone, and a stale one beside a valid final is exactly that.
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// `(final name, temporary name)` in REGISTRATION order — the order `publish_dir` renames
    /// in, and the order the simulator enumerates partial-publish crash states in.
    static PENDING: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

    pub(super) fn temp_name_for(path: &Path) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".publish-{}-{n}", std::process::id()));
        PathBuf::from(name)
    }

    /// Register `path` as created-but-unpublished at `temp`. A name already pending keeps its
    /// original temp and position: the caller truncated that same file.
    pub(super) fn register(path: &Path, temp: PathBuf) {
        let mut pending = PENDING.lock().unwrap();
        if !pending.iter().any(|(p, _)| p == path) {
            pending.push((path.to_path_buf(), temp));
        }
    }

    /// The temporary name of a pending final name, if it is pending.
    pub(super) fn pending_temp(path: &Path) -> Option<PathBuf> {
        PENDING.lock().unwrap().iter().find(|(p, _)| p == path).map(|(_, t)| t.clone())
    }

    /// Where `path` physically is right now: its temporary name while pending, itself otherwise.
    pub(super) fn resolve(path: &Path) -> PathBuf {
        pending_temp(path).unwrap_or_else(|| path.to_path_buf())
    }

    /// Forget a pending entry because the final name was consumed by a rename or unlink of the
    /// pending file itself.
    pub(super) fn forget(path: &Path) -> Option<PathBuf> {
        let mut pending = PENDING.lock().unwrap();
        let at = pending.iter().position(|(p, _)| p == path)?;
        Some(pending.remove(at).1)
    }

    /// Publish every pending file directly inside `dir`, in registration order. Each publish is a
    /// write-through, no-replace rename; the entry is removed only once its rename returned.
    pub(super) fn publish_dir(dir: &Path) -> std::io::Result<()> {
        let due: Vec<(PathBuf, PathBuf)> = PENDING
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.parent().map(|d| same_dir(d, dir)).unwrap_or(false))
            .cloned()
            .collect();
        for (path, temp) in due {
            crate::sys::rename_noreplace(&temp, &path)?;
            forget(&path);
        }
        Ok(())
    }

    fn same_dir(a: &Path, b: &Path) -> bool {
        let norm =
            |p: &Path| if p.as_os_str().is_empty() { PathBuf::from(".") } else { p.to_path_buf() };
        norm(a) == norm(b)
    }
}

#[cfg(feature = "dst")]
pub mod record {
    //! The recorder: armed per thread by the DST harness, ignored otherwise.
    use std::cell::{Cell, RefCell};
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
        /// A replace-rename whose destination is OPEN by this process (reclaim's final step).
        /// On Windows that takes the POSIX-semantics route, which has no write-through form,
        /// and no documented barrier ever promotes it: the simulator's Windows model carries
        /// old / new / neither for it through every later crash point, not only the one on the
        /// call. On POSIX it is an ordinary rename.
        RenameLagged {
            from: PathBuf,
            to: PathBuf,
        },
        /// `hard_link`: atomically publish another name for an already durable file. Unlike
        /// rename, this refuses when `to` exists, which is artifact installation's no-overwrite gate.
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

    thread_local! {
        // Syncs left before the next one fails, when armed; whether the armed fault fired; and
        // how many syncs were attempted since the last arming — the evidence that the intended
        // site was hit. All per thread: the parallel test runner never shares them.
        static SYNC_FAULT: Cell<Option<usize>> = const { Cell::new(None) };
        static SYNC_FIRED: Cell<Option<usize>> = const { Cell::new(None) };
        static SYNC_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
        static WRITE_FAULT: Cell<Option<usize>> = const { Cell::new(None) };
        static WRITE_FIRED: Cell<Option<usize>> = const { Cell::new(None) };
        static WRITE_ATTEMPTS: Cell<usize> = const { Cell::new(0) };
    }

    /// The distinctive cause of an injected sync failure, findable by `downcast_ref` through
    /// the `io::Error` (`get_ref`) inside a caller's `anyhow` chain — never matched by prose.
    #[derive(Debug)]
    pub struct InjectedSyncFailure {
        /// Which attempted sync (0-based, counted from the arming) failed.
        pub attempt: usize,
    }

    impl std::fmt::Display for InjectedSyncFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "injected sync failure at attempted sync {}", self.attempt)
        }
    }

    impl std::error::Error for InjectedSyncFailure {}

    /// Distinctive cause injected before one positioned write reaches the filesystem.
    #[derive(Debug)]
    pub struct InjectedWriteFailure {
        pub attempt: usize,
    }

    impl std::fmt::Display for InjectedWriteFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "injected write failure at attempted write {}", self.attempt)
        }
    }

    impl std::error::Error for InjectedWriteFailure {}

    /// An armed sync fault on the current thread. Dropping it — including during a panic's
    /// unwinding — disarms the thread, so no later test on this thread inherits it. The state
    /// is thread-local, so the guard is `!Send` and `!Sync`: it can only be dropped on the
    /// thread it armed, which is the whole guarantee.
    ///
    /// ```compile_fail
    /// # #[cfg(feature = "dst")] {
    /// let guard = turndb::vfs::record::fail_sync_after(0);
    /// std::thread::spawn(move || drop(guard)); // error: `SyncFault` cannot be sent between threads
    /// # }
    /// ```
    pub struct SyncFault {
        _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    pub struct WriteFault {
        _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
    }

    impl WriteFault {
        pub fn fired_at(&self) -> Option<usize> {
            WRITE_FIRED.with(|f| f.get())
        }
    }

    impl Drop for WriteFault {
        fn drop(&mut self) {
            WRITE_FAULT.with(|f| f.set(None));
        }
    }

    impl SyncFault {
        /// The attempted-sync index at which the armed fault fired, if it has — the evidence
        /// that the intended sync was the one that failed, whatever syncs followed it.
        pub fn fired_at(&self) -> Option<usize> {
            SYNC_FIRED.with(|f| f.get())
        }
        /// Syncs attempted on this thread since arming, the failed one included.
        pub fn attempts(&self) -> usize {
            SYNC_ATTEMPTS.with(|a| a.get())
        }
    }

    impl Drop for SyncFault {
        fn drop(&mut self) {
            SYNC_FAULT.with(|f| f.set(None));
        }
    }

    /// Arm this thread so that the sync after the next `n` syncs (`sync_file` or `sync_dir`)
    /// returns an I/O error carrying [`InjectedSyncFailure`] instead of syncing, and records
    /// nothing — a sync that failed did not happen, and the crash model's "no sync" states
    /// already cover the result. One-shot; the returned guard disarms on drop.
    pub fn fail_sync_after(n: usize) -> SyncFault {
        SYNC_FAULT.with(|f| f.set(Some(n)));
        SYNC_FIRED.with(|f| f.set(None));
        SYNC_ATTEMPTS.with(|a| a.set(0));
        SyncFault { _not_send: std::marker::PhantomData }
    }

    /// Fail exactly one positioned write on this thread before it mutates the filesystem.
    pub fn fail_write_after(n: usize) -> WriteFault {
        WRITE_FAULT.with(|f| f.set(Some(n)));
        WRITE_FIRED.with(|f| f.set(None));
        WRITE_ATTEMPTS.with(|a| a.set(0));
        WriteFault { _not_send: std::marker::PhantomData }
    }

    pub(super) fn write_fault() -> std::io::Result<()> {
        let attempt = WRITE_ATTEMPTS.with(|a| {
            let n = a.get();
            a.set(n + 1);
            n
        });
        WRITE_FAULT.with(|f| match f.get() {
            None => Ok(()),
            Some(0) => {
                f.set(None);
                WRITE_FIRED.with(|x| x.set(Some(attempt)));
                Err(std::io::Error::other(InjectedWriteFailure { attempt }))
            }
            Some(left) => {
                f.set(Some(left - 1));
                Ok(())
            }
        })
    }

    /// Consulted by every sync: counts the attempt, and fails exactly once when armed and due.
    pub(super) fn sync_fault() -> std::io::Result<()> {
        let attempt = SYNC_ATTEMPTS.with(|a| {
            let n = a.get();
            a.set(n + 1);
            n
        });
        SYNC_FAULT.with(|f| match f.get() {
            None => Ok(()),
            Some(0) => {
                f.set(None);
                SYNC_FIRED.with(|x| x.set(Some(attempt)));
                Err(std::io::Error::other(InjectedSyncFailure { attempt }))
            }
            Some(left) => {
                f.set(Some(left - 1));
                Ok(())
            }
        })
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
    #[cfg(windows)]
    let path = &publish::resolve(path);
    std::fs::OpenOptions::new().read(true).write(true).truncate(false).open(path)
}

/// Open for reading, by name — resolving a name this process created and has not yet published
/// (Windows) to where its bytes are. Reads record nothing: a read cannot change what a crash
/// preserves. Every read of a store file by path goes through here, not `File::open`, for
/// exactly that reason.
#[inline]
pub(crate) fn open_read(path: &Path) -> Result<File> {
    #[cfg(windows)]
    let path = &publish::resolve(path);
    File::open(path)
}

/// Does `path` exist as far as this writer is concerned — including a file it created and has
/// not yet published (Windows)?
#[inline]
pub(crate) fn exists(path: &Path) -> bool {
    #[cfg(windows)]
    let path = &publish::resolve(path);
    path.exists()
}

/// `File::create`: truncate-or-create at `path`. On Windows a name that already exists is
/// truncated in place — its name is already durable, and only its bytes change, which the
/// file's own fsync covers; a name still pending is truncated at its temporary location; only a
/// genuinely new name gets a temporary file and a registration. `std::fs::File::create`'s
/// semantics for callers, on every platform.
#[inline]
pub(crate) fn create(path: &Path) -> Result<File> {
    #[cfg(not(windows))]
    let f = File::create(path)?;
    #[cfg(windows)]
    let f = if let Some(temp) = publish::pending_temp(path) {
        File::create(&temp)?
    } else if path.exists() {
        File::create(path)?
    } else {
        let temp = publish::temp_name_for(path);
        let f = File::create(&temp)?;
        publish::register(path, temp);
        f
    };
    #[cfg(feature = "dst")]
    push(Op::Create { path: path.to_path_buf() });
    Ok(f)
}

/// Read a whole file by name, resolving a pending name (Windows). Records nothing.
#[inline]
pub(crate) fn read_file(path: &Path) -> Result<Vec<u8>> {
    #[cfg(windows)]
    let path = &publish::resolve(path);
    std::fs::read(path)
}

/// Open read-write, creating if absent — and say whether this call CREATED it, because a caller
/// that just created a file owes its directory an fsync before anything durable can depend on the
/// file existing.
#[inline]
pub(crate) fn open_or_create(path: &Path) -> Result<(File, bool)> {
    let existed = exists(path);
    // `truncate(false)` stated explicitly: this opens an EXISTING file to keep working on it, and
    // silently truncating one here would discard a durable WAL or segment.
    let open = |p: &Path| {
        std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(p)
    };
    #[cfg(not(windows))]
    let f = open(path)?;
    #[cfg(windows)]
    let f = if existed {
        open(&publish::resolve(path))?
    } else {
        let temp = publish::temp_name_for(path);
        let f = open(&temp)?;
        publish::register(path, temp);
        f
    };
    #[cfg(feature = "dst")]
    if !existed {
        push(Op::Create { path: path.to_path_buf() });
    }
    Ok((f, !existed))
}

/// `create_new` — exclusive creation, refusing a leftover file. Same recording as [`create`].
#[inline]
pub(crate) fn create_new(path: &Path) -> Result<File> {
    let open =
        |p: &Path| std::fs::OpenOptions::new().create_new(true).write(true).read(true).open(p);
    #[cfg(not(windows))]
    let f = open(path)?;
    #[cfg(windows)]
    let f = {
        // Exclusive against the FINAL name, pending or published: a leftover there is what
        // `create_new` refuses.
        if exists(path) {
            return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
        }
        let temp = publish::temp_name_for(path);
        let f = open(&temp)?;
        publish::register(path, temp);
        f
    };
    #[cfg(feature = "dst")]
    push(Op::Create { path: path.to_path_buf() });
    Ok(f)
}

/// Exclusively create an already-transient protocol name without Windows' additional
/// `.publish-*` indirection. The caller owns the exact name and must synchronize, install, and
/// clean it according to that protocol.
pub(crate) fn create_new_staging(path: &Path) -> Result<File> {
    let file = std::fs::OpenOptions::new().create_new(true).write(true).read(true).open(path)?;
    #[cfg(feature = "dst")]
    push(Op::Create { path: path.to_path_buf() });
    Ok(file)
}

/// Exclusively create a numbered, protocol-owned staging name beside `final_path`. This bypasses
/// Windows' additional pending-publish indirection because the staging name is already transient:
/// callers synchronize it and then install it with [`rename_noreplace`].
pub(crate) fn create_numbered_staging(
    final_path: &Path,
    operation: &str,
) -> Result<(std::path::PathBuf, File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    loop {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut name = final_path.as_os_str().to_os_string();
        name.push(format!(".{operation}-{}-{serial}", protocol_process_id()));
        let path = std::path::PathBuf::from(name);
        match create_new_staging(&path) {
            Ok(file) => {
                return Ok((path, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[inline]
pub(crate) fn write_all_at(f: &File, path: &Path, buf: &[u8], off: u64) -> Result<()> {
    #[cfg(feature = "dst")]
    record::write_fault()?;
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
    #[cfg(not(windows))]
    std::fs::write(path, data)?;
    #[cfg(windows)]
    {
        // Same semantics as `create`: an existing name is rewritten in place, a pending one at
        // its temp, a new one gets a temp and a registration.
        if let Some(temp) = publish::pending_temp(path) {
            std::fs::write(&temp, data)?;
        } else if path.exists() {
            std::fs::write(path, data)?;
        } else {
            let temp = publish::temp_name_for(path);
            std::fs::write(&temp, data)?;
            publish::register(path, temp);
        }
    }
    #[cfg(feature = "dst")]
    push(Op::WriteFile { path: path.to_path_buf(), data: data.to_vec() });
    Ok(())
}

#[inline]
pub(crate) fn sync_file(f: &File, path: &Path) -> Result<()> {
    #[cfg(feature = "dst")]
    record::sync_fault()?;
    f.sync_all()?;
    #[cfg(feature = "dst")]
    push(Op::SyncFile { path: path.to_path_buf() });
    #[cfg(not(feature = "dst"))]
    let _ = path;
    Ok(())
}

#[inline]
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    // `Path::new("mystore.turndb").parent()` is `Some("")`, and every caller reaches here with
    // exactly that when a store is named by a bare relative path — which is how README.md names
    // it. Opening `""` is ENOENT, so the first quickstart command failed (#121). The empty path
    // means the current directory, so sync the current directory.
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    #[cfg(feature = "dst")]
    record::sync_fault()?;
    // Windows: publish every file created in `dir` and not yet named, each by a write-through
    // rename — the documented barrier this platform has in place of a directory fsync. The
    // recorded `SyncDir` then means exactly that, and the simulator's Windows model reads it so.
    #[cfg(windows)]
    publish::publish_dir(dir)?;
    crate::sys::sync_dir(dir)?;
    #[cfg(feature = "dst")]
    push(Op::SyncDir { path: dir.to_path_buf() });
    Ok(())
}

#[inline]
pub(crate) fn rename(from: &Path, to: &Path) -> Result<()> {
    #[cfg(windows)]
    let physical = publish::resolve(from);
    #[cfg(windows)]
    let from_physical: &Path = &physical;
    #[cfg(not(windows))]
    let from_physical: &Path = from;
    crate::sys::rename(from_physical, to)?;
    #[cfg(windows)]
    publish::forget(from);
    #[cfg(feature = "dst")]
    push(Op::Rename { from: from.to_path_buf(), to: to.to_path_buf() });
    Ok(())
}

/// Replace `to` with `from` while `to` is held OPEN by this process — reclaim's final step. On
/// Windows `MoveFileExW` refuses an open destination and `sys::rename` takes the documented
/// POSIX-semantics route, which is not write-through; recorded as its own operation so the crash
/// model carries that uncertainty for as long as it really lasts (see `Op::RenameLagged`).
#[inline]
pub(crate) fn rename_replace_open(from: &Path, to: &Path) -> Result<()> {
    #[cfg(windows)]
    let physical = publish::resolve(from);
    #[cfg(windows)]
    let from_physical: &Path = &physical;
    #[cfg(not(windows))]
    let from_physical: &Path = from;
    crate::sys::rename(from_physical, to)?;
    #[cfg(windows)]
    publish::forget(from);
    #[cfg(feature = "dst")]
    push(Op::RenameLagged { from: from.to_path_buf(), to: to.to_path_buf() });
    Ok(())
}

/// Install `from` at `to` without replacement (an atomic rename on Linux/macOS, an atomic
/// hard-link name creation on WASI; see `sys::rename_noreplace`).
#[inline]
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    #[cfg(windows)]
    let physical = publish::resolve(from);
    #[cfg(windows)]
    let from_physical: &Path = &physical;
    #[cfg(not(windows))]
    let from_physical: &Path = from;
    crate::sys::rename_noreplace(from_physical, to)?;
    #[cfg(windows)]
    publish::forget(from);
    #[cfg(feature = "dst")]
    push(Op::Rename { from: from.to_path_buf(), to: to.to_path_buf() });
    Ok(())
}

#[inline]
pub(crate) fn unlink(path: &Path) -> Result<()> {
    #[cfg(windows)]
    let physical = publish::resolve(path);
    #[cfg(windows)]
    let path_physical: &Path = &physical;
    #[cfg(not(windows))]
    let path_physical: &Path = path;
    std::fs::remove_file(path_physical)?;
    #[cfg(windows)]
    publish::forget(path);
    #[cfg(feature = "dst")]
    push(Op::Unlink { path: path.to_path_buf() });
    Ok(())
}

/// Remove one owned file name, treating an already absent name as the same completed state while
/// propagating every other filesystem failure.
#[inline]
pub(crate) fn unlink_if_exists(path: &Path) -> Result<()> {
    match unlink(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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

#[cfg(all(test, windows))]
mod windows_publish_tests {
    //! The crash states of a pending publish, on the real filesystem: before the publish the
    //! final name does not exist and the temp is garbage to any other process; a stale temp
    //! beside a valid final is never consulted; after the publish only the final name remains.
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("turndb-publish-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn temps_beside(final_name: &Path) -> Vec<std::path::PathBuf> {
        let stem = final_name.file_name().unwrap().to_string_lossy().to_string();
        let mut v: Vec<_> = std::fs::read_dir(final_name.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().unwrap().to_string_lossy();
                n.starts_with(&format!("{stem}.publish-"))
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn an_unpublished_name_is_invisible_to_a_fresh_process_and_its_temp_is_garbage() {
        let d = scratch("unpublished");
        let name = d.join("part-00000001.part");
        let f = create(&name).unwrap();
        write_all_at(&f, &name, b"payload", 0).unwrap();
        sync_file(&f, &name).unwrap();
        // Within this process the name resolves: reads by name see the bytes.
        assert!(exists(&name));
        assert_eq!(std::io::Read::bytes(open_read(&name).unwrap()).count(), 7);
        assert_eq!(temps_beside(&name).len(), 1, "the bytes live under one temp name");
        // A crash before sync_dir: the process is gone, and with it this name's registration.
        // (Per name, not the whole registry: other tests in this process are publishing too.)
        publish::forget(&name);
        assert!(!name.exists(), "the final name was never published");
        assert!(!exists(&name), "and nothing resolves to the temp any more");
        assert!(open_read(&name).is_err(), "a fresh process cannot read it by name");
        assert_eq!(temps_beside(&name).len(), 1, "the temp is debris, still there, never promoted");
        drop(f);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stale_temp_beside_a_valid_final_is_never_consulted_and_a_publish_removes_nothing_else() {
        let d = scratch("stale");
        let name = d.join("part-00000001.part");
        // Debris from an earlier life: two stale temps, one of them torn.
        std::fs::write(d.join("part-00000001.part.publish-1-0"), b"stale one").unwrap();
        std::fs::write(d.join("part-00000001.part.publish-1-1"), b"st").unwrap();
        // A fresh create + publish of the same final name.
        let f = create(&name).unwrap();
        write_all_at(&f, &name, b"the real bytes", 0).unwrap();
        sync_file(&f, &name).unwrap();
        sync_dir(&d).unwrap();
        assert_eq!(std::fs::read(&name).unwrap(), b"the real bytes");
        assert_eq!(
            temps_beside(&name).len(),
            2,
            "stale temps are untouched by an unrelated publish"
        );
        // Reads by name never see a temp, stale or not.
        assert_eq!(std::io::Read::bytes(open_read(&name).unwrap()).count(), 14);
        // And once published, this process's own temp is gone: only the final name is ours.
        assert!(publish::forget(&name).is_none(), "nothing pending after a publish");
        assert!(name.exists());
        drop(f);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Registration order, not lexical order — observed, not assumed: `b` is created before `a`,
    /// and `a`'s final name is blocked by a directory, so the publish fails at `a`. If the order
    /// were lexical, `a` would fail first and `b` would still be pending.
    #[test]
    fn publishing_follows_registration_order_not_lexical_order() {
        let d = scratch("order");
        let b = d.join("b.part");
        let a = d.join("a.part");
        let fb = create(&b).unwrap();
        let fa = create(&a).unwrap();
        sync_file(&fb, &b).unwrap();
        sync_file(&fa, &a).unwrap();
        std::fs::create_dir(&a).unwrap(); // blocks a's publish
        let err = sync_dir(&d).expect_err("a's publish is blocked");
        assert!(err.raw_os_error().is_some(), "{err}");
        assert!(b.is_file(), "b, registered first, was published before a failed");
        assert_eq!(temps_beside(&b).len(), 0);
        assert_eq!(temps_beside(&a).len(), 1, "a is still pending");
        drop((fa, fb));
        publish::forget(&a);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `create` keeps `File::create`'s semantics: an existing final is truncated in place (no
    /// temp — its name is already durable), a pending name is truncated at its one temp, and
    /// `create_new` refuses both.
    #[test]
    fn create_truncates_an_existing_final_in_place_and_a_pending_name_at_its_one_temp() {
        let d = scratch("semantics");
        let existing = d.join("existing.part");
        std::fs::write(&existing, b"old bytes here").unwrap();
        let f = create(&existing).unwrap();
        drop(f);
        assert_eq!(std::fs::read(&existing).unwrap(), b"", "truncated in place");
        assert!(temps_beside(&existing).is_empty(), "no temp for a name that already exists");
        assert!(create_new(&existing).is_err());

        let fresh = d.join("fresh.part");
        let f1 = create(&fresh).unwrap();
        write_all_at(&f1, &fresh, b"first", 0).unwrap();
        drop(f1);
        let f2 = create(&fresh).unwrap();
        drop(f2);
        assert_eq!(temps_beside(&fresh).len(), 1, "a repeated create truncates the same temp");
        assert_eq!(read_file(&fresh).unwrap(), b"", "and it is truncated");
        assert!(create_new(&fresh).is_err(), "pending counts as existing for create_new");
        write_file(&fresh, b"via write_file").unwrap();
        assert_eq!(read_file(&fresh).unwrap(), b"via write_file");
        assert_eq!(temps_beside(&fresh).len(), 1);
        write_file(&existing, b"rewritten").unwrap();
        assert_eq!(std::fs::read(&existing).unwrap(), b"rewritten");
        assert!(temps_beside(&existing).is_empty());
        sync_dir(&d).unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"via write_file");
        assert!(temps_beside(&fresh).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syncing_the_empty_parent_of_a_bare_name_syncs_the_current_directory() {
        // The parent of a bare file name, as `Path::parent` reports it.
        let parent = Path::new("mystore.turndb").parent().unwrap();
        assert!(parent.as_os_str().is_empty());
        sync_dir(parent).expect("an empty parent is the current directory, not ENOENT");
    }
}
