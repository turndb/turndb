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
    /// LEGAL HOLDS on this member: reasons it must not be expired, each a free-form label
    /// (a matter number, a ticket, a regulation). Retention refuses a held member outright —
    /// **holds beat schedules, always**, because the failure modes are not symmetric: expiring
    /// data under hold is spoliation and cannot be undone, while keeping data past its schedule
    /// is a finding that can be corrected next sweep.
    ///
    /// A list rather than a flag because holds arrive from different matters and each must be
    /// released by its own; a member is free only when every one is gone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holds: Vec<String>,
}

impl Member {
    pub fn on_hold(&self) -> bool {
        !self.holds.is_empty()
    }

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

    /// Place a legal hold. Idempotent: the same reason twice is one hold.
    pub fn hold(&mut self, path: &str, reason: &str) -> Result<()> {
        let m = self
            .members
            .iter_mut()
            .find(|m| m.path == path)
            .ok_or_else(|| anyhow::anyhow!("catalog holds no member {path}"))?;
        if !m.holds.iter().any(|h| h == reason) {
            m.holds.push(reason.to_string());
        }
        Ok(())
    }

    /// Release one hold. Returns whether it was there; a member is expirable only once EVERY
    /// hold is gone.
    pub fn release(&mut self, path: &str, reason: &str) -> Result<bool> {
        let m = self
            .members
            .iter_mut()
            .find(|m| m.path == path)
            .ok_or_else(|| anyhow::anyhow!("catalog holds no member {path}"))?;
        let n = m.holds.len();
        m.holds.retain(|h| h != reason);
        Ok(m.holds.len() != n)
    }

    /// Plan a retention sweep: which members a policy would expire, and which it refuses to.
    ///
    /// A PLAN, not an action, because the whole point of the two-step is that an operator sees
    /// what is about to be destroyed before it is. `expired` is what the policy selects and no
    /// hold protects; `held` is what the policy selected and a hold saved, reported by name and
    /// reason so it appears in the record rather than silently not happening.
    pub fn plan_retention(&self, expire_before: &str) -> RetentionPlan {
        let mut expired = Vec::new();
        let mut held = Vec::new();
        for m in &self.members {
            let Some(w) = m.window.as_deref() else { continue };
            if w >= expire_before {
                continue;
            }
            if m.on_hold() {
                held.push((m.path.clone(), m.holds.clone()));
            } else {
                expired.push(m.path.clone());
            }
        }
        RetentionPlan { expire_before: expire_before.to_string(), expired, held }
    }

    /// Apply a plan: drop the expired members from the catalog and commit.
    ///
    /// The BYTES are not touched — deleting them is a separate, deliberate step, and the returned
    /// paths are what to delete once the operator is satisfied. Retention that destroys in the
    /// same breath as it decides leaves no moment to notice a mistake.
    pub fn apply_retention(&mut self, root: &Path, plan: &RetentionPlan) -> Result<Vec<String>> {
        // Re-check holds at APPLY time: a hold placed between planning and applying must win, or
        // the two-step becomes a race that loses to spoliation.
        let mut removed = Vec::new();
        for path in &plan.expired {
            match self.members.iter().find(|m| m.path == *path) {
                Some(m) if m.on_hold() => {
                    bail!("member {path} came under legal hold after the plan was made — refusing")
                }
                Some(_) => {
                    self.remove(path);
                    removed.push(path.clone());
                }
                None => {} // already gone; not an error
            }
        }
        if !removed.is_empty() {
            self.commit(root)?;
        }
        Ok(removed)
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
            c.members.push(Member { path, ordinal: i as u64, window: None, sealed , holds: Vec::new() });
        }
        Ok(c)
    }
}

/// What a retention sweep would do — the artifact an operator reviews, and the one a retention
/// record is written from.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetentionPlan {
    pub expire_before: String,
    /// Members the policy selects and no hold protects.
    pub expired: Vec<String>,
    /// Members the policy selected and a hold SAVED, with the reasons. Reported rather than
    /// silently skipped: "the schedule did not run here, and why" is exactly what an auditor asks.
    pub held: Vec<(String, Vec<String>)>,
}

/// What a re-seal did.
#[derive(Clone, Debug)]
pub struct ResealStats {
    pub members_collapsed: usize,
    pub records: usize,
    pub bytes: u64,
}

/// Collapse a run of members into ONE sealed pack, and swap the catalog to it.
///
/// The other half of the overlay pattern. Overlay *resolution* needs nothing but catalog order —
/// later members win — but a constellation that only ever grows members is one that slowly loses
/// the point of sealing. Re-sealing folds the corrections back down.
///
/// Written BESIDE the members it replaces and published by a catalog commit, which is the same
/// data-before-pointers discipline every other layer follows: a crash before the commit leaves an
/// unreferenced pack (sweepable), never a catalog naming something that is not there. The old
/// members' bytes are left on disk for the operator to remove — deliberately, because deleting
/// the inputs of an operation in the same breath as committing its output is how a mistake
/// becomes unrecoverable.
///
/// Resolution is preserved exactly: members are read in catalog order, later winning, and the
/// result is written as one store then packed.
pub fn reseal(
    root: &Path,
    members: &[String],
    out_name: &str,
    cfg: crate::fold::FoldCfg,
) -> Result<ResealStats> {
    if members.len() < 2 {
        bail!("re-sealing needs at least two members to collapse");
    }
    let mut cat = Catalog::load(root)?;
    let mut chosen: Vec<&Member> = Vec::new();
    for name in members {
        let m = cat
            .members
            .iter()
            .find(|m| m.path == *name)
            .ok_or_else(|| anyhow::anyhow!("catalog holds no member {name}"))?;
        chosen.push(m);
    }
    // Contiguity is a correctness gate, exactly as it is for a part merge: collapsing members
    // with another member interleaved between them in resolution order would silently change
    // which version of a shared id wins.
    let mut ords: Vec<u64> = chosen.iter().map(|m| m.ordinal).collect();
    ords.sort_unstable();
    let between = cat
        .members
        .iter()
        .filter(|m| m.ordinal > ords[0] && m.ordinal < ords[ords.len() - 1])
        .filter(|m| !members.contains(&m.path))
        .count();
    if between > 0 {
        bail!("re-seal inputs are not contiguous in resolution order — {between} member(s) sit between them");
    }
    let lowest = ords[0];

    // Read every input in resolution order, writing the winner of each id into a staging store.
    let staging = root.join(format!("{out_name}.staging"));
    let _ = std::fs::remove_dir_all(&staging);
    let mut opened: Vec<crate::store::ReadStore> = Vec::new();
    for m in &chosen {
        let p = root.join(&m.path);
        opened.push(if m.is_pack(root) {
            crate::store::open_read_pack(&p, cfg)?
        } else {
            crate::store::Store::open_read(&p, cfg)?
        });
    }
    let mut ids: Vec<String> = Vec::new();
    for s in &opened {
        ids.extend(s.ids()?);
    }
    ids.sort();
    ids.dedup();

    let mut records = 0usize;
    {
        let mut out = crate::store::Store::open(&staging, cfg)?;
        for id in &ids {
            // LATER members win — the same rule CatalogReader applies, so a re-seal cannot change
            // what a reader saw a moment before it ran.
            let mut winner = None;
            for s in opened.iter().rev() {
                if let Some(r) = s.get(id)? {
                    winner = Some((r, s));
                    break;
                }
            }
            let Some((rec, src)) = winner else { continue };
            let body = src.reconstruct(id)?.unwrap_or_default();
            // Spans are re-derived by the engine's carve: the pieces themselves are what dedup
            // works on, and a re-seal is exactly when to let the current opinion apply.
            out.put_body(&rec.id, &body, rec.attrs.clone())?;
            records += 1;
            if records % 2000 == 0 {
                out.sync()?;
                out.flush()?;
            }
        }
        out.sync()?;
        out.flush()?;
        if out.part_count() > 1 {
            out.merge_range(0, out.part_count())?;
        }
    }

    let pack_path = root.join(out_name);
    let stats = crate::pack::write(&staging, &pack_path)?;
    std::fs::remove_dir_all(&staging)?;

    // Publish: the pack is durable before the catalog names it.
    for name in members {
        cat.remove(name);
    }
    cat.add(Member { path: out_name.to_string(), ordinal: lowest, window: None, sealed: true, holds: Vec::new() })?;
    cat.commit(root)?;

    Ok(ResealStats { members_collapsed: members.len(), records, bytes: stats.bytes })
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
        c.add(Member { path: "w1".into(), ordinal: 0, window: Some("2026-W30".into()), sealed: true, holds: Vec::new() })
            .unwrap();
        c.add(Member { path: "w2".into(), ordinal: 1, window: Some("2026-W31".into()), sealed: false, holds: Vec::new() })
            .unwrap();
        assert!(c.add(Member { path: "w1".into(), ordinal: 9, window: None, sealed: false, holds: Vec::new() }).is_err(),
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
        c.add(Member { path: "late".into(), ordinal: 20, window: Some("2026-W31".into()), sealed: false, holds: Vec::new() }).unwrap();
        c.add(Member { path: "early".into(), ordinal: 10, window: Some("2026-W30".into()), sealed: true, holds: Vec::new() }).unwrap();
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
