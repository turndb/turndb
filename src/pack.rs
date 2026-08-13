//! The pack: a store in one file. See FORMAT.md § The pack — this module is its implementation,
//! and the format document is normative where they disagree.
//!
//! Writing is an EXPORT of the committed snapshot: `MANIFEST`, every part it names, the live fold
//! generation's segments plus their advisory companions. Reading hands each inner file back as a
//! bounded [`Slice`] over the pack — which is all the store's read paths need, because every one
//! of them already goes through [`ReadAt`].

use crate::readat::{ReadAt, Slice};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MAGIC: &[u8; 8] = b"TURNPACK";
pub const FOOTER_LEN: u64 = 40;
/// The pack layout this build writes, and the highest it will read. Reject-forward, like the part.
pub const PACK_VERSION: u8 = 1;
/// Default compressed and decompressed TOC admission ceiling. A normal TOC is tiny compared with
/// the files it indexes; bounding both sides prevents a sparse hostile pack from requesting a
/// multi-gigabyte allocation before any entry can be validated.
pub const DEFAULT_MAX_TOC_BYTES: u64 = 64 << 20;
/// Default number of files indexed by one pack. Configurable because this is embedding policy, not
/// a format limit; 100,000 is already far beyond a maintained store's ordinary part/fold count.
pub const DEFAULT_MAX_PACK_FILES: usize = 100_000;
/// Default UTF-8 bytes accepted in one pack entry name.
pub const DEFAULT_MAX_PACK_NAME_BYTES: usize = 16 << 10;
/// Whether this target has an atomic rename primitive that refuses replacement.
pub const ATOMIC_RESTORE: bool =
    cfg!(any(target_os = "linux", target_os = "macos", target_os = "ios"));

/// A safe backup/restore refusal that callers can classify without parsing prose.
#[derive(Debug)]
pub enum BackupError {
    DestinationExists(PathBuf),
    InvalidBackup { path: PathBuf, reason: String },
    Unsupported(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::DestinationExists(path) => {
                write!(f, "destination {} already exists; refusing to replace it", path.display())
            }
            BackupError::InvalidBackup { path, reason } => {
                write!(f, "backup {} is invalid: {reason}", path.display())
            }
            BackupError::Unsupported(reason) => {
                write!(f, "backup operation is unsupported: {reason}")
            }
        }
    }
}

impl std::error::Error for BackupError {}

/// One inner file: `(offset, length, crc32 of its bytes)`.
type Entry = (u64, u64, u32);

/// Resource admission policy for parsing pack metadata.
///
/// These limits do not alter the format and may be raised by an embedder handling an unusually large
/// artifact. They bound metadata allocation and iteration only; member bytes remain range-backed and
/// verification/restoration stream them in fixed-size chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackLimits {
    pub max_toc_stored_bytes: u64,
    pub max_toc_raw_bytes: u64,
    pub max_files: usize,
    pub max_name_bytes: usize,
}

impl Default for PackLimits {
    fn default() -> Self {
        PackLimits {
            max_toc_stored_bytes: DEFAULT_MAX_TOC_BYTES,
            max_toc_raw_bytes: DEFAULT_MAX_TOC_BYTES,
            max_files: DEFAULT_MAX_PACK_FILES,
            max_name_bytes: DEFAULT_MAX_PACK_NAME_BYTES,
        }
    }
}

impl PackLimits {
    fn validate(self) -> Result<PackLimits> {
        if self.max_toc_stored_bytes == 0
            || self.max_toc_raw_bytes == 0
            || self.max_files == 0
            || self.max_name_bytes == 0
        {
            bail!("pack parsing limits must all be greater than zero");
        }
        Ok(self)
    }
}

/// An open pack: the TOC, and shared access to the underlying bytes.
pub struct Pack {
    f: Arc<File>,
    toc: BTreeMap<String, Entry>,
}

impl Pack {
    pub fn open(path: &Path) -> Result<Pack> {
        Self::open_with_limits_and_control(
            path,
            PackLimits::default(),
            &crate::control::OperationControl::default(),
        )
    }

    /// Open with explicit resource admission for pack metadata.
    pub fn open_with_limits(path: &Path, limits: PackLimits) -> Result<Pack> {
        Self::open_with_limits_and_control(
            path,
            limits,
            &crate::control::OperationControl::default(),
        )
    }

    /// [`Pack::open`] with cooperative checks while validating and parsing the pack TOC.
    pub fn open_with_control(
        path: &Path,
        control: &crate::control::OperationControl,
    ) -> Result<Pack> {
        Self::open_with_limits_and_control(path, PackLimits::default(), control)
    }

    /// Open with explicit metadata limits and cooperative cancellation.
    pub fn open_with_limits_and_control(
        path: &Path,
        limits: PackLimits,
        control: &crate::control::OperationControl,
    ) -> Result<Pack> {
        let limits = limits.validate()?;
        control.check("backup validation")?;
        let f = File::open(path).with_context(|| format!("open pack {}", path.display()))?;
        let len = f.metadata()?.len();
        if len < FOOTER_LEN {
            bail!("pack of {len} bytes is too short to hold a footer");
        }
        let mut foot = [0u8; FOOTER_LEN as usize];
        crate::sys::read_exact_at(&f, &mut foot, len - FOOTER_LEN)?;
        if &foot[0..8] != MAGIC {
            bail!("not a turndb pack (bad magic) — or the footer never landed");
        }
        let want = blake3::hash(&foot[..FOOTER_LEN as usize - 4]);
        if want.as_bytes()[0..4] != foot[FOOTER_LEN as usize - 4..] {
            bail!("pack footer checksum mismatch — torn write");
        }
        let toc_off = u64::from_le_bytes(foot[8..16].try_into().unwrap());
        let toc_stored = u32::from_le_bytes(foot[16..20].try_into().unwrap());
        let toc_raw = u32::from_le_bytes(foot[20..24].try_into().unwrap());
        let n_files = u32::from_le_bytes(foot[24..28].try_into().unwrap()) as usize;
        let toc_codec = foot[28];
        let version = foot[29];
        if version > PACK_VERSION {
            bail!(
                "pack is format version {version}; this build reads up to {PACK_VERSION} — \
                 refusing rather than guessing at its layout"
            );
        }
        if foot[30..32] != [0u8; 2] {
            bail!("pack footer reserved bytes are non-zero — refusing rather than guessing");
        }
        let toc_xsum = u32::from_le_bytes(foot[32..36].try_into().unwrap());
        if u64::from(toc_stored) > limits.max_toc_stored_bytes {
            bail!(
                "pack TOC stores {toc_stored} bytes, exceeding the configured {}-byte limit",
                limits.max_toc_stored_bytes
            );
        }
        if u64::from(toc_raw) > limits.max_toc_raw_bytes {
            bail!(
                "pack TOC expands to {toc_raw} bytes, exceeding the configured {}-byte limit",
                limits.max_toc_raw_bytes
            );
        }
        if n_files > limits.max_files {
            bail!(
                "pack footer declares {n_files} files, exceeding the configured {}-file limit",
                limits.max_files
            );
        }
        if toc_off.saturating_add(toc_stored as u64) > len - FOOTER_LEN {
            bail!("pack TOC runs past where the footer says the files end");
        }
        let toc_stored = usize::try_from(toc_stored)
            .context("pack stored TOC length does not fit this platform")?;
        let mut tbuf = Vec::new();
        tbuf.try_reserve_exact(toc_stored).context("reserve pack TOC buffer")?;
        tbuf.resize(toc_stored, 0);
        control.check("backup validation")?;
        crate::sys::read_exact_at(&f, &mut tbuf, toc_off)?;
        if crc32fast::hash(&tbuf) != toc_xsum {
            bail!("pack TOC fails its checksum");
        }
        let toc_bytes = crate::fold::codec::decode(toc_codec, &tbuf, toc_raw, None)?;

        use crate::part::idcol::get_varint;
        let mut at = 0usize;
        let n = usize::try_from(get_varint(&toc_bytes, &mut at)?)
            .context("pack TOC file count does not fit this platform")?;
        if n != n_files {
            bail!("pack TOC holds {n} entries but the footer says {n_files}");
        }
        let mut toc: BTreeMap<String, Entry> = BTreeMap::new();
        for _ in 0..n {
            control.check("backup validation")?;
            let nl = usize::try_from(get_varint(&toc_bytes, &mut at)?)
                .context("pack TOC name length does not fit this platform")?;
            if nl > limits.max_name_bytes {
                bail!(
                    "pack TOC entry name is {nl} bytes, exceeding the configured {}-byte limit",
                    limits.max_name_bytes
                );
            }
            if nl > toc_bytes.len() - at {
                bail!("pack TOC entry name runs past the end of the TOC");
            }
            let name = String::from_utf8(toc_bytes[at..at + nl].to_vec())?;
            at += nl;
            let off = get_varint(&toc_bytes, &mut at)?;
            let flen = get_varint(&toc_bytes, &mut at)?;
            if 4 > toc_bytes.len() - at {
                bail!("pack TOC entry {name} is truncated before its checksum");
            }
            let xsum = u32::from_le_bytes(toc_bytes[at..at + 4].try_into().unwrap());
            at += 4;
            // Files live before the TOC — the tighter bound, and it rules out an entry claiming
            // to overlap the TOC or footer.
            if off.saturating_add(flen) > toc_off {
                bail!("pack TOC entry {name} runs past the end of the file region");
            }
            if toc.insert(name.clone(), (off, flen, xsum)).is_some() {
                bail!("pack TOC names {name} twice");
            }
        }
        if at != toc_bytes.len() {
            bail!("pack TOC has {} trailing bytes after its last entry", toc_bytes.len() - at);
        }
        Ok(Pack { f: Arc::new(f), toc })
    }

    /// Every inner file, in name order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.toc.keys().map(|s| s.as_str())
    }

    /// The inner file as a bounded extent — the shape every read path wants.
    pub fn file(&self, name: &str) -> Option<Slice<Arc<File>>> {
        let &(off, len, _) = self.toc.get(name)?;
        Some(Slice::new(self.f.clone(), off, len))
    }

    /// The inner file, loaded whole — for the small ones (manifest, sidecars, dictionaries).
    pub fn read_file(&self, name: &str) -> Result<Vec<u8>> {
        self.read_file_bounded(name, u64::MAX)
    }

    /// Load an inner file only when its declared extent fits an explicit allocation ceiling.
    pub fn read_file_bounded(&self, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let &(off, len, _) =
            self.toc.get(name).ok_or_else(|| anyhow::anyhow!("pack has no file {name}"))?;
        if len > max_bytes {
            bail!(
                "pack file {name} is {len} bytes, exceeding the configured {max_bytes}-byte inline-read limit"
            );
        }
        let len = usize::try_from(len).context("pack member length does not fit this platform")?;
        let mut b = Vec::new();
        b.try_reserve_exact(len).context("reserve pack member buffer")?;
        b.resize(len, 0);
        ReadAt::read_exact_at(&self.f, &mut b, off)?;
        Ok(b)
    }

    /// Check every inner file against its recorded checksum. NOT done on the read path, by the
    /// same policy as part sections: the inner formats carry their own integrity, and hashing the
    /// whole pack per read would tax every query for a scrub's job.
    pub fn verify(&self) -> Result<usize> {
        self.verify_with_control(&crate::control::OperationControl::default())
    }

    /// [`Pack::verify`] with cooperative checks between files and bounded read chunks.
    pub fn verify_with_control(&self, control: &crate::control::OperationControl) -> Result<usize> {
        let mut checked = 0usize;
        for (name, &(off, len, xsum)) in &self.toc {
            control.check("backup verification")?;
            let mut remaining = len;
            let mut at = off;
            let mut h = crc32fast::Hasher::new();
            let mut buf = vec![0u8; (1 << 20).min(len.max(1)) as usize];
            while remaining > 0 {
                control.check("backup verification")?;
                let take = buf.len().min(remaining as usize);
                ReadAt::read_exact_at(&self.f, &mut buf[..take], at)?;
                h.update(&buf[..take]);
                at += take as u64;
                remaining -= take as u64;
            }
            if h.finalize() != xsum {
                bail!("pack file {name} fails its checksum");
            }
            checked += 1;
        }
        Ok(checked)
    }
}

/// What a pack write did.
#[derive(Clone, Copy, Debug)]
pub struct PackStats {
    pub files: usize,
    pub bytes: u64,
}

/// What a safe online writer backup did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackupStats {
    pub files: usize,
    pub bytes: u64,
    pub commit: u64,
}

/// What a safe restore published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestoreStats {
    pub files: usize,
    pub bytes: u64,
    pub commit: u64,
}

pub(crate) fn ensure_destination_available(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(BackupError::DestinationExists(path.to_path_buf()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect destination {}", path.display())),
    }
}
