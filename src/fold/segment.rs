//! Segment files: the append-only containers holding block frames.

use super::block::{self, BLOCK_HDR_LEN, BLOCK_XSUM_LEN};
use crate::readat::ReadAt;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Segment header length. The first block begins here, so a `Loc.block_off` below it is invalid.
pub const SEG_HDR_LEN: u64 = 48;

/// Identity assertion, not a version. A mismatch refuses — there is no negotiation anywhere.
pub const MAGIC: &[u8; 8] = b"TDBFLD01";

/// Segment flag: this segment's block payloads are ENCRYPTED.
///
/// RESERVED AND REFUSED. Nothing writes it and nothing reads it — the bit is claimed so that if
/// encryption is ever built, every reader shipped before it already refuses rather than serving
/// ciphertext as content. A reject-forward lever protects only the readers that already refuse,
/// so claiming the bit early costs four bytes of documentation and buys that guarantee; the
/// refusal names encryption so an operator is not sent hunting corruption that is not there.
pub const SEG_FLAG_ENCRYPTED: u32 = 1 << 0;

/// Default roll threshold. Bounded so a crash-time tail scan stays short and mmap granularity stays sane.
pub const SEG_MAX_DEFAULT: u32 = 1 << 30;

/// Largest segment length representable by the persisted u32 tail and block offsets. A segment is
/// therefore strictly smaller than 2^32 bytes.
pub const SEG_MAX_LIMIT: u64 = u32::MAX as u64;
/// Largest segment number representable by the exact eight-digit member namespace.
pub const MAX_SEGMENT_NUMBER: u32 = 99_999_999;

/// Block id and byte offset entries reconstructed for one segment.
pub type BlockDirectory = Vec<(u32, u32)>;

/// A sidecar's committed segment tail and its block directory.
pub type DirectorySidecar = (u32, BlockDirectory);

pub fn seg_name(n: u32) -> String {
    format!("seg-{n:08}.fold")
}

/// Parse the one exact eight-decimal-digit segment member spelling.
pub fn parse_seg_name(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("seg-")?.strip_suffix(".fold")?;
    if rest.len() != 8 || !rest.bytes().all(|b| b.is_ascii_digit()) {
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
        // The reject-forward lever: unknown means stop, not adapt. The encryption bit is named
        // in its own refusal because "this is encrypted and this build cannot read it" and
        // "unknown flags" send an operator to very different places.
        if flags & SEG_FLAG_ENCRYPTED != 0 {
            bail!(
                "segment {expect_seg} is ENCRYPTED and this build has no decryption path — \
                 refusing rather than serving ciphertext as content"
            );
        }
        if flags != 0 {
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
/// The complete header is synchronized under an exact numbered staging name before a no-replace
/// rename installs `seg-N.fold`. A short final-name segment is therefore never current protocol
/// debris and writer open refuses it without mutation.
pub fn create(dir: &Path, n: u32, dict_id: [u8; 32]) -> Result<File> {
    create_flagged(dir, n, dict_id, 0)
}

/// Internal construction seam. The current format writes only zero flags; the parameter remains
/// here so every write-side implementation checks that invariant at its byte-production boundary.
pub(crate) fn create_flagged(dir: &Path, n: u32, dict_id: [u8; 32], flags: u32) -> Result<File> {
    if n > MAX_SEGMENT_NUMBER {
        bail!("segment number {n} exceeds the current member namespace");
    }
    if flags != 0 {
        bail!("current fold segments require zero flags");
    }
    let path = dir.join(seg_name(n));
    let (staging, f) = crate::vfs::create_numbered_staging(&path, "creating")
        .with_context(|| format!("create segment staging beside {}", path.display()))?;
    let initialized = (|| -> Result<()> {
        crate::vfs::write_all_at(&f, &staging, &SegHeader { seg: n, flags, dict_id }.encode(), 0)?;
        crate::vfs::sync_file(&f, &staging)?;
        crate::vfs::rename_noreplace(&staging, &path)
            .with_context(|| format!("install segment {}", path.display()))?;
        Ok(())
    })();
    if initialized.is_err() {
        let _ = crate::vfs::unlink(&staging);
        initialized?;
    }
    fsync_dir(dir)?;
    Ok(f)
}

/// Exact crash-staging spelling for a segment birth.
pub(crate) fn is_birth_staging_name(name: &std::ffi::OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    let Some(marker) = bytes.windows(b".creating-".len()).position(|w| w == b".creating-") else {
        return false;
    };
    let (final_name, suffix) = bytes.split_at(marker);
    if parse_seg_name(std::str::from_utf8(final_name).unwrap_or("")).is_none() {
        return false;
    }
    let Some(numbers) = suffix.strip_prefix(b".creating-") else { return false };
    let Some(dash) = numbers.iter().position(|&byte| byte == b'-') else { return false };
    let (pid, serial_with_dash) = numbers.split_at(dash);
    let serial = &serial_with_dash[1..];
    !pid.is_empty()
        && !serial.is_empty()
        && pid.iter().all(u8::is_ascii_digit)
        && serial.iter().all(u8::is_ascii_digit)
        && std::str::from_utf8(pid).ok().and_then(|v| v.parse::<u32>().ok()).is_some()
        && std::str::from_utf8(serial).ok().and_then(|v| v.parse::<u64>().ok()).is_some()
}

/// Read-only handle — never opens for write, so a reader cannot damage a store it does not own.
pub fn open_read(dir: &Path, n: u32) -> Result<File> {
    let path = dir.join(seg_name(n));
    crate::vfs::open_read(&path)
        .with_context(|| format!("open segment {} read-only", path.display()))
}

pub fn open_rw(dir: &Path, n: u32) -> Result<File> {
    let path = dir.join(seg_name(n));
    crate::vfs::open_rw(&path).with_context(|| format!("open segment {}", path.display()))
}

/// Deallocate `len` bytes at `off` — the extents are freed and read back as zeros, and the file's
/// length is untouched, so every offset in it still means what it meant.
///
/// This is the one operation that destroys committed fold bytes in place. TurnDB implements it
/// through Linux hole punching and Windows sparse-range zeroing; where the operating system or
/// filesystem declines, the error surfaces and the caller falls back to a re-fold, which reclaims
/// the same space by rewriting rather than by punching.
///
/// **Only a block's PAYLOAD is ever punched, never its header.** The frame chain is walked by
/// reading a header and stepping over `stored` bytes, so a punched header would end the chain and
/// silently orphan every block after it in the segment. Sixteen surviving header bytes carry no
/// content — they carry the length that keeps the chain walkable, and the `block_id` that lets a
/// scan report the erasure by name.
pub fn punch(f: &File, path: &Path, off: u64, len: u64) -> Result<()> {
    // Through the vfs seam, and `path` exists only to feed it: destroying committed bytes in
    // place is precisely the kind of mutation the crash simulator must be able to replay.
    if let Err(e) = crate::vfs::punch_hole(f, path, off, len) {
        bail!("punching {len} bytes at {off} failed ({e}) — this filesystem may not support hole punching; re-fold instead");
    }
    crate::vfs::sync_file(f, path)?;
    Ok(())
}

/// Walk the block chain to the last **complete, checksum-valid** block. Returns the offset just past
/// it plus every `(block_id, offset)` seen, which is how the block directory is rebuilt at open.
///
/// This is how the fold finds its own last good byte with no external length authority. It never
/// decompresses: read 12 bytes, hash `12 + stored`, advance. A structurally invalid frame or
/// checksum failure ends good data; an underlying positioned-read failure propagates. A frame the
/// manifest declares PUNCHED is stepped over by the `punched` parameter of the `_with_limits`
/// variants. This convenience wrapper has no manifest in hand and passes no declaration.
pub fn scan_tail(f: &dyn ReadAt, file_len: u64, has_dict: bool) -> Result<(u64, Vec<(u32, u32)>)> {
    scan_tail_with_limits(f, file_len, has_dict, &[], crate::read_limits::ReadLimits::default())
}

/// [`scan_tail`] with explicit admission before its reusable frame buffer grows, and with the
/// manifest's declared-punched block ranges (inclusive `[lo, hi]`) — empty when the caller holds
/// no manifest.
pub fn scan_tail_with_limits(
    f: &dyn ReadAt,
    file_len: u64,
    has_dict: bool,
    punched: &[(u32, u32)],
    read_limits: crate::read_limits::ReadLimits,
) -> Result<(u64, Vec<(u32, u32)>)> {
    if file_len > SEG_MAX_LIMIT {
        bail!("segment is {file_len} bytes, over the {SEG_MAX_LIMIT} format bound");
    }
    scan_tail_controlled_with_limits(
        f,
        file_len,
        has_dict,
        punched,
        &crate::control::OperationControl::default(),
        "fold scan",
        read_limits,
    )
}

/// [`scan_tail`] with cooperative checks between complete frames. No manifest in hand, so no
/// punched declaration reaches the walk.
pub fn scan_tail_controlled(
    f: &dyn ReadAt,
    file_len: u64,
    has_dict: bool,
    control: &crate::control::OperationControl,
    operation: &'static str,
) -> Result<(u64, Vec<(u32, u32)>)> {
    scan_tail_controlled_with_limits(
        f,
        file_len,
        has_dict,
        &[],
        control,
        operation,
        crate::read_limits::ReadLimits::default(),
    )
}

/// Controlled tail scan with explicit frame-byte and block-count admission. `punched` carries the
/// manifest's declared-erased block-id ranges so the walk can step over a punched frame whatever
/// residue a crash left in its payload.
pub fn scan_tail_controlled_with_limits(
    f: &dyn ReadAt,
    file_len: u64,
    has_dict: bool,
    punched: &[(u32, u32)],
    control: &crate::control::OperationControl,
    operation: &'static str,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<(u64, Vec<(u32, u32)>)> {
    let read_limits = read_limits.validate()?;
    if file_len > SEG_MAX_LIMIT {
        bail!("segment is {file_len} bytes, over the {SEG_MAX_LIMIT} format bound");
    }
    let mut off = SEG_HDR_LEN;
    let mut hdr = [0u8; BLOCK_HDR_LEN];
    let mut payload = Vec::new();
    // Blocks land in COMPLETION order, so position no longer implies identity — the directory is
    // rebuilt from the ids the frames carry.
    let mut dir: Vec<(u32, u32)> = Vec::new();
    loop {
        control.check(operation)?;
        if file_len.saturating_sub(off) < block::BLOCK_OVERHEAD as u64 {
            break; // cannot hold even an empty block
        }
        f.read_exact_at(&mut hdr, off)
            .with_context(|| format!("read fold block header at byte {off}"))?;
        let parsed = block::parse_hdr(&hdr, has_dict);
        let raw = u32::from_le_bytes(hdr[2..6].try_into().unwrap());
        let stored = u32::from_le_bytes(hdr[6..10].try_into().unwrap());
        let end = off
            .checked_add(BLOCK_HDR_LEN as u64)
            .and_then(|at| at.checked_add(u64::from(stored)))
            .and_then(|at| at.checked_add(BLOCK_XSUM_LEN as u64))
            .ok_or_else(|| anyhow::anyhow!("fold block frame end overflows at byte {off}"))?;
        if let Err(error) = parsed.as_ref() {
            if end > file_len {
                break; // an invalid-looking header whose advertised final frame never landed
            }
            read_limits.admit("unrecognized fold block", u64::from(stored), u64::from(raw))?;
            let span = BLOCK_HDR_LEN
                .checked_add(stored as usize)
                .and_then(|len| len.checked_add(BLOCK_XSUM_LEN))
                .ok_or_else(|| anyhow::anyhow!("fold block allocation length overflows"))?;
            payload.resize(span, 0);
            f.read_exact_at(&mut payload[..span], off)
                .with_context(|| format!("read unrecognized fold block frame at byte {off}"))?;
            let checksum_at = BLOCK_HDR_LEN + stored as usize;
            if block::xsum(&payload[..checksum_at])
                == payload[checksum_at..checksum_at + BLOCK_XSUM_LEN]
            {
                return Err(anyhow::anyhow!("{error}"))
                    .with_context(|| format!("checksumming fold frame at byte {off} has an invalid current-format header"));
            }
            if end < file_len {
                bail!(
                    "invalid fold frame at byte {off} has {} later bytes; refusing instead of truncating",
                    file_len - end
                );
            }
            break;
        }
        let h = parsed.expect("checked above");
        // Unlike checksum damage, a valid header outside runtime policy is not a crash-tail
        // boundary. Surface the typed refusal so directory-fold writer open never truncates a valid
        // block merely because this process was opened with a smaller budget.
        read_limits.admit(
            format!("fold block {}", h.block_id),
            u64::from(h.stored),
            u64::from(h.raw),
        )?;
        if end > file_len {
            break; // the promised payload/checksum never fully reached disk
        }
        // verify the whole block frame's bytes
        let span = block::frame_span_usize(h.stored)?;
        payload.resize(span, 0);
        f.read_exact_at(&mut payload[..span], off)
            .with_context(|| format!("read fold block frame at byte {off}"))?;
        control.check(operation)?;
        if block::verify_frame_bytes(&payload[..span], has_dict).is_err() {
            // A PUNCHED block: header intact (punch never touches it — that is what keeps the
            // chain walkable), payload deallocated. The deallocation is volatile until its fsync,
            // so a crash can leave the payload in any of THREE residues: fully readable (the
            // fallocate never landed — the frame still verifies and never reaches this branch),
            // fully zeroed (it completed), or PARTIALLY zeroed (fallocate is per-extent, so power
            // loss mid-punch genuinely half-zeroes a range). The manifest's punched declaration is
            // the erasure AUTHORITY — it is committed before any byte is destroyed — so a declared
            // frame is stepped over whatever its payload holds, and its location is retained so a
            // later read reports ERASED by name instead of resolving into damage.
            //
            let declared = punched.iter().any(|&(lo, hi)| (lo..=hi).contains(&h.block_id));
            if declared {
                read_limits.admit_fold_blocks(dir.len() as u64 + 1)?;
                read_limits.admit_fold_blocks(u64::from(h.block_id) + 1)?;
                dir.push((h.block_id, u32::try_from(off).context("block offset exceeds u32")?));
                off = end;
                continue;
            }
            if end < file_len {
                bail!(
                    "fold frame at byte {off} fails its checksum with {} later bytes present",
                    file_len - end
                );
            }
            break;
        }
        read_limits.admit_fold_blocks(dir.len() as u64 + 1)?;
        read_limits.admit_fold_blocks(u64::from(h.block_id) + 1)?;
        dir.push((h.block_id, u32::try_from(off).context("block offset exceeds u32")?));
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
// The directory sidecar — advisory, derived, and the answer to O(segment) opens
// ---------------------------------------------------------------------------------------------

/// Sidecar magic. A `seg-NNNNNNNN.dir` member beside a segment carries what `scan_tail` would
/// recompute: the block ids and offsets, and the scan end. The container writer stages one for
/// every committed segment so a ranged cold open never scans content payload.
pub const DIR_MAGIC: &[u8; 8] = b"TDBSDR01";

/// ```text
/// offset  size  field
///      0     8  MAGIC = "TDBSDR01"
///      8     4  seg          must match the filename AND the segment beside it
///     12     4  tail         scan end; for a sealed segment this IS the file length
///     16     4  n_entries
///     20   n*8  (block_id u32, offset u32) per block
///  20+n*8    4  crc32 over everything before it
/// ```
pub fn dir_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(format!("seg-{n:08}.dir"))
}

/// Parse the exact temporary-sidecar spelling emitted by [`write_dir_sidecar`].
pub(crate) fn parse_dir_tmp_name(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("seg-")?.strip_suffix(".dir.tmp")?;
    if rest.len() != 8 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u32>().ok()
}

/// Write a directory-layout sidecar for a sealed segment. ADVISORY, so tmp + rename but no fsync
/// anywhere: a sidecar lost to a crash costs one rescan at the next open, and a torn one fails its
/// checksum and costs the same. Nothing durable depends on it.
pub fn write_dir_sidecar(dir: &Path, n: u32, tail: u32, entries: &[(u32, u32)]) -> Result<()> {
    let b = encode_dir_sidecar(n, tail, entries);
    let tmp = dir.join(format!("seg-{n:08}.dir.tmp"));
    crate::vfs::write_file(&tmp, &b)?;
    crate::vfs::rename(&tmp, &dir_path(dir, n))?;
    Ok(())
}

/// The sidecar bytes alone — one encoding whether it lands as a file beside a segment or as a
/// member beside a segment member.
pub fn encode_dir_sidecar(n: u32, tail: u32, entries: &[(u32, u32)]) -> Vec<u8> {
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
    b
}

/// The sidecar's entries, or `None` — absent, damaged, or not describing this file's bytes. The
/// caller rescans on `None`; it must never trust a sidecar this refused.
///
/// `tail == file_len` is the staleness gate, not a nicety: directory-fold crash-tail repair can truncate a once-sealed
/// segment back into being the active one, and its leftover sidecar then describes blocks past
/// the committed tail. A sealed segment ends exactly at its last block, so any length mismatch
/// means the sidecar and the segment parted ways.
pub fn read_dir_sidecar(dir: &Path, n: u32, file_len: u64) -> Option<DirectorySidecar> {
    read_dir_sidecar_with_limits(dir, n, file_len, crate::read_limits::ReadLimits::default())
        .ok()
        .flatten()
}

/// Read and parse advisory directory metadata with explicit byte and object admission.
pub fn read_dir_sidecar_with_limits(
    dir: &Path,
    n: u32,
    file_len: u64,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<Option<DirectorySidecar>> {
    let Some(bytes) = read_dir_sidecar_bytes_with_limits(dir, n, file_len, read_limits)? else {
        return Ok(None);
    };
    parse_dir_sidecar_with_limits(&bytes, n, file_len, read_limits)
}

/// Largest structurally possible sidecar for a segment extent. Every indexed block consumes at
/// least one frame overhead in the segment, so advisory metadata cannot legitimately outgrow this.
pub fn max_dir_sidecar_bytes(file_len: u64) -> u64 {
    let frames = file_len.saturating_sub(SEG_HDR_LEN).div_ceil(super::block::BLOCK_OVERHEAD as u64);
    24u64.saturating_add(frames.saturating_mul(8))
}

/// Read advisory bytes only after their filesystem length fits what the segment could describe.
/// `None` is intentionally the only failure: callers rescan the authoritative segment.
pub fn read_dir_sidecar_bytes(dir: &Path, n: u32, file_len: u64) -> Option<Vec<u8>> {
    read_dir_sidecar_bytes_with_limits(dir, n, file_len, crate::read_limits::ReadLimits::default())
        .ok()
        .flatten()
}

/// Read advisory sidecar bytes after both structural and runtime byte admission.
pub fn read_dir_sidecar_bytes_with_limits(
    dir: &Path,
    n: u32,
    file_len: u64,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<Option<Vec<u8>>> {
    let file = match crate::vfs::open_read(&dir_path(dir, n)) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let max = max_dir_sidecar_bytes(file_len);
    let len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return Ok(None),
    };
    if len > max {
        return Ok(None);
    }
    read_limits.admit_stored(format!("fold segment {n} directory sidecar"), len)?;
    let capacity = match usize::try_from(len) {
        Ok(capacity) => capacity,
        Err(_) => return Ok(None),
    };
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(capacity).is_err()
        || file.take(max.saturating_add(1)).read_to_end(&mut bytes).is_err()
    {
        return Ok(None);
    }
    Ok((bytes.len() as u64 <= max).then_some(bytes))
}

/// [`read_dir_sidecar`]'s validation core, over bytes from any positioned byte source.
pub fn parse_dir_sidecar(b: &[u8], n: u32, file_len: u64) -> Option<DirectorySidecar> {
    parse_dir_sidecar_with_limits(b, n, file_len, crate::read_limits::ReadLimits::default())
        .ok()
        .flatten()
}

/// Parse advisory directory bytes with object admission before allocating the entry vector.
pub fn parse_dir_sidecar_with_limits(
    b: &[u8],
    n: u32,
    file_len: u64,
    read_limits: crate::read_limits::ReadLimits,
) -> Result<Option<DirectorySidecar>> {
    if b.len() < 24 || &b[0..8] != DIR_MAGIC {
        return Ok(None);
    }
    let crc = u32::from_le_bytes(b[b.len() - 4..].try_into().unwrap());
    if crc32fast::hash(&b[..b.len() - 4]) != crc {
        return Ok(None);
    }
    let seg = u32::from_le_bytes(b[8..12].try_into().unwrap());
    let tail = u32::from_le_bytes(b[12..16].try_into().unwrap());
    let n_entries = u32::from_le_bytes(b[16..20].try_into().unwrap()) as usize;
    let expected_len = n_entries.checked_mul(8).and_then(|bytes| 24usize.checked_add(bytes));
    if seg != n || tail as u64 != file_len || expected_len != Some(b.len()) {
        return Ok(None);
    }
    read_limits.admit_fold_blocks(n_entries as u64)?;
    let mut entries = Vec::with_capacity(n_entries);
    for i in 0..n_entries {
        let at = 20 + i * 8;
        entries.push((
            u32::from_le_bytes(b[at..at + 4].try_into().unwrap()),
            u32::from_le_bytes(b[at + 4..at + 8].try_into().unwrap()),
        ));
    }
    Ok(Some((tail, entries)))
}

/// Prove that parsed advisory entries describe the authoritative frame-header chain exactly.
///
/// A sidecar checksum authenticates only the sidecar's own bytes. Before its entries can replace a
/// segment scan, each offset and block id must agree with the header at the next contiguous frame
/// boundary, and the final frame must end exactly at `file_len`. Header/read damage returns `false`
/// so the caller falls back to the authoritative scan. A genuine runtime-admission refusal remains
/// an error: silently rescanning must not turn an explicit resource policy into a crash-tail guess.
pub fn validate_dir_sidecar_entries(
    segment: &dyn ReadAt,
    file_len: u64,
    has_dict: bool,
    entries: &[(u32, u32)],
    read_limits: crate::read_limits::ReadLimits,
) -> Result<bool> {
    let mut expected = SEG_HDR_LEN;
    let mut header = [0u8; BLOCK_HDR_LEN];
    for &(block_id, offset) in entries {
        if u64::from(offset) != expected || segment.read_exact_at(&mut header, expected).is_err() {
            return Ok(false);
        }
        let parsed = match block::parse_hdr(&header, has_dict) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(false),
        };
        if parsed.block_id != block_id {
            return Ok(false);
        }
        read_limits.admit(
            format!("fold block {}", parsed.block_id),
            u64::from(parsed.stored),
            u64::from(parsed.raw),
        )?;
        expected = match expected.checked_add(parsed.frame_len()) {
            Some(end) if end <= file_len => end,
            _ => return Ok(false),
        };
    }
    Ok(expected == file_len)
}

/// The full path of a segment, for callers that need it (tests, introspection).
pub fn seg_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(seg_name(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoRead;

    impl ReadAt for NoRead {
        fn read_exact_at(&self, _buf: &mut [u8], _off: u64) -> std::io::Result<()> {
            panic!("an over-bound segment must be refused before any read")
        }

        fn len(&self) -> std::io::Result<u64> {
            Ok(SEG_MAX_LIMIT + 1)
        }
    }

    #[test]
    fn seg_names_parse_numerically() {
        assert_eq!(seg_name(0), "seg-00000000.fold");
        assert_eq!(parse_seg_name("seg-00000000.fold"), Some(0));
        assert_eq!(parse_seg_name("seg-00000042.fold"), Some(42));
        // past the %08 width, numeric parsing still works where lexicographic ordering would fail
        assert_eq!(parse_seg_name("seg-123456789.fold"), None);
        assert_eq!(parse_seg_name("seg-0000000.fold"), None);
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
        let mut discarded = b;
        discarded[..8].copy_from_slice(b"TURNFOLD");
        assert!(
            SegHeader::decode(&discarded, 5).is_err(),
            "the discarded fold magic must not open"
        );

        // Both the reserved encryption bit and any unknown bit refuse rather than negotiate.
        let mut flagged = b;
        flagged[12] = SEG_FLAG_ENCRYPTED as u8;
        assert!(SegHeader::decode(&flagged, 5).is_err(), "the reserved encryption bit must refuse");
        let mut future = b;
        future[13] = 0x04;
        assert!(SegHeader::decode(&future, 5).is_err(), "unknown flags must refuse");
    }

    #[test]
    fn advisory_sidecar_size_is_bounded_by_the_segment_it_describes() {
        let dir = std::env::temp_dir().join(format!("turndb-sidecar-limit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_len = SEG_HDR_LEN + crate::fold::block::BLOCK_OVERHEAD as u64;
        let max = max_dir_sidecar_bytes(file_len);
        std::fs::File::create(dir_path(&dir, 0)).unwrap().set_len(max + 1).unwrap();
        let got = read_dir_sidecar_bytes(&dir, 0, file_len);
        std::fs::remove_dir_all(dir).ok();
        assert!(got.is_none(), "an impossible sparse sidecar must be ignored before allocation");
    }

    #[test]
    fn every_tail_scan_entry_point_enforces_the_u32_segment_domain() {
        assert!(scan_tail_controlled(
            &NoRead,
            SEG_MAX_LIMIT + 1,
            false,
            &crate::control::OperationControl::default(),
            "test scan",
        )
        .is_err());
    }
}
