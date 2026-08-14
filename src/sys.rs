//! The platform floor: the three OS facilities this engine needs, and what happens where one is
//! missing.
//!
//! Everything else in the crate is portable Rust over `std::fs`. Exactly three operations are not,
//! and each is isolated here rather than cfg-gated at its call sites, so "what does turndb need from
//! an operating system" has one answer you can read in one place.
//!
//! | facility | Unix | WASI | why it is needed |
//! |---|---|---|---|
//! | positioned read/write | `std::os::unix::fs::FileExt` | `std::os::wasi::fs::FileExt` | every read is `n` bytes at offset `o`; seek-then-read is not thread-safe |
//! | advisory whole-file lock | `flock` | **absent** — see [`lock_exclusive`] | the single-writer invariant, enforced by the OS rather than by convention |
//! | hole punching | `fallocate(PUNCH_HOLE)`, Linux | **absent** | erase content in place without moving a single offset |
//!
//! # The honest position on WASI
//!
//! Positioned I/O is genuinely equivalent — WASI's `pread`/`pwrite` are the same primitive under a
//! different module path, so that row costs nothing.
//!
//! The other two are real reductions, and this module makes each one *fail* rather than quietly
//! degrade. Hole punching returns `Unsupported`, which the fold already handles: the caller falls
//! back to a re-fold, which reclaims the same space by rewriting. Locking is the one that cannot be
//! papered over, and [`lock_exclusive`] documents exactly what is lost.

use std::fs::File;
use std::io;
use std::path::Path;

/// Physical bytes currently allocated to a regular file, where the platform exposes that fact.
///
/// Logical length is not a substitute: punched fold blocks remain inside the file's length while
/// consuming no blocks. `None` is an explicit capability absence rather than a fabricated value.
#[cfg(unix)]
pub(crate) fn allocated_bytes(metadata: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().checked_mul(512)
}

#[cfg(not(unix))]
pub(crate) fn allocated_bytes(_metadata: &std::fs::Metadata) -> Option<u64> {
    None
}

/// Bytes available to the current user on the filesystem containing `path`.
#[cfg(unix)]
pub(crate) fn filesystem_available_bytes(path: &Path) -> io::Result<Option<u64>> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    Ok(stats.f_bavail.checked_mul(stats.f_frsize))
}

#[cfg(not(unix))]
pub(crate) fn filesystem_available_bytes(_path: &Path) -> io::Result<Option<u64>> {
    Ok(None)
}

// ── Positioned I/O ──────────────────────────────────────────────────────────
//
// The same `pread`/`pwrite` on both platforms. Unix reaches them through the stable `FileExt`;
// WASI's equivalent trait is still unstable (`wasi_ext`), so we call the preview1 syscalls
// directly, which is stable and is what that trait would do anyway.
//
// Both loop, because a short `pread`/`pwrite` is legal and means "call again", not "end of file".
// Only a zero-length read at a non-empty request is EOF.

/// Fill `buf` from `off`, exactly, or error.
#[cfg(unix)]
#[inline]
pub(crate) fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, off)
}

/// Write all of `buf` at `off`, or error.
#[cfg(unix)]
#[inline]
pub(crate) fn write_all_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, buf, off)
}

#[cfg(target_os = "wasi")]
fn wasi_err(e: wasi::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw().into())
}

#[cfg(target_os = "wasi")]
pub(crate) fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd() as u32;
    while !buf.is_empty() {
        let iov = [wasi::Iovec { buf: buf.as_mut_ptr(), buf_len: buf.len() }];
        let n = unsafe { wasi::fd_pread(fd, &iov, off) }.map_err(wasi_err)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "positioned read hit end of file before filling the buffer",
            ));
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(target_os = "wasi")]
pub(crate) fn write_all_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd() as u32;
    while !buf.is_empty() {
        let iov = [wasi::Ciovec { buf: buf.as_ptr(), buf_len: buf.len() }];
        let n = unsafe { wasi::fd_pwrite(fd, &iov, off) }.map_err(wasi_err)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "positioned write made no progress",
            ));
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(not(any(unix, target_os = "wasi")))]
pub(crate) fn read_exact_at(_f: &File, _buf: &mut [u8], _off: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this target has no filesystem positioned-read primitive; use a custom ReadAt source",
    ))
}

#[cfg(not(any(unix, target_os = "wasi")))]
pub(crate) fn write_all_at(_f: &File, _buf: &[u8], _off: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this target has no filesystem positioned-write primitive",
    ))
}

// ── Whole-file advisory lock ────────────────────────────────────────────────

/// Take an exclusive advisory lock on `f`, or report that another writer holds it.
///
/// `Ok(true)` means the lock is held for as long as `f` is open. `Ok(false)` means another writer
/// has it. An `Err` is a real I/O failure, distinct from contention.
///
/// # Where this is weaker
///
/// On Unix this is `flock`, which the kernel releases when the file descriptor closes — including
/// when the process is killed, and including when it crashes. That property is what makes it a
/// *safe* single-writer gate: a stale lock cannot outlive its owner.
///
/// **WASI has no advisory locking**, so this returns `Ok(true)` unconditionally. The single-writer
/// invariant is then the embedder's to keep, and it is not an academic concern: in the one overlap
/// pattern that has been measured, both writers were acknowledged as durable and one writer's whole
/// record set was silently lost from a store that still verified clean. FORMAT.md, "The writer
/// lock", is the normative account and the only one; do not restate it here. The intended
/// deployment is one store per Node process, where the runtime provides the exclusion the OS
/// otherwise would. A lockfile is *not* a
/// substitute — an `O_EXCL` file survives a hard kill and would wedge the store closed with no safe
/// way to tell a stale lock from a live one.
///
/// This is a documented reduction in a guarantee the format states, not an oversight; FORMAT.md
/// says so too.
#[inline]
pub(crate) fn lock_exclusive(f: &File) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        // EWOULDBLOCK is contention — another writer holds it. Anything else is a real failure and
        // must not be reported as "someone else has it".
        match e.raw_os_error() {
            Some(c) if c == libc::EWOULDBLOCK || c == libc::EAGAIN => Ok(false),
            _ => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = f;
        Ok(true)
    }
}

// ── Hole punching ───────────────────────────────────────────────────────────

/// Deallocate `len` bytes at `off`, leaving the file's length untouched.
///
/// Linux only. Everywhere else this returns [`io::ErrorKind::Unsupported`], which callers already
/// treat as "re-fold instead" — reclaiming the same space by rewriting rather than in place.
#[inline]
pub(crate) fn punch_hole(f: &File, off: u64, len: u64) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe {
            libc::fallocate(
                f.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                off as libc::off_t,
                len as libc::off_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (f, off, len);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hole punching needs fallocate(FALLOC_FL_PUNCH_HOLE), which this platform lacks",
        ))
    }
}

// ── Exclusive rename ──────────────────────────────────────────────────────

/// Atomically rename `from` to `to` while refusing to replace any existing filesystem object.
///
/// This is stronger than `exists()` followed by `std::fs::rename`: ordinary POSIX rename may
/// replace an empty directory created between those calls. Backup restoration uses this as its
/// final publication point, so a concurrent creator can make restoration fail but cannot lose its
/// destination.
#[cfg(target_os = "linux")]
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let from = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let from = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
pub(crate) fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this platform",
    ))
}
