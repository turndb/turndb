//! The catalog: many stores, one query surface — the manifest trick applied one level up.
//!
//! A store is single-writer by design, and the answer to scale is **more stores**, not a bigger
//! one. The measurement that makes this free is the same one that shaped the carve and the keying:
//! dedup in trace data is *trajectory-local*, so partitioning by time or tenant costs almost no
//! deduplication. What it buys is everything the constellation needs:
//!
//! * **Retention becomes `rm`.** A store aligned to a retention window expires by deleting a
//!   directory or a pack — the cheapest and most auditable deletion primitive there is. Erasure
//!   inside a live window stays the job of tombstones, punching, and re-folds.
//! * **Sealed members can be packs.** A member is a directory *or* a `.turndb` file, and the
//!   reader cannot tell — that is what [`crate::readat`] and [`crate::pack`] were built for.
//! * **Writers stay uncontended.** One writer per member; the catalog itself takes no lock and
//!   holds no writer role.
//!
//! # What the catalog is NOT
//!
//! It is not a distributed system, a consensus protocol, or a coordinator. It is a list of member
//! stores plus resolution rules, committed the same way a manifest is: written whole, checksummed,
//! renamed into place. If it is lost, `turndb catalog rebuild` reconstructs it by scanning the
//! directory — every fact in it is derived from the members themselves.
//!
//! # Resolution across members
//!
//! Members are ordered, and later members win: within a member the store's own sequence rules
//! decide, and across members the catalog's order does. A record present in two members resolves
//! to the later one, so an OVERLAY — a small writable member after a sealed pack — is exactly this
//! rule with no new machinery, which is why overlays wait on this and not the other way round.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One member store: where it is, what it holds, and whether it is still open for writes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Member {
    /// Path relative to the catalog directory — so a constellation is movable, copyable, and
    /// packable as a unit.
    pub path: String,
    /// Ordering key. Later wins on conflict; also the natural place for a time-window bound.
    pub ordinal: u64,
    /// Free-form window label (`"2026-W30"`, `"tenant-a/2026-07"`). The catalog never parses it;
    /// retention policy does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// A SEALED member takes no more writes. Packs are sealed by definition; a directory becomes
    /// sealed when policy says its window closed.
    #[serde(default)]
    pub sealed: bool,
}

impl Member {
    /// Is this member a pack file rather than a directory? Answered from the filesystem, not from
    /// the name, because the answer decides which reader opens it.
    pub fn is_pack(&self, root: &Path) -> bool {
        root.join(&self.path).is_file()
    }
}

/// The catalog: members in resolution order.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Catalog {
    pub members: Vec<Member>,
    /// Monotonic commit counter, the same discipline the manifest follows.
    #[serde(default)]
    pub commit: u64,
}

const FILE: &str = "CATALOG";

impl Catalog {
    /// Load, verifying the checksum trailer. A MISSING catalog is an empty constellation; an
    /// unreadable one is an error — the same distinction, and for the same reason, as the manifest:
    /// conflating them lets one bad byte look like "nothing here".
    pub fn load(root: &Path) -> Result<Catalog> {
        match std::fs::read(root.join(FILE)) {
            Ok(b) => Catalog::parse(&b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Catalog::default()),
            Err(e) => Err(e).with_context(|| {
                format!("cannot read {} — refusing to treat an unreadable catalog as an empty one",
                        root.join(FILE).display())
            }),
        }
    }

    fn parse(bytes: &[u8]) -> Result<Catalog> {
        let (payload, want) = match split_trailer(bytes) {
            Some(x) => x,
            None => bail!("CATALOG has no checksum trailer"),
        };
        let got = crc32fast::hash(payload);
        if got != want {
            bail!("CATALOG fails its checksum (crc32 {got:08x}, recorded {want:08x})");
        }
        serde_json::from_slice(payload).context("corrupt CATALOG")
    }

    /// Commit: tmp + fsync + rename + fsync-dir, exactly as the manifest does. The catalog is a
    /// commit point of its own, so it gets the commit discipline of one.
    pub fn commit(&mut self, root: &Path) -> Result<()> {
        self.commit += 1;
        let mut buf = serde_json::to_vec(self)?;
        let crc = crc32fast::hash(&buf);
        buf.extend_from_slice(format!("\ncrc32={crc:08x}").as_bytes());
        let tmp = root.join("CATALOG.tmp");
        let f = crate::vfs::create(&tmp)?;
        crate::vfs::write_all_at(&f, &tmp, &buf, 0)?;
        crate::vfs::sync_file(&f, &tmp)?;
        drop(f);
        crate::vfs::rename(&tmp, &root.join(FILE))?;
        crate::vfs::sync_dir(root)?;
        Ok(())
    }

    /// Add a member, keeping the list in ordinal order. A duplicate path is refused: two entries
    /// for one store would make resolution depend on which was consulted.
    pub fn add(&mut self, m: Member) -> Result<()> {
        if self.members.iter().any(|x| x.path == m.path) {
            bail!("catalog already holds a member at {}", m.path);
        }
        self.members.push(m);
        self.members.sort_by_key(|m| m.ordinal);
        Ok(())
    }

    /// Remove a member by path — the retention primitive. Removing it from the catalog is what
    /// makes it invisible; deleting its bytes is a separate, deliberate step, so an operator can
    /// stage an expiry and still change their mind before it is irreversible.
    pub fn remove(&mut self, path: &str) -> bool {
        let n = self.members.len();
        self.members.retain(|m| m.path != path);
        self.members.len() != n
    }

    /// Members whose window matches a predicate — the retention sweep's input.
    pub fn in_window(&self, pred: impl Fn(&str) -> bool) -> Vec<&Member> {
        self.members
            .iter()
            .filter(|m| m.window.as_deref().is_some_and(&pred))
            .collect()
    }

    /// Rebuild by scanning `root` for stores and packs — the catalog is derived, and this is what
    /// makes losing it an inconvenience rather than a disaster.
    ///
    /// Ordinals come from the sorted member names, so a rebuild is deterministic and matches what
    /// an operator would expect from `ls`. A rebuild cannot recover window labels or seal flags —
    /// those are policy, not facts on disk — and says so rather than inventing them.
    pub fn rebuild(root: &Path) -> Result<Catalog> {
        let mut names: Vec<String> = Vec::new();
        for e in std::fs::read_dir(root)
            .with_context(|| format!("scan {}", root.display()))?
            .flatten()
        {
            let name = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            let is_store = p.is_dir() && (p.join("MANIFEST").exists() || p.join("fold").exists());
            let is_pack = p.is_file() && name.ends_with(".turndb");
            if is_store || is_pack {
                names.push(name);
            }
        }
        names.sort();
        let mut c = Catalog::default();
        for (i, path) in names.into_iter().enumerate() {
            let sealed = root.join(&path).is_file();
            c.members.push(Member { path, ordinal: i as u64, window: None, sealed });
        }
        Ok(c)
    }
}

fn split_trailer(bytes: &[u8]) -> Option<(&[u8], u32)> {
    let pos = bytes.iter().rposition(|&b| b == b'\n')?;
    let tail = &bytes[pos + 1..];
    if tail.len() != 14 || !tail.starts_with(b"crc32=") {
        return None;
    }
    let hex = std::str::from_utf8(&tail[6..]).ok()?;
    Some((&bytes[..pos], u32::from_str_radix(hex, 16).ok()?))
}

/// A read-only view over a whole constellation.
///
/// Opens every member — directory or pack, the reader does not care — and resolves across them by
/// catalog order. Takes no lock and holds no writer role, so it runs happily beside live writers
/// on any member.
pub struct CatalogReader {
    /// Members in resolution order, EARLIEST first.
    stores: Vec<(String, crate::store::ReadStore)>,
}

impl CatalogReader {
    pub fn open(root: &Path, cfg: crate::fold::FoldCfg) -> Result<CatalogReader> {
        let cat = Catalog::load(root)?;
        let mut stores = Vec::with_capacity(cat.members.len());
        for m in &cat.members {
            let p = root.join(&m.path);
            let rs = if m.is_pack(root) {
                crate::store::open_read_pack(&p, cfg)
                    .with_context(|| format!("open packed member {}", m.path))?
            } else {
                crate::store::Store::open_read(&p, cfg)
                    .with_context(|| format!("open member {}", m.path))?
            };
            stores.push((m.path.clone(), rs));
        }
        Ok(CatalogReader { stores })
    }

    pub fn member_count(&self) -> usize {
        self.stores.len()
    }

    /// Every live id across the constellation, sorted and distinct.
    pub fn ids(&self) -> Result<Vec<String>> {
        let mut all = Vec::new();
        for (_, s) in &self.stores {
            all.extend(s.ids()?);
        }
        all.sort();
        all.dedup();
        Ok(all)
    }

    /// Reconstruct `id`, LATER MEMBERS WINNING — which is what makes an overlay after a sealed
    /// pack work with no new machinery.
    pub fn reconstruct(&self, id: &str) -> Result<Option<Vec<u8>>> {
        for (_, s) in self.stores.iter().rev() {
            if let Some(b) = s.reconstruct(id)? {
                return Ok(Some(b));
            }
            // A member that holds the id as a TOMBSTONE answers None here, and so would a member
            // that never held it — the two are indistinguishable through this API, which is why
            // `get` below is the one that resolves deletions correctly.
            if s.get(id)?.is_some() {
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// The record for `id`, later members winning. `None` when no member holds it, or when the
    /// latest member that does holds a deletion.
    pub fn get(&self, id: &str) -> Result<Option<crate::types::Record>> {
        for (_, s) in self.stores.iter().rev() {
            if let Some(r) = s.get(id)? {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// Which member answered for `id` — for tooling that needs to say where data lives.
    pub fn locate(&self, id: &str) -> Result<Option<String>> {
        for (name, s) in self.stores.iter().rev() {
            if s.get(id)?.is_some() {
                return Ok(Some(name.clone()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("turndb-catalog-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn commits_reloads_and_refuses_damage() {
        let d = tmp("commit");
        let mut c = Catalog::default();
        c.add(Member { path: "w1".into(), ordinal: 0, window: Some("2026-W30".into()), sealed: true })
            .unwrap();
        c.add(Member { path: "w2".into(), ordinal: 1, window: Some("2026-W31".into()), sealed: false })
            .unwrap();
        assert!(c.add(Member { path: "w1".into(), ordinal: 9, window: None, sealed: false }).is_err(),
            "a duplicate member path must refuse — resolution would depend on which was consulted");
        c.commit(&d).unwrap();

        let back = Catalog::load(&d).unwrap();
        assert_eq!(back.members, c.members);
        assert_eq!(back.commit, 1);

        let mut b = std::fs::read(d.join(FILE)).unwrap();
        b[12] ^= 0xFF;
        std::fs::write(d.join(FILE), &b).unwrap();
        assert!(Catalog::load(&d).is_err(), "a damaged catalog must refuse, not read as empty");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn ordinals_order_members_and_retention_selects_by_window() {
        let d = tmp("order");
        let mut c = Catalog::default();
        c.add(Member { path: "late".into(), ordinal: 20, window: Some("2026-W31".into()), sealed: false }).unwrap();
        c.add(Member { path: "early".into(), ordinal: 10, window: Some("2026-W30".into()), sealed: true }).unwrap();
        assert_eq!(c.members[0].path, "early", "members must sit in ordinal order");

        let old = c.in_window(|w| w < "2026-W31");
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].path, "early");

        assert!(c.remove("early"));
        assert!(!c.remove("early"), "removing twice is not an error");
        assert_eq!(c.members.len(), 1);
        std::fs::remove_dir_all(&d).ok();
    }
}
