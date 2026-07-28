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
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

pub const MAGIC: &[u8; 8] = b"TURNPACK";
pub const FOOTER_LEN: u64 = 40;
/// The pack layout this build writes, and the highest it will read. Reject-forward, like the part.
pub const PACK_VERSION: u8 = 1;

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
        let &(off, len, _) = self
            .toc
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("pack has no file {name}"))?;
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
/// Refuses a store with uncommitted records: a pack silently missing acked data would be a lie
/// with a checksum on it. Written tmp + fsync + rename, so a crash leaves no half-pack under the
/// final name.
pub fn write(dir: &Path, out: &Path) -> Result<PackStats> {
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
        let keep = n.ends_with(".fold") || n.ends_with(".dir") || (n.starts_with("zdict-") && n.ends_with(".zd"));
        if keep {
            fold_files.push(format!("{fold_rel}/{n}"));
        }
    }
    fold_files.sort();
    names.extend(fold_files);

    let tmp = out.with_extension("pack.tmp");
    let f = crate::vfs::create(&tmp)?;
    let mut off = 0u64;
    let mut entries: Vec<(String, Entry)> = Vec::with_capacity(names.len());
    let mut buf = vec![0u8; 1 << 20];
    for name in &names {
        let mut src = File::open(dir.join(name))
            .with_context(|| format!("open {name} for packing"))?;
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
    crate::vfs::rename(&tmp, out)?;
    if let Some(parent) = out.parent() {
        let _ = crate::vfs::sync_dir(parent);
    }
    Ok(PackStats { files: entries.len(), bytes: off + FOOTER_LEN })
}

/// Extract every inner file into `out_dir`, byte for byte — after which the directory is an
/// ordinary store, writer role available again. Both crossings are mechanical.
pub fn unpack(pack_path: &Path, out_dir: &Path) -> Result<usize> {
    let pack = Pack::open(pack_path)?;
    let names: Vec<String> = pack.names().map(String::from).collect();
    for name in &names {
        if name.contains("..") || name.starts_with('/') {
            bail!("pack names a path outside its own root: {name:?}");
        }
        let dst = out_dir.join(name);
        if let Some(parent) = dst.parent() {
            crate::vfs::mkdir_all(parent)?;
        }
        crate::vfs::write_file(&dst, &pack.read_file(name)?)?;
    }
    Ok(names.len())
}

/// What a pack write did.
#[derive(Clone, Copy, Debug)]
pub struct PackStats {
    pub files: usize,
    pub bytes: u64,
}
