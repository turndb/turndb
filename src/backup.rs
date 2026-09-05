//! Online backup and restore result types and refusal contracts.
//!
//! A backup is another current TurnDB container, not a separate archival format. The store module
//! owns the copy, verification, and atomic artifact-installation protocol; this module keeps its public
//! evidence and typed refusals independent of storage internals.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Whether this target has an atomic rename primitive that refuses replacement.
pub const ATOMIC_RESTORE: bool =
    cfg!(any(target_os = "linux", target_os = "macos", target_os = "ios"));

/// A safe backup/restore refusal that callers can classify without parsing prose.
#[derive(Debug)]
pub enum BackupError {
    DestinationExists(PathBuf),
    SourceStagingCollision { source: PathBuf, staging: PathBuf },
    InvalidBackup { path: PathBuf, reason: String },
    Unsupported(String),
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackupError::DestinationExists(path) => {
                write!(f, "destination {} already exists; refusing to replace it", path.display())
            }
            BackupError::SourceStagingCollision { source, staging } => write!(
                f,
                "source {} is the operation's staging path {}; refusing to remove it",
                source.display(),
                staging.display()
            ),
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

/// What a safe online writer backup did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackupStats {
    pub members: usize,
    pub bytes: u64,
    /// Public store-authority encoding: zero is the canonical origin; a positive value is the
    /// copied manifest revision.
    pub commit: u64,
}

/// What a safe restore installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestoreStats {
    pub members: usize,
    pub bytes: u64,
    /// Public store-authority encoding: zero is the canonical origin; a positive value is the
    /// installed manifest revision.
    pub commit: u64,
}

pub(crate) fn ensure_destination_available(path: &Path) -> Result<()> {
    crate::store::debris::validate_store_path(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(BackupError::DestinationExists(path.to_path_buf()).into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect destination {}", path.display())),
    }
}

pub(crate) fn ensure_source_is_not_staging(source: &Path, staging: &Path) -> Result<()> {
    let same_path = source == staging
        || match (std::fs::canonicalize(source), std::fs::canonicalize(staging)) {
            (Ok(source), Ok(staging)) => source == staging,
            _ => false,
        };
    if same_path {
        return Err(BackupError::SourceStagingCollision {
            source: source.to_path_buf(),
            staging: staging.to_path_buf(),
        }
        .into());
    }
    Ok(())
}
