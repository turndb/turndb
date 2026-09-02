//! The platform floor: the three OS facilities this engine needs, and what happens where one is
//! missing.
//!
//! Everything else in the crate is portable Rust over `std::fs`. Exactly three operations are not,
//! and each is isolated here rather than cfg-gated at its call sites, so "what does turndb need from
//! an operating system" has one answer you can read in one place.
//!
//! | facility | Unix | Windows | WASI | why it is needed |
//! |---|---|---|---|---|
//! | positioned read/write | `std::os::unix::fs::FileExt` | `std::os::windows::fs::FileExt` (`seek_read`/`seek_write`, offset per call) | `std::os::wasi::fs::FileExt` | every read is `n` bytes at offset `o`; seek-then-read is not thread-safe |
//! | advisory whole-file lock | `flock` | `LockFileEx` on one byte past any real offset — see [`lock_exclusive`] | **absent** — see [`lock_exclusive`] | the single-writer invariant, enforced by the OS rather than by convention |
//! | hole punching | `fallocate(PUNCH_HOLE)`, Linux | `FSCTL_SET_ZERO_DATA` on a sparse file — see [`punch_hole`] | **absent** | erase content in place without moving a single offset |
//!
//! Two more are not in that table because Unix gets them from `std` for free and Windows does
//! not: a **durable rename** ([`rename`], [`rename_noreplace`] — `MoveFileExW` with
//! `MOVEFILE_WRITE_THROUGH`, because `std::fs::rename` on Windows passes no write-through flag)
//! and a **directory sync** ([`sync_dir`] — a no-op on Windows, which has no directory fsync;
//! FORMAT.md states the durability model that replaces it).
//!
//! Where the platforms differ in what an operation *guarantees* rather than in how it is spelled,
//! the difference is declared here as a fact — [`replace_open_durability`] — and a protocol above
//! this module chooses its steps by that fact, never by `cfg!(windows)`. That is the separation
//! that keeps one platform's constraint from becoming every platform's protocol: the same
//! discipline SQLite's pager gets from its VFS's device characteristics.
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
pub(crate) fn allocated_bytes(_path: &Path, metadata: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().checked_mul(512)
}

/// Windows has no allocation count on a metadata record; `GetCompressedFileSizeW` answers by name
/// and, for a sparse or compressed file, reports the bytes actually allocated — which is what a
/// punched range gives back. A failure is an explicit `None`, never a logical length in disguise.
#[cfg(windows)]
pub(crate) fn allocated_bytes(path: &Path, _metadata: &std::fs::Metadata) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::{GetCompressedFileSizeW, INVALID_FILE_SIZE};
    let wide = wide_path(path).ok()?;
    let mut high: u32 = 0;
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if low == INVALID_FILE_SIZE && io::Error::last_os_error().raw_os_error() != Some(0) {
        return None;
    }
    Some(((high as u64) << 32) | low as u64)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn allocated_bytes(_path: &Path, _metadata: &std::fs::Metadata) -> Option<u64> {
    None
}

/// A NUL-terminated UTF-16 path for the wide Win32 entry points, with the long-path handling
/// `std` applies to its own calls: a path that would exceed the classic `MAX_PATH` limit is
/// made absolute and given the `\\?\` verbatim prefix (`\\?\UNC\` for a network path), which
/// lifts the limit to 32,767 characters. Shorter paths are passed as given, exactly as `std`
/// passes them.
#[cfg(windows)]
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Prefix};

    const MAX_PATH: usize = 260;
    let plain: Vec<u16> = path.as_os_str().encode_wide().collect();
    // An interior NUL would silently truncate the name at the Win32 boundary; `std` refuses it
    // with `InvalidInput`, and so does this.
    if plain.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"));
    }
    // The Win32 limit counts the terminating NUL.
    if plain.len() < MAX_PATH {
        return Ok(plain.into_iter().chain(std::iter::once(0)).collect());
    }
    let already_verbatim = matches!(
        path.components().next(),
        Some(Component::Prefix(p)) if p.kind().is_verbatim()
    );
    if already_verbatim {
        return Ok(plain.into_iter().chain(std::iter::once(0)).collect());
    }
    // Absolute and normalized (`GetFullPathNameW` underneath), because a verbatim path is taken
    // literally: no `.`/`..` resolution and no `/` separators once the prefix is on.
    let absolute = std::path::absolute(path)?;
    let mut out: Vec<u16> = Vec::with_capacity(absolute.as_os_str().len() + 8);
    let unc = matches!(
        absolute.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::UNC(..))
    );
    if unc {
        // `\\server\share\...` becomes `\\?\UNC\server\share\...`.
        out.extend("\\\\?\\UNC".encode_utf16());
        let rest: Vec<u16> = absolute.as_os_str().encode_wide().skip(1).collect();
        out.extend(rest);
    } else {
        out.extend("\\\\?\\".encode_utf16());
        out.extend(absolute.as_os_str().encode_wide());
    }
    for c in &mut out {
        if *c == b'/' as u16 {
            *c = b'\\' as u16;
        }
    }
    // Sanity: the result must still round-trip as an OsString; a malformed conversion is a bug
    // here, never something to hand the OS.
    debug_assert!(!std::ffi::OsString::from_wide(&out).is_empty());
    out.push(0);
    Ok(out)
}

#[cfg(unix)]
fn checked_filesystem_bytes<A: Into<u64>, B: Into<u64>>(blocks: A, block_size: B) -> Option<u64> {
    blocks.into().checked_mul(block_size.into())
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
    // libc follows each OS's statvfs ABI: Darwin exposes f_bavail as u32 while Linux uses u64.
    // Normalize both operands without narrowing before checking the byte-count multiplication.
    Ok(checked_filesystem_bytes(stats.f_bavail, stats.f_frsize))
}

/// `GetDiskFreeSpaceExW` wants a directory; callers pass one, but a file path is mapped to its
/// parent rather than failing, so the probe never depends on which the caller happened to hold.
#[cfg(windows)]
pub(crate) fn filesystem_available_bytes(path: &Path) -> io::Result<Option<u64>> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let dir = if path.is_file() { path.parent().unwrap_or_else(|| Path::new(".")) } else { path };
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let wide = wide_path(dir)?;
    let mut available: u64 = 0;
    let rc = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(available))
}

#[cfg(not(any(unix, windows)))]
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

// Windows: `seek_read`/`seek_write` hand the offset to `ReadFile`/`WriteFile` through an
// OVERLAPPED per call, so two threads reading the same `File` at different offsets each get the
// bytes they asked for — the cursor they also happen to move is never consulted. Both loop, as
// on Unix: a short transfer means "call again".
#[cfg(windows)]
pub(crate) fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, off) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "positioned read hit end of file before filling the buffer",
                ))
            }
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn write_all_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_write(buf, off) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "positioned write made no progress",
                ))
            }
            Ok(n) => {
                buf = &buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows, target_os = "wasi")))]
pub(crate) fn read_exact_at(_f: &File, _buf: &mut [u8], _off: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this target has no filesystem positioned-read primitive; use a custom ReadAt source",
    ))
}

#[cfg(not(any(unix, windows, target_os = "wasi")))]
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
///
/// **On Windows this is `LockFileEx`**, which the OS releases when the handle closes or the
/// process terminates — the same property that makes `flock` safe. One difference matters:
/// a Windows byte-range lock is *mandatory*, not advisory — an exclusive lock denies other
/// handles both reads and writes of the locked range. Readers of a live store never take the
/// lock and must keep reading, so the lock covers exactly one byte at offset 2^64 − 2, past any
/// offset a file can hold and past anything a reader ever asks for (locking beyond end-of-file
/// is explicitly permitted). Contention is `ERROR_LOCK_VIOLATION`; anything else is a real
/// failure.
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
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_LOCK_VIOLATION};
        use windows_sys::Win32::Storage::FileSystem::{
            LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        };
        use windows_sys::Win32::System::IO::OVERLAPPED;

        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.Anonymous.Anonymous.Offset = LOCK_BYTE_OFFSET as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (LOCK_BYTE_OFFSET >> 32) as u32;
        let rc = unsafe {
            LockFileEx(
                f.as_raw_handle() as _,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if rc != 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(c) if c == ERROR_LOCK_VIOLATION as i32 || c == ERROR_IO_PENDING as i32 => {
                Ok(false)
            }
            _ => Err(e),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = f;
        Ok(true)
    }
}

/// The one byte the Windows writer lock covers: past every offset a file can have, so the
/// mandatory lock never intersects a read.
#[cfg(windows)]
const LOCK_BYTE_OFFSET: u64 = u64::MAX - 1;

// ── Hole punching ───────────────────────────────────────────────────────────

/// Deallocate `len` bytes at `off`, leaving the file's length untouched.
///
/// Linux: `fallocate(PUNCH_HOLE | KEEP_SIZE)`. Windows: the file is marked sparse once
/// (`FSCTL_SET_SPARSE`, idempotent) and the range zeroed with `FSCTL_SET_ZERO_DATA`. Microsoft's
/// contract for that call is exact on the bytes and best-effort on the space: the range reads
/// back as zeros without the length changing, and "if the file is sparse or compressed, the NTFS
/// file system *may* deallocate disk space" — in practice at NTFS's 64 KiB sparse granularity,
/// so a hole smaller than that is zeroed but not returned. The erasure contract (bytes gone,
/// offsets unmoved) therefore holds exactly on Windows; how much space comes back is what
/// `allocated_bytes` measures rather than something this function promises. Everywhere else
/// this returns [`io::ErrorKind::Unsupported`], which callers already treat as "re-fold
/// instead" — reclaiming the same space by rewriting rather than in place.
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
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Ioctl::{
            FILE_ZERO_DATA_INFORMATION, FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA,
        };
        use windows_sys::Win32::System::IO::DeviceIoControl;

        let end = off.checked_add(len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "punch range overflows u64")
        })?;
        if end > i64::MAX as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "punch range exceeds the Windows file offset space",
            ));
        }
        let h = f.as_raw_handle() as _;
        let mut returned: u32 = 0;
        let rc = unsafe {
            DeviceIoControl(
                h,
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if rc == 0 {
            return Err(io::Error::last_os_error());
        }
        let info =
            FILE_ZERO_DATA_INFORMATION { FileOffset: off as i64, BeyondFinalZero: end as i64 };
        let rc = unsafe {
            DeviceIoControl(
                h,
                FSCTL_SET_ZERO_DATA,
                (&info as *const FILE_ZERO_DATA_INFORMATION).cast(),
                std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if rc == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (f, off, len);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hole punching needs fallocate(FALLOC_FL_PUNCH_HOLE), which this platform lacks",
        ))
    }
}

// ── Exclusive rename ──────────────────────────────────────────────────────

/// Rename `from` to `to` while refusing to replace any existing filesystem object — on Linux
/// (`renameat2(RENAME_NOREPLACE)`) and macOS (`renamex_np(RENAME_EXCL)`) as one atomic step; the
/// Windows form below makes no atomicity claim.
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

/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` refuses an existing destination
/// (`ERROR_ALREADY_EXISTS`), and `MOVEFILE_WRITE_THROUGH` makes the function "not return until
/// the file is actually moved on the disk" — the durability a directory fsync provides on Unix
/// and Windows has no other way to ask for. No crash-atomicity is claimed: the proof admits
/// source-only, destination-only, and neither.
#[cfg(windows)]
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
    let (from, to) = (wide_path(from)?, wide_path(to)?);
    let rc = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if rc == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios", windows)))]
pub(crate) fn rename_noreplace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename is unavailable on this platform",
    ))
}

// ── Replace rename and directory sync ─────────────────────────────────────
//
// On Unix these are `std::fs::rename` — POSIX `rename(2)`, which replaces the destination as one
// atomic step — and an fsync of the directory, which is what makes the new name durable. Windows
// has no directory fsync. What it has instead is `MOVEFILE_WRITE_THROUGH`, so the rename itself
// is the durability barrier on return, and the directory sync becomes a no-op. Microsoft's
// contract for `MoveFileExW` says what a *successful* call has done and nothing about the state a
// crash during the call can leave; the crash-safety proof therefore admits a state in which the
// old destination is gone and the source has not landed (`RenameNeither` in tests/dst.rs), and
// no comment in this crate calls a Windows rename atomic. FORMAT.md, "Durability on Windows", states the model the
// crash-safety proof runs under on this platform, built from documented operations only: a name
// is durable when the operation that produced it was write-through, and laggable otherwise.

/// Replace `to` with `from`: POSIX `rename(2)`, atomic with respect to every observer; durable
/// once the caller syncs the directory.
#[cfg(not(windows))]
#[inline]
pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// Replace `to` with `from`. On success the destination has been replaced and, because
/// `std::fs::rename` on Windows is `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` with no write-through
/// flag (library/std/src/sys/fs/windows.rs, Rust 1.95), this adds `MOVEFILE_WRITE_THROUGH` so the
/// function returns only once the move is on disk. That is the whole claim: what a crash during
/// the call leaves behind is not documented, and the proof models it as old, new, or neither.
///
/// **When the destination is open** — a reader, or `reclaim` itself, which holds the writer lock
/// on the container it is replacing (FORMAT.md, "Free space") — `MoveFileExW` refuses with
/// `ERROR_ACCESS_DENIED`; Windows will not replace a file with open handles by that route. The
/// documented route that will is `FileRenameInfoEx` with `FILE_RENAME_FLAG_REPLACE_IF_EXISTS |
/// FILE_RENAME_FLAG_POSIX_SEMANTICS` (the target must have been opened with
/// `FILE_SHARE_DELETE`, which `std` always does), and that is exactly `std::fs::rename`'s own
/// fallback on the same error, so it is used here rather than re-implemented. It has no
/// write-through flag: on that path the rename is **not** durable on return. That fact is what
/// [`replace_open_durability`] declares as `Lagged`, and `reclaim` reads it to choose the anchor
/// protocol here rather than the one rename that suffices under `Atomic`.
#[cfg(windows)]
pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let (wfrom, wto) = (wide_path(from)?, wide_path(to)?);
    let rc = unsafe {
        MoveFileExW(
            wfrom.as_ptr(),
            wto.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if rc != 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        // The destination is open: take std's POSIX-semantics route. Not write-through.
        return std::fs::rename(from, to);
    }
    Err(e)
}

/// Make the names inside `dir` durable: an fsync of the directory itself.
#[cfg(not(windows))]
#[inline]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows has no directory fsync, and opening a directory as a file is refused. A name is made
/// durable only by a write-through rename ([`rename`], [`rename_noreplace`]); `FlushFileBuffers`
/// covers a file's own bytes and nothing in the namespace. This is a no-op by design, and the DST
/// harness's Windows model gives `SyncDir` no meaning.
#[cfg(windows)]
#[inline]
pub(crate) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

// ── What the platform guarantees, stated once ─────────────────────────────
//
// The arms above spell each operation in its platform's idiom. What a protocol needs to know is
// not the spelling but the guarantee — and where the guarantee differs between platforms, it is
// declared here as a value a protocol can branch on and a test can force on any host. No protocol
// in this crate consults `cfg!(windows)`; it consults this.

/// What a replace-rename over a destination that is OPEN — by a reader, or by the very writer
/// performing the replace — guarantees on this platform once it returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplaceOpenDurability {
    /// `rename(2)`: one atomic step to every observer, durable once the directory is synced. A
    /// crash leaves the old name or the new one, never neither. A protocol may replace the live
    /// name directly and needs nothing durable across the step.
    Atomic,
    /// The replace lands, but the route that replaces an open destination has no write-through
    /// form and no later documented barrier promotes it (Windows: `FileRenameInfoEx` with POSIX
    /// semantics, the fallback [`rename`] takes on `ERROR_ACCESS_DENIED`). A crash may leave old,
    /// new, or neither — at that crash point and every later one. A protocol must hold a durable
    /// anchor across the step, and recover from it.
    Lagged,
}

/// The guarantee this build's platform gives a replace over an open destination. Windows is the
/// one platform whose documented operations do not make such a replace durable; every other
/// target this crate builds for gets POSIX `rename(2)` semantics from `std`.
#[inline]
pub(crate) const fn replace_open_durability() -> ReplaceOpenDurability {
    if cfg!(windows) {
        ReplaceOpenDurability::Lagged
    } else {
        ReplaceOpenDurability::Atomic
    }
}

#[cfg(test)]
mod guarantee_tests {
    use super::*;

    /// The platform fact, stated as the platform fact — not derived from the function under test.
    #[test]
    fn the_replace_guarantee_is_lagged_on_windows_and_atomic_everywhere_else() {
        let expected = if cfg!(target_os = "windows") {
            ReplaceOpenDurability::Lagged
        } else {
            ReplaceOpenDurability::Atomic
        };
        assert_eq!(replace_open_durability(), expected);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    //! The writer lock on Windows is a *mandatory* byte-range lock, so the proof that it gates
    //! writers without touching readers has to exercise both reader paths Microsoft documents:
    //! ordinary reads, which fail when they overlap a locked range, and mapped views, which ignore
    //! byte-range locks entirely. The far-offset byte must be invisible to both.
    use super::*;
    use std::io::Write;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("turndb-sys-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn open_rw(p: &Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(p)
            .unwrap()
    }

    #[test]
    fn second_writer_is_refused_and_the_lock_dies_with_its_handle() {
        let p = scratch("lock");
        let a = open_rw(&p);
        assert!(lock_exclusive(&a).unwrap(), "first writer takes the lock");
        let b = open_rw(&p);
        assert!(!lock_exclusive(&b).unwrap(), "second handle sees contention, not an error");
        drop(a);
        assert!(lock_exclusive(&b).unwrap(), "closing the holder's handle releases the lock");
        drop(b);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn an_ordinary_reader_reads_the_whole_file_under_a_writer_lock() {
        let p = scratch("lock-read");
        let mut w = open_rw(&p);
        w.write_all(&[7u8; 100_000]).unwrap();
        assert!(lock_exclusive(&w).unwrap());
        let r = File::open(&p).unwrap();
        let mut buf = vec![0u8; 100_000];
        read_exact_at(&r, &mut buf, 0).expect("a lock-free reader must not be denied");
        assert!(buf.iter().all(|&b| b == 7));
        // ... and the same through the plain sequential path a CLI tool would take.
        let all = std::fs::read(&p).unwrap();
        assert_eq!(all.len(), 100_000);
        drop(r);
        drop(w);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_mapped_view_reads_the_whole_file_under_a_writer_lock() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, PAGE_READONLY,
        };

        let p = scratch("lock-map");
        let mut w = open_rw(&p);
        w.write_all(&[9u8; 65_536]).unwrap();
        w.sync_all().unwrap();
        assert!(lock_exclusive(&w).unwrap());
        let r = File::open(&p).unwrap();
        unsafe {
            let mapping = CreateFileMappingW(
                r.as_raw_handle() as _,
                std::ptr::null(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            );
            assert!(!mapping.is_null(), "CreateFileMappingW: {}", io::Error::last_os_error());
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            assert!(!view.Value.is_null(), "MapViewOfFile: {}", io::Error::last_os_error());
            let bytes = std::slice::from_raw_parts(view.Value as *const u8, 65_536);
            assert!(bytes.iter().all(|&b| b == 9), "mapped view must read every byte");
            assert_ne!(UnmapViewOfFile(view), 0);
            assert_ne!(CloseHandle(mapping), 0);
        }
        drop(r);
        drop(w);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn punch_zeroes_the_range_and_keeps_the_length() {
        let p = scratch("punch");
        let mut f = open_rw(&p);
        let body: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8 + 1).collect();
        f.write_all(&body).unwrap();
        f.sync_all().unwrap();
        // 64 KiB at a 64 KiB boundary: the one shape NTFS documents as deallocatable; the bytes
        // are the contract, the space is a measurement.
        punch_hole(&f, 65_536, 65_536).expect("FSCTL_SET_ZERO_DATA on a sparse file");
        f.sync_all().unwrap();
        assert_eq!(f.metadata().unwrap().len(), body.len() as u64, "length unchanged");
        let mut back = vec![0u8; body.len()];
        read_exact_at(&f, &mut back, 0).unwrap();
        assert_eq!(&back[..65_536], &body[..65_536]);
        assert!(back[65_536..131_072].iter().all(|&b| b == 0), "punched range reads as zeros");
        assert_eq!(&back[131_072..], &body[131_072..]);
        let allocated = allocated_bytes(&p, &f.metadata().unwrap());
        println!(
            "allocated after punch: {allocated:?} of {} logical (measured, not asserted)",
            body.len()
        );
        drop(f);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn rename_noreplace_refuses_an_existing_destination_and_replace_rename_does_not() {
        let a = scratch("ren-a");
        let b = scratch("ren-b");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let err = rename_noreplace(&a, &b).expect_err("destination exists");
        assert!(err.raw_os_error().is_some());
        assert_eq!(std::fs::read(&b).unwrap(), b"b", "destination untouched");
        rename(&a, &b).expect("replace rename");
        assert_eq!(std::fs::read(&b).unwrap(), b"a");
        assert!(!a.exists());
        let _ = std::fs::remove_file(&b);
    }

    /// A path with an interior NUL is refused the way `std` refuses it, never truncated at the
    /// Win32 boundary into a different name.
    #[test]
    fn a_path_with_an_interior_nul_is_invalid_input_not_a_truncated_name() {
        use std::os::windows::ffi::OsStringExt;
        let bad = std::ffi::OsString::from_wide(&[b'a' as u16, 0, b'b' as u16]);
        let bad = Path::new(&bad);
        let err = wide_path(bad).expect_err("interior NUL");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(rename(bad, bad).expect_err("rename").kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            rename_noreplace(bad, bad).expect_err("rename_noreplace").kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            filesystem_available_bytes(bad).expect_err("free space").kind(),
            io::ErrorKind::InvalidInput
        );
        let meta = std::fs::metadata(std::env::temp_dir()).unwrap();
        assert_eq!(allocated_bytes(bad, &meta), None);
    }

    /// Every raw call here goes through `wide_path`, so the one thing that could silently regress
    /// against `std` is the classic 260-character limit. Build a path well past it — under the
    /// current directory, so the RELATIVE form is exercised deterministically rather than only
    /// when the temp directory happens to sit under cwd — and drive rename, no-replace rename,
    /// free space and allocation through both forms.
    #[test]
    fn paths_past_max_path_work_for_rename_and_the_space_measurements() {
        let base = Path::new("target").join(format!("turndb-long-{}", std::process::id()));
        let mut rel = base.clone();
        while rel.as_os_str().len() < 300 {
            rel.push("a-deliberately-long-directory-component-0123456789");
        }
        // Relative and past the limit on its own: `std::fs` handles it, so every raw call must.
        std::fs::create_dir_all(&rel).unwrap();
        assert!(rel.is_relative() && rel.as_os_str().len() > 260);
        {
            let a = rel.join("rel-a.turndb");
            let b = rel.join("rel-b.turndb");
            std::fs::write(&a, b"a").unwrap();
            rename_noreplace(&a, &b).expect("relative no-replace rename past MAX_PATH");
            std::fs::write(&a, b"a2").unwrap();
            rename(&a, &b).expect("relative replace rename past MAX_PATH");
            assert_eq!(std::fs::read(&b).unwrap(), b"a2");
            assert!(filesystem_available_bytes(&rel).unwrap().is_some());
            assert!(filesystem_available_bytes(&b).unwrap().is_some());
            assert!(allocated_bytes(&b, &std::fs::metadata(&b).unwrap()).is_some());
        }
        // The absolute form of the same directory.
        let dir = std::path::absolute(&rel).unwrap();
        assert!(dir.is_absolute() && dir.as_os_str().len() > 260);
        let a = dir.join("a.turndb");
        let b = dir.join("b.turndb");
        let c = dir.join("c.turndb");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        rename_noreplace(&a, &c).expect("no-replace rename past MAX_PATH");
        assert!(c.exists() && !a.exists());
        rename_noreplace(&c, &b).expect_err("destination exists");
        rename(&c, &b).expect("replace rename past MAX_PATH");
        assert_eq!(std::fs::read(&b).unwrap(), b"a");
        assert!(filesystem_available_bytes(&dir).unwrap().is_some());
        assert!(filesystem_available_bytes(&b).unwrap().is_some());
        assert_eq!(
            allocated_bytes(&b, &std::fs::metadata(&b).unwrap()).map(|n| n >= 1),
            Some(true)
        );
        drop(std::fs::remove_dir_all(&base));
    }

    /// `reclaim` replaces a container it holds open and locked. `MoveFileExW` refuses that;
    /// the POSIX-semantics fallback does not, and the old handle keeps reading the old bytes.
    #[test]
    fn replace_rename_over_an_open_locked_destination_succeeds_and_the_old_handle_survives() {
        let a = scratch("open-a");
        let b = scratch("open-b");
        std::fs::write(&a, b"fresh").unwrap();
        let mut old = open_rw(&b);
        old.write_all(b"old").unwrap();
        old.sync_all().unwrap();
        assert!(lock_exclusive(&old).unwrap());
        rename(&a, &b).expect("replace over an open, locked destination");
        assert_eq!(std::fs::read(&b).unwrap(), b"fresh", "the name now serves the fresh bytes");
        let mut back = [0u8; 3];
        read_exact_at(&old, &mut back, 0).unwrap();
        assert_eq!(&back, b"old", "the old handle still reads the old file");
        drop(old);
        let _ = std::fs::remove_file(&b);
    }

    /// After the POSIX-semantics replace, the lock taken on the SOURCE handle must still gate the
    /// name — asserted, not inferred from handle identity.
    #[test]
    fn the_candidate_lock_survives_the_replace_fallback_and_gates_the_new_name() {
        let cand = scratch("handoff-cand");
        let store = scratch("handoff-store");
        std::fs::write(&cand, b"new").unwrap();
        std::fs::write(&store, b"old").unwrap();
        let old = open_rw(&store);
        assert!(lock_exclusive(&old).unwrap(), "old store locked, as reclaim holds it");
        let new = open_rw(&cand);
        assert!(lock_exclusive(&new).unwrap(), "candidate locked before publication");
        rename(&cand, &store).expect("replace over the open, locked destination (fallback)");
        let third = open_rw(&store);
        assert!(
            !lock_exclusive(&third).unwrap(),
            "a writer at the name meets the candidate's lock"
        );
        drop(old);
        let fourth = open_rw(&store);
        assert!(
            !lock_exclusive(&fourth).unwrap(),
            "the old handle's lock was never what gated the new name"
        );
        drop(new);
        assert!(lock_exclusive(&fourth).unwrap(), "released with the candidate handle");
        drop(third);
        drop(fourth);
        let _ = std::fs::remove_file(&store);
    }

    #[test]
    fn free_space_is_measured_for_a_directory_and_for_a_file_path() {
        let p = scratch("space");
        std::fs::write(&p, b"x").unwrap();
        let by_dir = filesystem_available_bytes(&std::env::temp_dir()).unwrap();
        let by_file = filesystem_available_bytes(&p).unwrap();
        assert!(by_dir.is_some() && by_file.is_some());
        let _ = std::fs::remove_file(&p);
    }
}
