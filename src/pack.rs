//! The pack: a store in one file. See FORMAT.md § The pack — this module is its implementation,
//! and the format document is normative where they disagree.
//!
//! Writing is an EXPORT of the committed snapshot: `MANIFEST`, every part it names, the live fold
//! generation's segments plus their advisory companions. Reading hands each inner file back as a
//! bounded [`Slice`] over the pack — which is all the store's read paths need, because every one
//! of them already goes through [`ReadAt`].

use crate::readat::{ReadAt, Slice};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

/// Pack the committed snapshot of the store at `dir` into one file at `out`.
///
/// Takes the store's writer role, recovers and settles its WAL, and then writes the exact published
/// cut. A live writer therefore causes ordinary writer contention instead of a racing export. The
/// completed temporary artifact is fully verified, then hard-linked under the final name. That
/// publication is atomic and refuses an existing destination.
pub fn write(dir: &Path, out: &Path) -> Result<PackStats> {
    write_with_control(dir, out, &crate::control::OperationControl::default())
}

/// [`write()`] with cooperative cancellation before atomic artifact publication.
pub fn write_with_control(
    dir: &Path,
    out: &Path,
    control: &crate::control::OperationControl,
) -> Result<PackStats> {
    control.check("backup")?;
    ensure_destination_available(out)?;
    // Taking the writer role makes the public directory-based operation safe alongside other
    // processes and also replays and includes a durable WAL instead of refusing or omitting it.
    let mut store = crate::store::Store::open(dir, crate::fold::FoldCfg::default())?;
    let stats = store.backup_with_control(out, control)?;
    Ok(PackStats { files: stats.files, bytes: stats.bytes })
}

/// Write a snapshot while the caller owns the store's writer role and has settled its WAL.
pub(crate) fn write_committed_with_control(
    dir: &Path,
    out: &Path,
    control: &crate::control::OperationControl,
) -> Result<PackStats> {
    control.check("backup")?;
    let canonical_root = std::fs::canonicalize(dir)
        .with_context(|| format!("resolve store root {} for packing", dir.display()))?;
    let manifest_path = local_pack_source(dir, &canonical_root, "MANIFEST")?;
    let manifest_bytes = crate::store::read_manifest_file(&manifest_path)
        .with_context(|| format!("read MANIFEST at {} — is this a store?", dir.display()))?;
    let manifest = crate::store::manifest_from_bytes(&manifest_bytes)?;
    if let Ok(m) = std::fs::metadata(dir.join("WAL")) {
        if m.len() > 0 {
            bail!(
                "the WAL at {} holds uncommitted records — flush before packing, or the pack \
                 would silently omit acked data",
                dir.display()
            );
        }
    }

    // The snapshot's files, store-relative. Sorted by name at TOC time; gathered here in
    // manifest-then-fold order for a sequential read pattern.
    let mut names: Vec<String> = vec!["MANIFEST".into()];
    for p in &manifest.parts {
        control.check("backup packing")?;
        names.push(p.file.clone());
    }
    let fold_rel = if manifest.fold_gen == 0 {
        "fold".to_string()
    } else {
        format!("fold-{:04}", manifest.fold_gen)
    };
    let fold_dir = dir.join(&fold_rel);
    let mut fold_files: Vec<String> = Vec::new();
    for e in std::fs::read_dir(&fold_dir)
        .with_context(|| format!("read fold dir {}", fold_dir.display()))?
        .flatten()
    {
        control.check("backup packing")?;
        let n = e.file_name().to_string_lossy().to_string();
        let keep = n.ends_with(".fold")
            || n.ends_with(".dir")
            || (n.starts_with("zdict-") && n.ends_with(".zd"));
        if keep {
            fold_files.push(format!("{fold_rel}/{n}"));
        }
    }
    fold_files.sort();
    names.extend(fold_files);

    ensure_destination_available(out)?;
    let (tmp, f) = create_temp_file(out, "pack")?;
    let mut cleanup = Cleanup::file(tmp.clone());
    let mut off = 0u64;
    let mut entries: Vec<(String, Entry)> = Vec::with_capacity(names.len());
    let mut buf = vec![0u8; 1 << 20];
    for name in &names {
        control.check("backup packing")?;
        let source_path = local_pack_source(dir, &canonical_root, name)?;
        let mut src =
            File::open(&source_path).with_context(|| format!("open {name} for packing"))?;
        let start = off;
        let mut h = crc32fast::Hasher::new();
        loop {
            control.check("backup packing")?;
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crate::vfs::write_all_at(&f, &tmp, &buf[..n], off)?;
            h.update(&buf[..n]);
            off += n as u64;
        }
        entries.push((name.clone(), (start, off - start, h.finalize())));
    }

    use crate::part::idcol::put_varint;
    control.check("backup packing")?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut toc = Vec::new();
    put_varint(&mut toc, entries.len() as u64);
    for (name, (o, l, x)) in &entries {
        control.check("backup packing")?;
        put_varint(&mut toc, name.len() as u64);
        toc.extend_from_slice(name.as_bytes());
        put_varint(&mut toc, *o);
        put_varint(&mut toc, *l);
        toc.extend_from_slice(&x.to_le_bytes());
    }
    let (toc_codec, toc_payload) = crate::fold::codec::encode(&toc, None, 3)?;
    let toc_off = off;
    crate::vfs::write_all_at(&f, &tmp, &toc_payload, off)?;
    off += toc_payload.len() as u64;

    let mut foot = Vec::with_capacity(FOOTER_LEN as usize);
    foot.extend_from_slice(MAGIC);
    foot.extend_from_slice(&toc_off.to_le_bytes());
    foot.extend_from_slice(&(toc_payload.len() as u32).to_le_bytes());
    foot.extend_from_slice(&(toc.len() as u32).to_le_bytes());
    foot.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    foot.push(toc_codec);
    foot.push(PACK_VERSION);
    foot.extend_from_slice(&[0u8; 2]);
    foot.extend_from_slice(&crc32fast::hash(&toc_payload).to_le_bytes());
    let x = blake3::hash(&foot);
    foot.extend_from_slice(&x.as_bytes()[0..4]);
    debug_assert_eq!(foot.len(), FOOTER_LEN as usize);
    crate::vfs::write_all_at(&f, &tmp, &foot, off)?;
    crate::vfs::sync_file(&f, &tmp)?;
    drop(f);
    control.check("backup verification")?;
    let staged = Pack::open_with_control(&tmp, control)
        .context("open completed backup before publication")?;
    staged.verify_with_control(control).context("verify completed backup before publication")?;
    drop(staged);
    // The last cancellation point. Once the link exists, return the publication result rather than
    // claiming cancellation after making the requested artifact visible.
    control.check("backup publication")?;
    crate::vfs::link(&tmp, out).map_err(|error| destination_error(out, error))?;
    crate::vfs::unlink(&tmp)?;
    cleanup.disarm();
    crate::vfs::sync_dir(parent_dir(out))?;
    Ok(PackStats { files: entries.len(), bytes: off + FOOTER_LEN })
}

/// Extract every inner file into `out_dir`, byte for byte — after which the directory is an
/// ordinary store, writer role available again. This is the safe restore operation: it verifies
/// the complete pack first, stages and validates the store beside the destination, then atomically
/// publishes it without replacing any existing filesystem object.
pub fn unpack(pack_path: &Path, out_dir: &Path) -> Result<usize> {
    Ok(restore(pack_path, out_dir)?.files)
}

/// Restore a verified pack to a new ordinary store directory.
pub fn restore(pack_path: &Path, out_dir: &Path) -> Result<RestoreStats> {
    restore_with_control(pack_path, out_dir, &crate::control::OperationControl::default())
}

/// [`restore`] with cooperative cancellation before atomic destination publication.
pub fn restore_with_control(
    pack_path: &Path,
    out_dir: &Path,
    control: &crate::control::OperationControl,
) -> Result<RestoreStats> {
    restore_with_limits_and_control(
        pack_path,
        out_dir,
        crate::read_limits::ReadLimits::default(),
        control,
    )
}

/// [`restore_with_control`] with explicit atomic-frame admission while validating the staged store.
pub fn restore_with_limits_and_control(
    pack_path: &Path,
    out_dir: &Path,
    read_limits: crate::read_limits::ReadLimits,
    control: &crate::control::OperationControl,
) -> Result<RestoreStats> {
    let read_limits = read_limits.validate()?;
    control.check("backup restore")?;
    if !ATOMIC_RESTORE {
        return Err(BackupError::Unsupported(
            "this platform has no atomic no-replace directory rename".into(),
        )
        .into());
    }
    ensure_destination_available(out_dir)?;
    let pack = Pack::open_with_control(pack_path, control).map_err(|error| {
        let interrupted = crate::error::classify(&error) == crate::error::ErrorClass::Cancelled;
        let missing = error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        if interrupted || missing {
            error
        } else {
            invalid_backup(pack_path, error)
        }
    })?;
    let files = pack
        .verify_with_control(control)
        .map_err(|error| preserve_control_refusal_or_invalid_backup(pack_path, error))?;
    let manifest = crate::store::manifest_from_bytes(
        &pack
            .read_file_bounded("MANIFEST", crate::store::MAX_MANIFEST_BYTES)
            .map_err(|error| invalid_backup(pack_path, error))?,
    )
    .map_err(|error| preserve_control_refusal_or_invalid_backup(pack_path, error))?;
    for part in &manifest.parts {
        control.check("backup restore validation")?;
        if !safe_relative_name(&part.file) {
            return Err(invalid_backup(
                pack_path,
                format!("manifest names a part outside its own root: {:?}", part.file),
            ));
        }
    }
    for name in pack.names() {
        control.check("backup restore validation")?;
        if !safe_relative_name(name) {
            return Err(invalid_backup(
                pack_path,
                format!("pack names a path outside its own root: {name:?}"),
            ));
        }
    }

    let stage = create_temp_dir(out_dir, "restore")?;
    let mut cleanup = Cleanup::dir(stage.clone());
    extract_into(&pack, &stage, control).context("extract verified TurnDB backup")?;
    control.check("backup restore validation")?;
    crate::store::Store::open_read_with_limits(
        &stage,
        crate::fold::FoldCfg::default(),
        read_limits,
    )
    .map_err(|error| preserve_control_refusal_or_invalid_backup(pack_path, error))?;

    // The last cancellation point. Once renamed, the destination exists and must be reported as the
    // operation's outcome rather than as a cancellation.
    control.check("backup restore publication")?;
    crate::vfs::rename_noreplace(&stage, out_dir).map_err(|error| {
        if error.kind() == std::io::ErrorKind::Unsupported {
            BackupError::Unsupported(error.to_string()).into()
        } else {
            destination_error(out_dir, error)
        }
    })?;
    cleanup.disarm();
    crate::vfs::sync_dir(parent_dir(out_dir))?;
    Ok(RestoreStats { files, bytes: std::fs::metadata(pack_path)?.len(), commit: manifest.commit })
}

fn extract_into(
    pack: &Pack,
    out_dir: &Path,
    control: &crate::control::OperationControl,
) -> Result<()> {
    let names: Vec<String> = pack.names().map(String::from).collect();
    let mut dirs = BTreeSet::new();
    dirs.insert(out_dir.to_path_buf());
    for name in &names {
        control.check("backup restore extraction")?;
        let dst = out_dir.join(name);
        if let Some(parent) = dst.parent() {
            crate::vfs::mkdir_all(parent)?;
            let mut ancestor = Some(parent);
            while let Some(dir) = ancestor {
                if !dir.starts_with(out_dir) {
                    break;
                }
                dirs.insert(dir.to_path_buf());
                if dir == out_dir {
                    break;
                }
                ancestor = dir.parent();
            }
        }
        let source = pack.file(name).ok_or_else(|| anyhow::anyhow!("pack lost file {name}"))?;
        let file = crate::vfs::create_new(&dst)?;
        let len = source.len()?;
        let mut at = 0u64;
        let mut buf = vec![0u8; (1 << 20).min(len.max(1)) as usize];
        while at < len {
            control.check("backup restore extraction")?;
            let take = buf.len().min((len - at) as usize);
            source.read_exact_at(&mut buf[..take], at)?;
            crate::vfs::write_all_at(&file, &dst, &buf[..take], at)?;
            at += take as u64;
        }
        crate::vfs::sync_file(&file, &dst)?;
    }
    // Child directory entries before their parents: after the final directory rename, a crash
    // sees either no destination or the complete tree, never a published tree with missing files.
    let mut dirs: Vec<PathBuf> = dirs.into_iter().collect();
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for dir in dirs {
        control.check("backup restore extraction")?;
        crate::vfs::sync_dir(&dir)?;
    }
    Ok(())
}

fn preserve_control_refusal_or_invalid_backup(path: &Path, error: anyhow::Error) -> anyhow::Error {
    if matches!(
        crate::error::classify(&error),
        crate::error::ErrorClass::Cancelled | crate::error::ErrorClass::ResourceExhausted
    ) {
        error
    } else {
        invalid_backup(path, error)
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

fn safe_relative_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\\')
        && Path::new(name).components().all(|component| matches!(component, Component::Normal(_)))
}

/// Resolve an existing backup member and require it to be the ordinary file at that exact path
/// beneath the canonical store root. This rejects final and intermediate symlinks, preventing an
/// offline attacker-supplied store from turning backup into a host-file disclosure primitive.
fn local_pack_source(dir: &Path, canonical_root: &Path, name: &str) -> Result<PathBuf> {
    if !safe_relative_name(name) {
        bail!("backup source name {name:?} is not a store-relative path");
    }
    let expected = canonical_root.join(name);
    let actual = std::fs::canonicalize(dir.join(name))
        .with_context(|| format!("resolve backup source {name}"))?;
    if actual != expected {
        bail!(
            "backup source {name:?} resolves to {}, outside or through a symlink from {}",
            actual.display(),
            expected.display()
        );
    }
    let metadata = std::fs::metadata(&actual)?;
    if !metadata.is_file() {
        bail!("backup source {name:?} is not a regular file");
    }
    Ok(actual)
}

fn parent_dir(path: &Path) -> &Path {
    path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."))
}

pub(crate) fn ensure_destination_available(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(BackupError::DestinationExists(path.to_path_buf()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect destination {}", path.display())),
    }
}

fn destination_error(path: &Path, error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        BackupError::DestinationExists(path.to_path_buf()).into()
    } else {
        anyhow::Error::new(error).context(format!("publish destination {}", path.display()))
    }
}

fn invalid_backup(path: &Path, error: impl std::fmt::Display) -> anyhow::Error {
    BackupError::InvalidBackup { path: path.to_path_buf(), reason: error.to_string() }.into()
}

fn temp_path(target: &Path, tag: &str) -> Result<PathBuf> {
    let name = target
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("destination {} has no file name", target.display()))?
        .to_string_lossy();
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(parent_dir(target)
        .join(format!(".{name}.turndb-{tag}-{}-{sequence}.tmp", std::process::id())))
}

fn create_temp_file(target: &Path, tag: &str) -> Result<(PathBuf, File)> {
    for _ in 0..128 {
        let path = temp_path(target, tag)?;
        match crate::vfs::create_new(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("could not reserve a unique staging file beside {}", target.display())
}

fn create_temp_dir(target: &Path, tag: &str) -> Result<PathBuf> {
    for _ in 0..128 {
        let path = temp_path(target, tag)?;
        match crate::vfs::mkdir_new(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("could not reserve a unique staging directory beside {}", target.display())
}

enum CleanupKind {
    File,
    Dir,
}

struct Cleanup {
    path: PathBuf,
    kind: CleanupKind,
    armed: bool,
}

impl Cleanup {
    fn file(path: PathBuf) -> Cleanup {
        Cleanup { path, kind: CleanupKind::File, armed: true }
    }

    fn dir(path: PathBuf) -> Cleanup {
        Cleanup { path, kind: CleanupKind::Dir, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match self.kind {
            CleanupKind::File => {
                let _ = crate::vfs::unlink(&self.path);
            }
            CleanupKind::Dir => {
                let _ = crate::vfs::remove_tree(&self.path);
            }
        }
    }
}
