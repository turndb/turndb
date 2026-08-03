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

/// An open pack: the TOC, and shared access to the underlying bytes.
pub struct Pack {
    f: Arc<File>,
    toc: BTreeMap<String, Entry>,
}

impl Pack {
    pub fn open(path: &Path) -> Result<Pack> {
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
        if toc_off.saturating_add(toc_stored as u64) > len - FOOTER_LEN {
            bail!("pack TOC runs past where the footer says the files end");
        }
        let mut tbuf = vec![0u8; toc_stored as usize];
        crate::sys::read_exact_at(&f, &mut tbuf, toc_off)?;
        if crc32fast::hash(&tbuf) != toc_xsum {
            bail!("pack TOC fails its checksum");
        }
        let toc_bytes = crate::fold::codec::decode(toc_codec, &tbuf, toc_raw, None)?;

        use crate::part::idcol::get_varint;
        let mut at = 0usize;
        let n = get_varint(&toc_bytes, &mut at)? as usize;
        if n != n_files {
            bail!("pack TOC holds {n} entries but the footer says {n_files}");
        }
        let mut toc: BTreeMap<String, Entry> = BTreeMap::new();
        for _ in 0..n {
            let nl = get_varint(&toc_bytes, &mut at)? as usize;
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
        let &(off, len, _) =
            self.toc.get(name).ok_or_else(|| anyhow::anyhow!("pack has no file {name}"))?;
        let mut b = vec![0u8; len as usize];
        ReadAt::read_exact_at(&self.f, &mut b, off)?;
        Ok(b)
    }

    /// Check every inner file against its recorded checksum. NOT done on the read path, by the
    /// same policy as part sections: the inner formats carry their own integrity, and hashing the
    /// whole pack per read would tax every query for a scrub's job.
    pub fn verify(&self) -> Result<usize> {
        let mut checked = 0usize;
        for (name, &(off, len, xsum)) in &self.toc {
            let mut remaining = len;
            let mut at = off;
            let mut h = crc32fast::Hasher::new();
            let mut buf = vec![0u8; (1 << 20).min(len.max(1)) as usize];
            while remaining > 0 {
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
    ensure_destination_available(out)?;
    // Taking the writer role makes the public directory-based operation safe alongside other
    // processes and also replays and includes a durable WAL instead of refusing or omitting it.
    let mut store = crate::store::Store::open(dir, crate::fold::FoldCfg::default())?;
    let stats = store.backup(out)?;
    Ok(PackStats { files: stats.files, bytes: stats.bytes })
}

/// Write a snapshot while the caller owns the store's writer role and has settled its WAL.
pub(crate) fn write_committed(dir: &Path, out: &Path) -> Result<PackStats> {
    let manifest_bytes = std::fs::read(dir.join("MANIFEST"))
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
        let mut src =
            File::open(dir.join(name)).with_context(|| format!("open {name} for packing"))?;
        let start = off;
        let mut h = crc32fast::Hasher::new();
        loop {
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
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut toc = Vec::new();
    put_varint(&mut toc, entries.len() as u64);
    for (name, (o, l, x)) in &entries {
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
    let staged = Pack::open(&tmp).context("open completed backup before publication")?;
    staged.verify().context("verify completed backup before publication")?;
    drop(staged);
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
    if !ATOMIC_RESTORE {
        return Err(BackupError::Unsupported(
            "this platform has no atomic no-replace directory rename".into(),
        )
        .into());
    }
    ensure_destination_available(out_dir)?;
    let pack = Pack::open(pack_path).map_err(|error| {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            error
        } else {
            invalid_backup(pack_path, error)
        }
    })?;
    let files = pack.verify().map_err(|error| invalid_backup(pack_path, error))?;
    let manifest = crate::store::manifest_from_bytes(
        &pack.read_file("MANIFEST").map_err(|error| invalid_backup(pack_path, error))?,
    )
    .map_err(|error| invalid_backup(pack_path, error))?;
    for part in &manifest.parts {
        if !safe_relative_name(&part.file) {
            return Err(invalid_backup(
                pack_path,
                format!("manifest names a part outside its own root: {:?}", part.file),
            ));
        }
    }
    for name in pack.names() {
        if !safe_relative_name(name) {
            return Err(invalid_backup(
                pack_path,
                format!("pack names a path outside its own root: {name:?}"),
            ));
        }
    }

    let stage = create_temp_dir(out_dir, "restore")?;
    let mut cleanup = Cleanup::dir(stage.clone());
    extract_into(&pack, &stage).context("extract verified TurnDB backup")?;
    crate::store::Store::open_read(&stage, crate::fold::FoldCfg::default())
        .map_err(|error| invalid_backup(pack_path, error))?;

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

fn extract_into(pack: &Pack, out_dir: &Path) -> Result<()> {
    let names: Vec<String> = pack.names().map(String::from).collect();
    let mut dirs = BTreeSet::new();
    dirs.insert(out_dir.to_path_buf());
    for name in &names {
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
        crate::vfs::sync_dir(&dir)?;
    }
    Ok(())
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
