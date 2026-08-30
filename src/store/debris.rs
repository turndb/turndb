//! Transient names the publication and reclaim protocols can leave beside a store after a crash —
//! recognised by exact name or by the layout's own grammar, never by substring; removed by a
//! writer open only beside an authoritative final name; reported by a reader; never silently
//! accumulated. One recognizer, so `turndb inspect` and writer open cannot drift.
//!
//! The audit behind this list is `/objectives/obj-mtg0jtf1-l/audit.md` in the team files.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// What a transient name is. Each variant is one exact shape; the doc comment is the grammar.
/// Non-exhaustive: a future protocol may add a class, and a consumer must match with a wildcard.
///
/// Where a name carries a number, it is a decimal the producer wrote with `{:08}` / `{:04}` —
/// a MINIMUM width, zero-padded — so the grammar here accepts any non-empty decimal, exactly as
/// the engine's own parsers do.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebrisKind {
    /// `<final>.publish-<decimal pid>-<decimal n>`, anchored after a syntactically valid final
    /// name of the layout: a Windows pending publish whose process died before its directory
    /// sync. Never durable, never recovery material.
    PendingPublish,
    /// `<store>.reclaiming`: reclaim's staging container, before the anchor was published.
    ReclaimStaging,
    /// `<store>.reclaimed`: reclaim's anchor. Listed only beside a PRESENT store; beside an
    /// absent one it is recovery's referent, not debris.
    ReclaimAnchor,
    /// `<store>.reclaim-candidate` and `<store>.reclaim-candidate.tmp`.
    ReclaimCandidate,
    /// `<store>-tmp/`: a crashed streaming merge's spool directory (single-file layout).
    MergeScratch,
    /// `MANIFEST.tmp` (directory layout): a commit's staging file before its rename.
    ManifestStaging,
    /// `MANIFEST.<commit>` older than the retention window while a live `MANIFEST` is present
    /// (directory layout): what the commit's prune promised to remove and a crash resurrected.
    ExcessRetainedManifest,
    /// `seg-<n>.dir.tmp` in a fold directory (`fold/` or `fold-<generation>/`): a sidecar
    /// before its rename.
    SegmentSidecarStaging,
    /// `<part>.s<n>.tmp` (directory layout): the part builder's spool, for either part form
    /// (`part-<seq>.part`, or a merged `part-<lo>-<hi>.part`).
    PartBuilderSpool,
    /// `<artifact>.sealing`, `.restoring`, `.converting`: an artifact operation's staging file.
    ArtifactStaging,
}

/// One transient name, as found. `path` is exact and never lossy: only the ASCII suffix or the
/// layout grammar is matched, the rest of the name is carried as it is. Non-exhaustive: fields
/// may be added; the public ones stay readable.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebrisEntry {
    pub path: PathBuf,
    pub kind: DebrisKind,
}

/// The transient names found beside one store or artifact — an inventory, nothing decided.
/// Non-exhaustive: fields may be added; `entries` stays readable.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DebrisReport {
    pub entries: Vec<DebrisEntry>,
}

// ── Grammar ────────────────────────────────────────────────────────────────

/// A non-empty decimal — what `{:08}`/`{:04}` produce (minimum width) and the engine's own
/// parsers accept.
fn decimal(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `part-<seq>` or a merged `part-<lo>-<hi>`, without the `.part` suffix.
fn is_part_stem(stem: &str) -> bool {
    let Some(rest) = stem.strip_prefix("part-") else { return false };
    match rest.split_once('-') {
        None => decimal(rest),
        Some((lo, hi)) => decimal(lo) && decimal(hi),
    }
}

/// `<name>.publish-<decimal pid>-<decimal n>` exactly, anchored after `final_name`.
pub(crate) fn is_pending_publish_of(final_name: &str, candidate: &str) -> bool {
    let Some(rest) = candidate.strip_prefix(final_name) else { return false };
    let Some(rest) = rest.strip_prefix(".publish-") else { return false };
    let mut parts = rest.split('-');
    matches!((parts.next(), parts.next(), parts.next()), (Some(pid), Some(n), None) if decimal(pid) && decimal(n))
}

/// The pending-publish suffix split off a candidate: `Some(final)` if `candidate` is
/// `<final>.publish-<pid>-<n>` for some non-empty `<final>`.
fn pending_publish_final(candidate: &str) -> Option<&str> {
    let at = candidate.rfind(".publish-")?;
    let (final_name, _) = candidate.split_at(at);
    (!final_name.is_empty() && is_pending_publish_of(final_name, candidate)).then_some(final_name)
}

/// A syntactically valid final name in a directory-layout store's root: the manifest and its
/// retained copies and staging, the WAL and lock, both part forms and their builder spools.
fn is_dir_layout_root_final(name: &str) -> bool {
    if name == "MANIFEST" || name == "MANIFEST.tmp" || name == "WAL" || name == "WRITER.lock" {
        return true;
    }
    if let Some(c) = name.strip_prefix("MANIFEST.") {
        return decimal(c);
    }
    if let Some(stem) = name.strip_suffix(".part") {
        return is_part_stem(stem);
    }
    // `<part>.s<n>.tmp`
    if let Some(mid) = name.strip_suffix(".tmp") {
        if let Some((stem, s)) = mid.rsplit_once(".part.s") {
            return is_part_stem(stem) && decimal(s);
        }
    }
    false
}

/// A syntactically valid final name in a fold directory (`fold/` or `fold-<generation>/`).
fn is_fold_dir_final(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("seg-") {
        for suffix in [".fold", ".dir", ".dir.tmp"] {
            if let Some(n) = rest.strip_suffix(suffix) {
                return decimal(n);
            }
        }
    }
    if let Some(rest) = name.strip_prefix("zdict-") {
        if let Some(h) = rest.strip_suffix(".zd") {
            return h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit());
        }
    }
    false
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(suffix);
    PathBuf::from(p)
}

/// A transient name found by the pure scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub path: PathBuf,
    pub kind: DebrisKind,
    pub is_dir: bool,
}

fn entries_in(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(out) };
    for e in rd {
        out.push(e.with_context(|| format!("read {}", dir.display()))?);
    }
    out.sort_by_key(|e| e.file_name());
    Ok(out)
}

// ── Single-file layout ─────────────────────────────────────────────────────

/// The final names a single-file store's protocols publish beside `<store>`.
fn single_file_finals(store: &Path) -> Vec<PathBuf> {
    let names = crate::container::reclaim_names(store);
    vec![
        store.to_path_buf(),
        with_suffix(store, "-wal"),
        names.staging,
        names.anchor,
        names.candidate_tmp,
        names.candidate,
    ]
}

/// The pure scan beside a single-file store: every transient name present, nothing decided.
pub(crate) fn scan_single_file(store: &Path) -> Result<Vec<Found>> {
    let store_present = store.is_file();
    let names = crate::container::reclaim_names(store);
    let dir = store.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let dir = if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir };
    let mut found = Vec::new();
    let mut consider = |path: PathBuf, kind: DebrisKind, is_dir: bool| {
        let exists = if is_dir { path.is_dir() } else { path.is_file() };
        if !exists {
            return;
        }
        // The anchor beside an absent store is recovery's referent, not debris.
        if kind == DebrisKind::ReclaimAnchor && !store_present {
            return;
        }
        found.push(Found { path, kind, is_dir });
    };
    consider(names.staging.clone(), DebrisKind::ReclaimStaging, false);
    consider(names.anchor.clone(), DebrisKind::ReclaimAnchor, false);
    consider(names.candidate_tmp.clone(), DebrisKind::ReclaimCandidate, false);
    consider(names.candidate.clone(), DebrisKind::ReclaimCandidate, false);
    consider(with_suffix(store, "-tmp"), DebrisKind::MergeScratch, true);
    let finals: Vec<String> = single_file_finals(store)
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
        .collect();
    for e in entries_in(&dir)? {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if finals.iter().any(|f| is_pending_publish_of(f, name)) && e.path().is_file() {
            found.push(Found { path: e.path(), kind: DebrisKind::PendingPublish, is_dir: false });
        }
    }
    Ok(found)
}

// ── Directory layout ───────────────────────────────────────────────────────

/// The pure scan of a directory-layout store: staging files by exact grammar, pending publishes
/// anchored to a valid final name of the root or of a fold directory — every `fold/` and
/// `fold-<generation>/` directory in the root is entered — and, when the live manifest is
/// readable, retained copies older than the retention window.
pub(crate) fn scan_dir_layout(dir: &Path) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    let live_commit: Option<u64> = super::read_manifest_file(&dir.join("MANIFEST"))
        .ok()
        .and_then(|b| super::Manifest::parse(&b).ok())
        .map(|m| m.commit);
    for e in entries_in(dir)? {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = e.path();
        if path.is_dir() {
            if super::refold::parse_fold_gen(name).is_some() || name == "fold" {
                for fe in entries_in(&path)? {
                    let fname = fe.file_name();
                    let Some(fname) = fname.to_str() else { continue };
                    if !fe.path().is_file() {
                        continue;
                    }
                    if let Some(rest) = fname.strip_prefix("seg-") {
                        if rest.strip_suffix(".dir.tmp").is_some_and(decimal) {
                            found.push(Found {
                                path: fe.path(),
                                kind: DebrisKind::SegmentSidecarStaging,
                                is_dir: false,
                            });
                            continue;
                        }
                    }
                    if pending_publish_final(fname).is_some_and(is_fold_dir_final) {
                        found.push(Found {
                            path: fe.path(),
                            kind: DebrisKind::PendingPublish,
                            is_dir: false,
                        });
                    }
                }
            }
            continue;
        }
        if !path.is_file() {
            continue;
        }
        if name == "MANIFEST.tmp" {
            found.push(Found { path, kind: DebrisKind::ManifestStaging, is_dir: false });
        } else if let Some(c) = name.strip_prefix("MANIFEST.").filter(|c| decimal(c)) {
            if let (Some(live), Ok(commit)) = (live_commit, c.parse::<u64>()) {
                if commit + (super::MANIFEST_RETAIN as u64) <= live {
                    found.push(Found {
                        path,
                        kind: DebrisKind::ExcessRetainedManifest,
                        is_dir: false,
                    });
                }
            }
        } else if name.starts_with("part-")
            && name.ends_with(".tmp")
            && is_dir_layout_root_final(name)
        {
            found.push(Found { path, kind: DebrisKind::PartBuilderSpool, is_dir: false });
        } else if pending_publish_final(name).is_some_and(is_dir_layout_root_final) {
            found.push(Found { path, kind: DebrisKind::PendingPublish, is_dir: false });
        }
    }
    Ok(found)
}

// ── Artifacts ──────────────────────────────────────────────────────────────

/// The pure scan beside an artifact: its operation's staging file, if a crash left one.
pub(crate) fn scan_artifact(artifact: &Path) -> Vec<Found> {
    let mut found = Vec::new();
    for suffix in [".sealing", ".restoring", ".converting"] {
        let p = with_suffix(artifact, suffix);
        if p.is_file() {
            found.push(Found { path: p, kind: DebrisKind::ArtifactStaging, is_dir: false });
        }
    }
    found
}

// ── The public inventory and the writer's decisions ────────────────────────

/// Read-only: every transient name beside `path` — a single-file store's, a directory-layout
/// store's (when `path` is a directory), an artifact's staging file — nothing touched. What
/// `turndb inspect` prints.
pub fn debris_report(path: &Path) -> Result<DebrisReport> {
    let found = if path.is_dir() {
        scan_dir_layout(path)?
    } else {
        let mut f = scan_single_file(path)?;
        f.extend(scan_artifact(path));
        f
    };
    Ok(DebrisReport {
        entries: found.into_iter().map(|f| DebrisEntry { path: f.path, kind: f.kind }).collect(),
    })
}

/// Writer open beside a PRESENT single-file store: everything the scan found is dead by the
/// protocol and is removed. Returns how many; a removal that fails is the open's error, with the
/// path and the underlying cause (#126: nothing is counted on failure).
pub(crate) fn remove_beside_present_store(store: &Path) -> Result<u64> {
    debug_assert!(store.is_file());
    remove_all(scan_single_file(store)?)
}

/// Writer open of a directory-layout store with a readable live manifest: same rule.
pub(crate) fn remove_in_dir_layout(dir: &Path) -> Result<u64> {
    remove_all(scan_dir_layout(dir)?)
}

fn remove_all(found: Vec<Found>) -> Result<u64> {
    let mut removed = 0u64;
    for f in found {
        let r =
            if f.is_dir { crate::vfs::remove_tree(&f.path) } else { crate::vfs::unlink(&f.path) };
        r.with_context(|| format!("remove transient {:?} {}", f.kind, f.path.display()))?;
        removed += 1;
    }
    Ok(removed)
}

/// Writer open beside an ABSENT single-file store with no anchor: the names that mean "a store
/// was being published here". Nothing is removed; a fresh store is not created over them.
pub(crate) fn names_refusing_creation(store: &Path) -> Result<Vec<PathBuf>> {
    Ok(scan_single_file(store)?
        .into_iter()
        .filter(|f| {
            matches!(
                f.kind,
                DebrisKind::PendingPublish
                    | DebrisKind::ReclaimStaging
                    | DebrisKind::ReclaimCandidate
            )
        })
        .map(|f| f.path)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "turndb-debris-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names_in(d: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn pending_publish_names_are_recognised_exactly() {
        assert!(is_pending_publish_of("s.turndb", "s.turndb.publish-12-3"));
        assert!(is_pending_publish_of("s.turndb-wal", "s.turndb-wal.publish-1-0"));
        assert!(!is_pending_publish_of("s.turndb", "s.turndb.publish-12"));
        assert!(!is_pending_publish_of("s.turndb", "s.turndb.publish-ab-3"));
        assert!(!is_pending_publish_of("s.turndb", "s.turndb.publish-12-3-4"));
        assert!(!is_pending_publish_of("s.turndb", "notes.publish-12-3"));
        assert!(!is_pending_publish_of("s.turndb", "s.turndb.publish-"));
        assert!(!is_pending_publish_of("s.turndb", "other.turndb.publish-12-3"));
        assert!(!is_pending_publish_of("s.turndb", "my.publish-12-3.notes"));
        assert_eq!(
            pending_publish_final("part-00000004.part.publish-7-1"),
            Some("part-00000004.part")
        );
        assert_eq!(pending_publish_final("x.publish-7-1.publish-8-2"), Some("x.publish-7-1"));
        assert_eq!(pending_publish_final(".publish-7-1"), None);
    }

    #[test]
    fn directory_layout_grammar_is_exact() {
        // Minimum-width decimals, both part forms, their spools.
        for ok in [
            "MANIFEST",
            "MANIFEST.00000004",
            "MANIFEST.4",
            "MANIFEST.123456789",
            "MANIFEST.tmp",
            "WAL",
            "WRITER.lock",
            "part-00000004.part",
            "part-4.part",
            "part-00000004-00000009.part",
            "part-00000004.part.s3.tmp",
            "part-00000004-00000009.part.s12.tmp",
        ] {
            assert!(is_dir_layout_root_final(ok), "{ok}");
        }
        for bad in [
            "MANIFEST.",
            "MANIFEST.0000000a",
            "part-.part",
            "part-4-.part",
            "part-a.part",
            "part-00000004.part.sx.tmp",
            "part-00000004.part.s.tmp",
            "notes.txt",
            "part-00000004.partx",
        ] {
            assert!(!is_dir_layout_root_final(bad), "{bad}");
        }
        for ok in ["seg-00000003.fold", "seg-3.fold", "seg-00000003.dir", "seg-00000003.dir.tmp"] {
            assert!(is_fold_dir_final(ok), "{ok}");
        }
        assert!(is_fold_dir_final(&format!("zdict-{}.zd", "ab".repeat(32))));
        assert!(!is_fold_dir_final("seg-.fold"));
        assert!(!is_fold_dir_final("seg-x.fold"));
        assert!(!is_fold_dir_final("zdict-xyz.zd"));
    }

    #[test]
    fn beside_a_present_store_debris_is_removed_and_a_user_file_is_untouched() {
        let d = scratch("present");
        let store = d.join("s.turndb");
        std::fs::write(&store, b"store").unwrap();
        std::fs::write(d.join("s.turndb.reclaiming"), b"x").unwrap();
        std::fs::write(d.join("s.turndb.reclaimed"), b"x").unwrap();
        std::fs::write(d.join("s.turndb.reclaim-candidate"), b"x").unwrap();
        std::fs::write(d.join("s.turndb.publish-4-2"), b"x").unwrap();
        std::fs::write(d.join("s.turndb-wal.publish-4-3"), b"x").unwrap();
        std::fs::create_dir_all(d.join("s.turndb-tmp")).unwrap();
        std::fs::write(d.join("s.turndb-tmp").join("spool"), b"x").unwrap();
        std::fs::write(d.join("my.publish-1-1.notes"), b"mine").unwrap();
        assert_eq!(debris_report(&store).unwrap().entries.len(), 6);
        assert_eq!(remove_beside_present_store(&store).unwrap(), 6);
        assert_eq!(names_in(&d), vec!["my.publish-1-1.notes", "s.turndb"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn beside_an_absent_store_nothing_is_removed_and_creation_is_refused() {
        let d = scratch("absent");
        let store = d.join("s.turndb");
        std::fs::write(d.join("s.turndb.publish-4-2"), b"x").unwrap();
        std::fs::write(d.join("s.turndb.reclaim-candidate"), b"x").unwrap();
        std::fs::create_dir_all(d.join("s.turndb-tmp")).unwrap();
        let refusing = names_refusing_creation(&store).unwrap();
        assert_eq!(refusing.len(), 2, "{refusing:?} — the scratch dir reports but does not refuse");
        assert_eq!(names_in(&d).len(), 3, "nothing removed");
        // The anchor beside an absent store is recovery's, not debris.
        std::fs::write(d.join("s.turndb.reclaimed"), b"x").unwrap();
        assert!(debris_report(&store)
            .unwrap()
            .entries
            .iter()
            .all(|e| e.kind != DebrisKind::ReclaimAnchor));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    // The Windows branch restores its own scratch file's read-only bit to remove it.
    #[allow(clippy::permissions_set_readonly_false)]
    fn a_removal_that_fails_is_the_open_error_with_path_and_cause() {
        let d = scratch("fail");
        let store = d.join("s.turndb");
        std::fs::write(&store, b"store").unwrap();
        let stale = d.join("s.turndb.reclaiming");
        std::fs::write(&stale, b"x").unwrap();
        // Make the removal fail: a read-only parent directory on Unix (unlink needs write on the
        // directory); a read-only file on Windows (DeleteFile refuses it).
        #[cfg(unix)]
        let restore = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
            d.clone()
        };
        #[cfg(windows)]
        let restore = {
            let mut p = std::fs::metadata(&stale).unwrap().permissions();
            p.set_readonly(true);
            std::fs::set_permissions(&stale, p).unwrap();
            stale.clone()
        };
        let err = remove_beside_present_store(&store).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("s.turndb.reclaiming"), "{text}");
        assert!(err.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some()), "{text}");
        assert!(stale.exists(), "nothing was removed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&restore, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(windows)]
        {
            let mut p = std::fs::metadata(&restore).unwrap().permissions();
            p.set_readonly(false);
            std::fs::set_permissions(&restore, p).unwrap();
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn artifact_stagings_are_reported_and_never_removed_by_a_reader() {
        let d = scratch("artifact");
        let out = d.join("backup.turndb");
        std::fs::write(&out, b"sealed").unwrap();
        std::fs::write(d.join("backup.turndb.sealing"), b"x").unwrap();
        let r = debris_report(&out).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].kind, DebrisKind::ArtifactStaging);
        assert!(d.join("backup.turndb.sealing").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn directory_layout_scan_names_every_kind_and_only_those() {
        let d = scratch("dirlayout");
        std::fs::write(d.join("MANIFEST.tmp"), b"x").unwrap();
        std::fs::write(d.join("part-00000004.part.s2.tmp"), b"x").unwrap();
        std::fs::write(d.join("part-00000004.part.publish-9-1"), b"x").unwrap();
        std::fs::write(d.join("part-00000005.part"), b"live part").unwrap();
        std::fs::write(d.join("notes.publish-9-1"), b"mine").unwrap();
        std::fs::create_dir_all(d.join("fold")).unwrap();
        std::fs::write(d.join("fold").join("seg-00000001.dir.tmp"), b"x").unwrap();
        std::fs::write(d.join("fold").join("seg-00000002.fold.publish-9-3"), b"x").unwrap();
        std::fs::write(d.join("fold").join("seg-00000002.fold"), b"live").unwrap();
        let r = debris_report(&d).unwrap();
        let mut kinds: Vec<(String, DebrisKind)> = r
            .entries
            .iter()
            .map(|e| (e.path.file_name().unwrap().to_string_lossy().to_string(), e.kind))
            .collect();
        kinds.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            kinds,
            vec![
                ("MANIFEST.tmp".to_string(), DebrisKind::ManifestStaging),
                ("part-00000004.part.publish-9-1".to_string(), DebrisKind::PendingPublish),
                ("part-00000004.part.s2.tmp".to_string(), DebrisKind::PartBuilderSpool),
                ("seg-00000001.dir.tmp".to_string(), DebrisKind::SegmentSidecarStaging),
                ("seg-00000002.fold.publish-9-3".to_string(), DebrisKind::PendingPublish),
            ]
        );
        assert_eq!(remove_in_dir_layout(&d).unwrap(), 5);
        assert!(d.join("notes.publish-9-1").exists() && d.join("part-00000005.part").exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
