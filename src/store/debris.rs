//! Transient names current single-file protocols can leave beside a store after a crash.
//!
//! Recognition is exact. A writer removes proven-dead debris only when the final store exists;
//! beside an absent store, the same names refuse accidental creation over an interrupted protocol.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::read_limits::ReadLimits;

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebrisKind {
    /// `<final>.publish-<pid>-<n>` from interrupted Windows final-name installation.
    PendingPublish,
    /// `<store>.creating-<pid>-<n>` from a staged container birth.
    CreationStaging,
    /// `<store>.reclaiming` before reclaim installs its durable anchor.
    ReclaimStaging,
    /// `<store>.reclaimed`, listed as debris only beside a present store.
    ReclaimAnchor,
    /// `<store>.reclaim-candidate` or `<store>.reclaim-candidate.tmp`.
    ReclaimCandidate,
    /// `<store>-tmp/`, the current merge/refold spool directory.
    MergeScratch,
    /// `<store>.backing-up-<pid>-<n>` or `<store>.restoring-<pid>-<n>`.
    ArtifactStaging,
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebrisEntry {
    pub path: PathBuf,
    pub kind: DebrisKind,
}

#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DebrisReport {
    pub entries: Vec<DebrisEntry>,
}

#[derive(Clone, Debug)]
struct Found {
    path: PathBuf,
    kind: DebrisKind,
    is_dir: bool,
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn numbered_protocol_suffix_bytes(name: &[u8], marker: &[u8]) -> bool {
    let Some(marker_at) = name.windows(marker.len()).rposition(|window| window == marker) else {
        return false;
    };
    if marker_at == 0 {
        return false;
    }
    numbered_protocol_instance_bytes(&name[marker_at..], marker)
}

fn numbered_protocol_instance_bytes(name: &[u8], marker: &[u8]) -> bool {
    let Some(suffix) = name.strip_prefix(marker) else { return false };
    let Some(dash) = suffix.iter().position(|&byte| byte == b'-') else { return false };
    let (pid, serial_with_dash) = suffix.split_at(dash);
    let serial = &serial_with_dash[1..];
    !pid.is_empty()
        && !serial.is_empty()
        && pid.iter().all(u8::is_ascii_digit)
        && serial.iter().all(u8::is_ascii_digit)
        && std::str::from_utf8(pid).ok().and_then(|value| value.parse::<u32>().ok()).is_some()
        && std::str::from_utf8(serial).ok().and_then(|value| value.parse::<u64>().ok()).is_some()
}

/// Refuse names that another store can interpret as its mutable protocol state. Without this
/// restriction, opening `foo` can truncate or unlink a perfectly valid store named `foo-wal`,
/// `foo.reclaimed`, or one of the publication staging forms.
pub(crate) fn validate_store_path(path: &Path) -> Result<()> {
    let Some(name) = path.file_name() else { return Ok(()) };
    let name = name.as_encoded_bytes();
    let exact = [
        b"-wal".as_slice(),
        b"-tmp".as_slice(),
        b".reclaiming".as_slice(),
        b".reclaimed".as_slice(),
        b".reclaim-candidate".as_slice(),
        b".reclaim-candidate.tmp".as_slice(),
    ];
    if exact.iter().any(|suffix| name.ends_with(suffix))
        || numbered_protocol_suffix_bytes(name, b".publish-")
        || numbered_protocol_suffix_bytes(name, b".creating-")
        || numbered_protocol_suffix_bytes(name, b".backing-up-")
        || numbered_protocol_suffix_bytes(name, b".restoring-")
    {
        return Err(crate::error::InvalidArgumentError(format!(
            "{} uses a name reserved for TurnDB protocol state",
            path.display()
        ))
        .into());
    }
    Ok(())
}

pub(crate) fn is_pending_publish_of(final_name: &str, candidate: &str) -> bool {
    let Some(rest) = candidate.strip_prefix(final_name) else { return false };
    let Some(rest) = rest.strip_prefix(".publish-") else { return false };
    let mut pieces = rest.split('-');
    matches!(
        (pieces.next(), pieces.next(), pieces.next()),
        (Some(pid), Some(n), None)
            if !pid.is_empty()
                && !n.is_empty()
                && pid.bytes().all(|b| b.is_ascii_digit())
                && n.bytes().all(|b| b.is_ascii_digit())
                && pid.parse::<u32>().is_ok()
                && n.parse::<u64>().is_ok()
    )
}

fn is_artifact_staging_of_os(store_name: &std::ffi::OsStr, candidate: &std::ffi::OsStr) -> bool {
    let store = store_name.as_encoded_bytes();
    let candidate = candidate.as_encoded_bytes();
    let Some(rest) = candidate.strip_prefix(store) else { return false };
    numbered_protocol_instance_bytes(rest, b".backing-up-")
        || numbered_protocol_instance_bytes(rest, b".restoring-")
}

fn is_creation_staging_of_os(store_name: &std::ffi::OsStr, candidate: &std::ffi::OsStr) -> bool {
    let Some(rest) = candidate.as_encoded_bytes().strip_prefix(store_name.as_encoded_bytes())
    else {
        return false;
    };
    numbered_protocol_instance_bytes(rest, b".creating-")
}

fn is_pending_publish_of_os(final_name: &std::ffi::OsStr, candidate: &std::ffi::OsStr) -> bool {
    let Some(rest) = candidate.as_encoded_bytes().strip_prefix(final_name.as_encoded_bytes())
    else {
        return false;
    };
    let Ok(rest) = std::str::from_utf8(rest) else { return false };
    is_pending_publish_of("", rest)
}

fn scan(store: &Path, read_limits: ReadLimits) -> Result<Vec<Found>> {
    let read_limits = read_limits.validate()?;
    let present = store.is_file();
    let reclaim = crate::container::reclaim_names(store);
    let mut found = Vec::new();
    let mut exact = |path: PathBuf, kind: DebrisKind, is_dir: bool| {
        let exists = if is_dir { path.is_dir() } else { path.is_file() };
        if exists && (present || kind != DebrisKind::ReclaimAnchor) {
            found.push(Found { path, kind, is_dir });
        }
    };
    exact(reclaim.staging.clone(), DebrisKind::ReclaimStaging, false);
    exact(reclaim.anchor.clone(), DebrisKind::ReclaimAnchor, false);
    exact(reclaim.candidate_tmp.clone(), DebrisKind::ReclaimCandidate, false);
    exact(reclaim.candidate.clone(), DebrisKind::ReclaimCandidate, false);
    exact(with_suffix(store, "-tmp"), DebrisKind::MergeScratch, true);

    let finals = [
        store.to_path_buf(),
        with_suffix(store, "-wal"),
        reclaim.staging,
        reclaim.anchor,
        reclaim.candidate_tmp,
        reclaim.candidate,
    ];
    let final_names: Vec<std::ffi::OsString> =
        finals.iter().filter_map(|path| path.file_name().map(std::ffi::OsStr::to_owned)).collect();
    let parent = store.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(error) => return Err(error).with_context(|| format!("read {}", parent.display())),
    };
    let mut visited = 0u64;
    for entry in entries {
        visited = visited.saturating_add(1);
        read_limits.admit_directory_entries("store parent during debris scan", visited)?;
        let entry = entry?;
        if !entry.path().is_file() {
            continue;
        }
        if final_names.iter().any(|final_name| {
            is_pending_publish_of_os(final_name.as_os_str(), entry.file_name().as_os_str())
        }) {
            found.push(Found {
                path: entry.path(),
                kind: DebrisKind::PendingPublish,
                is_dir: false,
            });
        } else if store.file_name().is_some_and(|store_name| {
            is_creation_staging_of_os(store_name, entry.file_name().as_os_str())
        }) {
            found.push(Found {
                path: entry.path(),
                kind: DebrisKind::CreationStaging,
                is_dir: false,
            });
        } else if store.file_name().is_some_and(|store_name| {
            is_artifact_staging_of_os(store_name, entry.file_name().as_os_str())
        }) {
            found.push(Found {
                path: entry.path(),
                kind: DebrisKind::ArtifactStaging,
                is_dir: false,
            });
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

pub fn debris_report(path: &Path) -> Result<DebrisReport> {
    debris_report_with_limits(path, ReadLimits::default())
}

pub fn debris_report_with_limits(path: &Path, read_limits: ReadLimits) -> Result<DebrisReport> {
    Ok(DebrisReport {
        entries: scan(path, read_limits)?
            .into_iter()
            .map(|found| DebrisEntry { path: found.path, kind: found.kind })
            .collect(),
    })
}

pub(crate) fn remove_beside_present_store(store: &Path, read_limits: ReadLimits) -> Result<u64> {
    let found = scan(store, read_limits)?;
    let mut removed = 0u64;
    for item in found {
        if item.is_dir {
            crate::vfs::remove_tree(&item.path)
                .with_context(|| format!("remove debris directory {}", item.path.display()))?;
        } else {
            crate::vfs::unlink(&item.path)
                .with_context(|| format!("remove debris file {}", item.path.display()))?;
        }
        removed = removed.saturating_add(1);
    }
    if removed != 0 {
        let parent = store.parent().unwrap_or_else(|| Path::new("."));
        crate::vfs::sync_dir(parent).with_context(|| {
            format!(
                "sync {} after removing recognized store debris beside {}",
                parent.display(),
                store.display()
            )
        })?;
    }
    Ok(removed)
}

pub(crate) fn names_refusing_creation(
    store: &Path,
    read_limits: ReadLimits,
) -> Result<Vec<PathBuf>> {
    let mut names: Vec<PathBuf> = scan(store, read_limits)?
        .into_iter()
        .filter(|found| found.kind != DebrisKind::CreationStaging)
        .map(|found| found.path)
        .collect();
    if !store.exists() {
        let wal = with_suffix(store, "-wal");
        if wal.is_file() && !names.contains(&wal) {
            names.push(wal);
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_current_exact_staging_names_are_recognized() {
        let root = std::env::temp_dir().join(format!(
            "turndb-debris-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = root.join("s.turndb");
        std::fs::write(&store, b"authority").unwrap();
        std::fs::write(root.join("s.turndb.creating-12-2"), b"x").unwrap();
        std::fs::write(root.join("s.turndb.backing-up-12-0"), b"x").unwrap();
        std::fs::write(root.join("s.turndb.restoring-12-1"), b"x").unwrap();
        let near_misses = [
            "s.turndb.backing-up-user.backing-up-12-3",
            "s.turndb.backing-up--12-3",
            "s.turndb.backing-up-12-3-extra",
            "s.turndb.restoring-4294967296-1",
        ];
        for name in near_misses {
            std::fs::write(root.join(name), b"unowned").unwrap();
        }

        let report = debris_report(&store).unwrap();
        assert_eq!(report.entries.len(), 3);
        assert_eq!(
            report.entries.iter().filter(|entry| entry.kind == DebrisKind::CreationStaging).count(),
            1
        );
        assert_eq!(
            report.entries.iter().filter(|entry| entry.kind == DebrisKind::ArtifactStaging).count(),
            2
        );
        remove_beside_present_store(&store, ReadLimits::default()).unwrap();
        for name in near_misses {
            assert_eq!(
                std::fs::read(root.join(name)).unwrap(),
                b"unowned",
                "near-miss protocol name must not be removed: {name}"
            );
        }
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn pending_publication_grammar_is_exact() {
        assert!(is_pending_publish_of("s.turndb", "s.turndb.publish-12-9"));
        assert!(!is_pending_publish_of("s.turndb", "s.turndb.publish-x-9"));
        assert!(!is_pending_publish_of("s.turndb", "other.publish-12-9"));
    }

    #[cfg(unix)]
    #[test]
    fn current_protocol_names_are_recognized_without_utf8_path_assumptions() {
        use std::os::unix::ffi::OsStringExt;

        let root = std::env::temp_dir().join(format!(
            "turndb-debris-nonutf8-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let base = b"s-\xff.turndb".to_vec();
        let store = root.join(std::ffi::OsString::from_vec(base.clone()));
        std::fs::write(&store, b"authority").unwrap();
        let mut staging = base.clone();
        staging.extend_from_slice(b".restoring-12-3");
        let staging = root.join(std::ffi::OsString::from_vec(staging));
        std::fs::write(&staging, b"protocol evidence").unwrap();

        let report = debris_report(&store).unwrap();
        assert_eq!(
            report.entries,
            vec![DebrisEntry { path: staging, kind: DebrisKind::ArtifactStaging }]
        );

        let mut reserved = base;
        reserved.extend_from_slice(b".backing-up-12-4");
        let reserved = root.join(std::ffi::OsString::from_vec(reserved));
        assert!(validate_store_path(&reserved).is_err());
        std::fs::remove_dir_all(root).ok();
    }
}
