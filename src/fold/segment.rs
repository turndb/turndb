//! Segment files: the append-only containers holding block frames.

use super::block::{self, BLOCK_HDR_LEN, BLOCK_XSUM_LEN};
use crate::readat::ReadAt;
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// Segment header length. The first block begins here, so a `Loc.block_off` below it is invalid.
pub const SEG_HDR_LEN: u64 = 48;

/// Identity assertion, not a version. A mismatch refuses — there is no negotiation anywhere.
pub const MAGIC: &[u8; 8] = b"TURNFOLD";

/// Segment flag: this segment's block payloads are ENCRYPTED, and reading them requires keys this
/// build may not have.
///
/// RESERVED, not yet written by anything — and reserved *early on purpose*. The whole value of a
/// reject-forward lever is that shipped readers already refuse the thing before any writer can
/// produce it; a bit reserved at the same moment it is first set protects nobody who is already
/// running. This is the same discipline the part footer's reserved bytes follow, applied in
/// advance of the work that will need it.
pub const SEG_FLAG_ENCRYPTED: u32 = 1 << 0;

/// Every flag bit this build understands. Anything outside it refuses the open.
pub const KNOWN_SEG_FLAGS: u32 = SEG_FLAG_ENCRYPTED;

/// Default roll threshold. Bounded so a crash-time tail scan stays short and mmap granularity stays sane.
pub const SEG_MAX_DEFAULT: u32 = 1 << 30;

/// Format bound. `Loc.block_off` is a `u32`, so a block start must fit in one.
pub const SEG_MAX_LIMIT: u64 = 1 << 32;

pub fn seg_name(n: u32) -> String {
    format!("seg-{n:08}.fold")
}

/// Parse a segment file name **numerically**. Lexicographic ordering breaks once the `%08` width is
/// exceeded, and a mis-ordered segment list corrupts recovery.
pub fn parse_seg_name(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("seg-")?.strip_suffix(".fold")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u32>().ok()
}

/// The 48-byte segment header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegHeader {
    pub seg: u32,
    /// See [`SEG_FLAG_ENCRYPTED`]. Zero for a plaintext segment.
    pub flags: u32,
    /// BLAKE3 of this segment's trained dictionary, or all-zero for "no dictionary".
    pub dict_id: [u8; 32],
}

impl SegHeader {
    pub fn has_dict(&self) -> bool {
        self.dict_id != [0u8; 32]
    }

    pub fn is_encrypted(&self) -> bool {
        self.flags & SEG_FLAG_ENCRYPTED != 0
    }

    pub fn encode(&self) -> [u8; SEG_HDR_LEN as usize] {
        let mut b = [0u8; SEG_HDR_LEN as usize];
        b[0..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&self.seg.to_le_bytes());
        b[12..16].copy_from_slice(&self.flags.to_le_bytes());
        b[16..48].copy_from_slice(&self.dict_id);
        b
    }

    /// Decode + validate. `expect_seg` is the number parsed from the filename; the header must agree,
    /// which catches a renamed, copied, or cross-wired segment.
    pub fn decode(b: &[u8], expect_seg: u32) -> Result<SegHeader> {
        if b.len() < SEG_HDR_LEN as usize {
            bail!("segment header truncated: {} bytes", b.len());
        }
        if &b[0..8] != MAGIC {
            bail!("not a turndb fold segment (bad magic)");
        }
        let seg = u32::from_le_bytes(b[8..12].try_into().unwrap());
        if seg != expect_seg {
            bail!("segment header says seg {seg} but the file is named for {expect_seg}");
        }
        let flags = u32::from_le_bytes(b[12..16].try_into().unwrap());
        // The reject-forward lever. Bits this build knows are accepted and acted on; anything
        // else stops the open, because unknown means stop, not adapt. An encrypted segment is
        // openable — the fold reports ERASED per block when no key opens it, which is the honest
        // answer — but a bit from a future format is not, and never becomes one by guessing.
        if flags & !KNOWN_SEG_FLAGS != 0 {
            bail!("segment flags {flags:#x} unknown — refusing (no compatibility negotiation)");
        }
        let mut dict_id = [0u8; 32];
        dict_id.copy_from_slice(&b[16..48]);
        Ok(SegHeader { seg, flags, dict_id })
    }
}

/// fsync a directory so a create/rename within it is durable.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    crate::vfs::sync_dir(dir).with_context(|| format!("fsync dir {}", dir.display()))
}

/// Create segment `n` with its header durable before any frame can follow it.
///
/// `O_EXCL`: a leftover file is never appended to. Combined with fsyncing the header before returning,
/// this makes "file exists but is shorter than a header" provably hold no durable frame — which is what
/// lets recovery delete such a file safely.
pub fn create(dir: &Path, n: u32, dict_id: [u8; 32]) -> Result<File> {
    create_flagged(dir, n, dict_id, 0)
}

/// [`create`], with segment flags — how an encrypted segment announces itself.
pub fn create_flagged(dir: &Path, n: u32, dict_id: [u8; 32], flags: u32) -> Result<File> {
    let path = dir.join(seg_name(n));
    let f = crate::vfs::create_new(&path)
        .with_context(|| format!("create segment {}", path.display()))?;
    crate::vfs::write_all_at(&f, &path, &SegHeader { seg: n, flags, dict_id }.encode(), 0)?;
    crate::vfs::sync_file(&f, &path)?;
    fsync_dir(dir)?;
    Ok(f)
}

/// Read-only handle — never opens for write, so a reader cannot damage a store it does not own.
pub fn open_read(dir: &Path, n: u32) -> Result<File> {
    let path = dir.join(seg_name(n));
    File::open(&path).with_context(|| format!("open segment {} read-only", path.display()))
}

pub fn open_rw(dir: &Path, n: u32) -> Result<File> {
    let path = dir.join(seg_name(n));
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open segment {}", path.display()))
}

/// Deallocate `len` bytes at `off` — the extents are freed and read back as zeros, and the file's
/// length is untouched, so every offset in it still means what it meant.
///
/// This is the one operation that destroys committed fold bytes in place. It is a Linux
/// filesystem feature (ext4, xfs, btrfs, tmpfs...); where the kernel or filesystem declines, the
/// error surfaces and the caller falls back to a re-fold, which reclaims the same space by
/// rewriting rather than by punching.
///
/// **Only a block's PAYLOAD is ever punched, never its header.** The frame chain is walked by
/// reading a header and stepping over `stored` bytes, so a punched header would end the chain and
/// silently orphan every block after it in the segment. Sixteen surviving header bytes carry no
/// content — they carry the length that keeps the chain walkable, and the `block_id` that lets a
/// scan report the erasure by name.
pub fn punch(f: &File, off: u64, len: u64) -> Result<()> {
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
        let e = std::io::Error::last_os_error();
        bail!("punching {len} bytes at {off} failed ({e}) — this filesystem may not support hole punching; re-fold instead");
    }
    f.sync_all()?;
    Ok(())
}

/// Walk the block chain to the last **complete, checksum-valid** block. Returns the offset just past
/// it plus every `(block_id, offset)` seen, which is how the block directory is rebuilt at open.
///
/// This is how the fold finds its own last good byte with no external length authority. It never
/// decompresses: read 12 bytes, hash `12 + stored`, advance. The first failure of any kind is the end
/// of good data — during a tail scan a bad frame is a boundary, not an error.
pub fn scan_tail(f: &dyn ReadAt, file_len: u64, has_dict: bool) -> Result<(u64, Vec<(u32, u32)>)> {
    scan_tail_in(f, file_len, has_dict, false)
}

/// [`scan_tail`], told whether the segment is encrypted — the frame invariants differ.
pub fn scan_tail_in(f: &dyn ReadAt, file_len: u64, has_dict: bool, encrypted: bool) -> Result<(u64, Vec<(u32, u32)>)> {
    let mut off = SEG_HDR_LEN;
    let mut hdr = [0u8; BLOCK_HDR_LEN];
    let mut payload = Vec::new();
    // Blocks land in COMPLETION order, so position no longer implies identity — the directory is
    // rebuilt from the ids the frames carry.
    let mut dir: Vec<(u32, u32)> = Vec::new();
    loop {
        if off + (block::BLOCK_OVERHEAD as u64) > file_len {
            break; // cannot hold even an empty block
        }
        if f.read_exact_at(&mut hdr, off).is_err() {
            break;
        }
        let h = match block::parse_hdr_in(&hdr, has_dict, encrypted) {
            Ok(h) => h,
            Err(_) => break,
        };
        let end = match off.checked_add(BLOCK_HDR_LEN as u64 + h.stored as u64 + BLOCK_XSUM_LEN as u64) {
            Some(e) => e,
            None => break,
        };
        if end > file_len {
            break; // the promised payload/checksum never fully reached disk
        }
        // verify the whole block frame's bytes
        let span = (BLOCK_HDR_LEN + h.stored as usize + BLOCK_XSUM_LEN) as usize;
        payload.resize(span, 0);
        if f.read_exact_at(&mut payload[..span], off).is_err() {
            break;
        }
        if block::verify_frame_bytes_in(&payload[..span], has_dict, encrypted).is_err() {
            // A PUNCHED block: header intact, payload deallocated to zeros. The chain steps over
            // it (that is what keeping the header buys) and the block is left OUT of the
            // directory, so nothing can resolve a Loc into erased bytes. Anything else that fails
            // here is a torn write, and ends the segment's valid span.
            let body = &payload[BLOCK_HDR_LEN..span - BLOCK_XSUM_LEN];
            if h.stored > 0 && body.iter().all(|&b| b == 0) {
                off = end;
                continue;
            }
            break;
        }
        dir.push((h.block_id, off as u32));
        off = end;
    }
    Ok((off, dir))
}

/// The durable end of the fold: everything strictly before this is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct FoldTail {
    pub seg: u32,
    pub off: u32,
}

// ---------------------------------------------------------------------------------------------
// The directory sidecar — advisory, derived, and the answer to O(store) opens
// ---------------------------------------------------------------------------------------------

/// Sidecar magic. `seg-NNNNNNNN.dir` beside a SEALED segment carries what `scan_tail` would
/// recompute: the block ids and offsets, and the scan end.
pub const DIR_MAGIC: &[u8; 8] = b"TURNSDIR";

/// ```text
/// offset  size  field
///      0     8  MAGIC = "TURNSDIR"
///      8     4  seg          must match the filename AND the segment beside it
///     12     4  tail         scan end; for a sealed segment this IS the file length
///     16     4  n_entries
///     20   n*8  (block_id u32, offset u32) per block
///  20+n*8    4  crc32 over everything before it
/// ```
pub fn dir_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(format!("seg-{n:08}.dir"))
}

/// Write the sidecar for a sealed segment. ADVISORY, so tmp + rename but no fsync anywhere: a
/// sidecar lost to a crash costs one rescan at the next open, and a torn one fails its checksum
/// and costs the same. Nothing durable depends on it.
pub fn write_dir_sidecar(dir: &Path, n: u32, tail: u32, entries: &[(u32, u32)]) -> Result<()> {
    let mut b = Vec::with_capacity(24 + entries.len() * 8);
    b.extend_from_slice(DIR_MAGIC);
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&tail.to_le_bytes());
    b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (id, off) in entries {
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&off.to_le_bytes());
    }
    let crc = crc32fast::hash(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    let tmp = dir.join(format!("seg-{n:08}.dir.tmp"));
    crate::vfs::write_file(&tmp, &b)?;
    crate::vfs::rename(&tmp, &dir_path(dir, n))?;
    Ok(())
}

/// The sidecar's entries, or `None` — absent, damaged, or not describing this file's bytes. The
/// caller rescans on `None`; it must never trust a sidecar this refused.
///
/// `tail == file_len` is the staleness gate, not a nicety: recovery can truncate a once-sealed
/// segment back into being the active one, and its leftover sidecar then describes blocks past
/// the committed tail. A sealed segment ends exactly at its last block, so any length mismatch
/// means the sidecar and the segment parted ways.
pub fn read_dir_sidecar(dir: &Path, n: u32, file_len: u64) -> Option<(u32, Vec<(u32, u32)>)> {
    parse_dir_sidecar(&std::fs::read(dir_path(dir, n)).ok()?, n, file_len)
}

/// [`read_dir_sidecar`]'s validation core, over bytes from ANY source — a directory or a pack.
pub fn parse_dir_sidecar(b: &[u8], n: u32, file_len: u64) -> Option<(u32, Vec<(u32, u32)>)> {
    if b.len() < 24 || &b[0..8] != DIR_MAGIC {
        return None;
    }
    let crc = u32::from_le_bytes(b[b.len() - 4..].try_into().unwrap());
    if crc32fast::hash(&b[..b.len() - 4]) != crc {
        return None;
    }
    let seg = u32::from_le_bytes(b[8..12].try_into().unwrap());
    let tail = u32::from_le_bytes(b[12..16].try_into().unwrap());
    let n_entries = u32::from_le_bytes(b[16..20].try_into().unwrap()) as usize;
    if seg != n || tail as u64 != file_len || b.len() != 24 + n_entries * 8 {
        return None;
    }
    let mut entries = Vec::with_capacity(n_entries);
    for i in 0..n_entries {
        let at = 20 + i * 8;
        entries.push((
            u32::from_le_bytes(b[at..at + 4].try_into().unwrap()),
            u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()),
        ));
    }
    Some((tail, entries))
}

/// The full path of a segment, for callers that need it (tests, introspection).
pub fn seg_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(seg_name(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seg_names_parse_numerically() {
        assert_eq!(seg_name(0), "seg-00000000.fold");
        assert_eq!(parse_seg_name("seg-00000000.fold"), Some(0));
        assert_eq!(parse_seg_name("seg-00000042.fold"), Some(42));
        // past the %08 width, numeric parsing still works where lexicographic ordering would fail
        assert_eq!(parse_seg_name("seg-123456789.fold"), Some(123_456_789));
        assert_eq!(parse_seg_name("seg-.fold"), None);
        assert_eq!(parse_seg_name("seg-00x0.fold"), None);
        assert_eq!(parse_seg_name("nope.fold"), None);
    }

    #[test]
    fn header_roundtrips_and_validates() {
        let h = SegHeader { seg: 5, flags: 0, dict_id: [0u8; 32] };
        let b = h.encode();
        assert_eq!(b.len(), SEG_HDR_LEN as usize);
        assert_eq!(SegHeader::decode(&b, 5).unwrap(), h);
        assert!(!h.has_dict());

        // header/filename disagreement is caught
        assert!(SegHeader::decode(&b, 6).is_err());

        // bad magic is caught
        let mut bad = b;
        bad[0] = b'X';
        assert!(SegHeader::decode(&bad, 5).is_err());

        // A KNOWN flag is accepted and acted on — bit 0 means encrypted, which this build can
        // hold (and reports per block when it has no key).
        let mut flagged = b;
        flagged[12] = SEG_FLAG_ENCRYPTED as u8;
        let h = SegHeader::decode(&flagged, 5).unwrap();
        assert!(h.is_encrypted());
        // An UNKNOWN bit still refuses rather than negotiates.
        let mut future = b;
        future[13] = 0x04;
        assert!(SegHeader::decode(&future, 5).is_err(), "unknown flags must refuse");
    }
}
