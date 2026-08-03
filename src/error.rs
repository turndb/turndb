//! Stable, domain-neutral error classification for embedding boundaries.
//!
//! TurnDB keeps rich [`anyhow::Error`] chains internally. Embedders still need a small category they
//! can branch on without parsing those chains' display text. This module classifies only typed causes
//! (plus [`std::io::ErrorKind`]); an unknown failure stays [`ErrorClass::Internal`] rather than being
//! guessed from prose.

/// Stable failure classes shared by Rust embedders and language bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    InvalidArgument,
    Cancelled,
    ResourceExhausted,
    Unsupported,
    Contention,
    NotFound,
    Corruption,
    Io,
    Internal,
}

impl ErrorClass {
    /// Stable upper-snake spelling used by language bindings and diagnostic records.
    pub const fn code(self) -> &'static str {
        match self {
            ErrorClass::InvalidArgument => "INVALID_ARGUMENT",
            ErrorClass::Cancelled => "CANCELLED",
            ErrorClass::ResourceExhausted => "RESOURCE_EXHAUSTED",
            ErrorClass::Unsupported => "UNSUPPORTED",
            ErrorClass::Contention => "CONTENTION",
            ErrorClass::NotFound => "NOT_FOUND",
            ErrorClass::Corruption => "CORRUPTION",
            ErrorClass::Io => "IO",
            ErrorClass::Internal => "INTERNAL",
        }
    }
}

/// An integrity check found persisted bytes or references that violate TurnDB's format invariants.
///
/// Low-level parsers retain detailed context in their source chain. Operations whose purpose is to
/// verify persisted state use this wrapper only for otherwise-unclassified failures; cancellation,
/// contention, and filesystem failures keep their more actionable classes.
#[derive(Debug)]
pub struct IntegrityError {
    context: &'static str,
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl IntegrityError {
    pub fn new(context: &'static str, source: anyhow::Error) -> IntegrityError {
        IntegrityError { context, source: source.into_boxed_dyn_error() }
    }
}

impl std::fmt::Display for IntegrityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.context, self.source)
    }
}

impl std::error::Error for IntegrityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Classify a rich TurnDB error chain without matching its rendered message.
pub fn classify(error: &anyhow::Error) -> ErrorClass {
    use crate::pack::BackupError;
    use crate::store::{CompactionError, RecoveryError, WriteAdmissionError};

    if error.chain().any(|cause| {
        cause.downcast_ref::<crate::scan::ScanInterrupted>().is_some()
            || cause.downcast_ref::<crate::control::OperationInterrupted>().is_some()
    }) {
        return ErrorClass::Cancelled;
    }
    if error.chain().any(|cause| cause.downcast_ref::<IntegrityError>().is_some()) {
        return ErrorClass::Corruption;
    }
    if let Some(admission) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::read_limits::ReadAdmissionError>())
    {
        return match admission {
            crate::read_limits::ReadAdmissionError::InvalidLimits(_) => ErrorClass::InvalidArgument,
            crate::read_limits::ReadAdmissionError::StoredFrameTooLarge { .. }
            | crate::read_limits::ReadAdmissionError::DecodedFrameTooLarge { .. } => {
                ErrorClass::ResourceExhausted
            }
        };
    }
    if error.chain().any(|cause| cause.downcast_ref::<crate::fold::WriterLocked>().is_some()) {
        return ErrorClass::Contention;
    }
    if let Some(admission) =
        error.chain().find_map(|cause| cause.downcast_ref::<WriteAdmissionError>())
    {
        return match admission {
            WriteAdmissionError::InvalidLimits(_)
            | WriteAdmissionError::EmptyIdentifier { .. }
            | WriteAdmissionError::IdentifierTooLong { .. }
            | WriteAdmissionError::DuplicateContentName { .. } => ErrorClass::InvalidArgument,
            WriteAdmissionError::RecordTooLarge { .. }
            | WriteAdmissionError::BatchTooLarge { .. }
            | WriteAdmissionError::TooManyBatchRecords { .. } => ErrorClass::ResourceExhausted,
        };
    }
    if error.chain().any(|cause| cause.downcast_ref::<crate::scan::ScanInputError>().is_some()) {
        return ErrorClass::InvalidArgument;
    }
    if let Some(compaction) =
        error.chain().find_map(|cause| cause.downcast_ref::<CompactionError>())
    {
        return match compaction {
            CompactionError::InvalidBudget(_) => ErrorClass::InvalidArgument,
            CompactionError::BudgetTooSmall { .. } => ErrorClass::ResourceExhausted,
        };
    }
    if let Some(backup) = error.chain().find_map(|cause| cause.downcast_ref::<BackupError>()) {
        return match backup {
            BackupError::DestinationExists(_) => ErrorClass::InvalidArgument,
            BackupError::InvalidBackup { .. } => ErrorClass::Corruption,
            BackupError::Unsupported(_) => ErrorClass::Unsupported,
        };
    }
    if let Some(recovery) = error.chain().find_map(|cause| cause.downcast_ref::<RecoveryError>()) {
        return match recovery {
            RecoveryError::Healthy(_) | RecoveryError::RollbackLimit { .. } => {
                ErrorClass::InvalidArgument
            }
            RecoveryError::NoUsableCandidate { .. } => ErrorClass::Corruption,
        };
    }

    #[cfg(feature = "sql")]
    {
        use crate::query::sql::SqlErrorClass;
        match crate::query::sql::classify_error(error) {
            SqlErrorClass::InvalidArgument => return ErrorClass::InvalidArgument,
            SqlErrorClass::ResourceExhausted => return ErrorClass::ResourceExhausted,
            SqlErrorClass::Unsupported => return ErrorClass::Unsupported,
            SqlErrorClass::Io => return ErrorClass::Io,
            SqlErrorClass::Internal => {}
        }
    }

    if let Some(io) = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()) {
        return match io.kind() {
            std::io::ErrorKind::NotFound => ErrorClass::NotFound,
            std::io::ErrorKind::Unsupported => ErrorClass::Unsupported,
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
                ErrorClass::Corruption
            }
            _ => ErrorClass::Io,
        };
    }
    ErrorClass::Internal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_causes_survive_context_and_io_is_not_guessed_from_prose() {
        let invalid = anyhow::Error::new(crate::scan::ScanInputError::new("bad request"))
            .context("prepare scan");
        assert_eq!(classify(&invalid), ErrorClass::InvalidArgument);

        let missing = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound))
            .context("open part");
        assert_eq!(classify(&missing), ErrorClass::NotFound);

        let prose = anyhow::anyhow!("not found: this is only text");
        assert_eq!(classify(&prose), ErrorClass::Internal);
    }

    #[test]
    fn integrity_is_explicit_and_codes_are_stable() {
        let damaged = anyhow::Error::new(IntegrityError::new(
            "verify part",
            anyhow::anyhow!("checksum mismatch"),
        ));
        assert_eq!(classify(&damaged), ErrorClass::Corruption);
        assert_eq!(classify(&damaged).code(), "CORRUPTION");

        assert_eq!(ErrorClass::InvalidArgument.code(), "INVALID_ARGUMENT");
        assert_eq!(ErrorClass::Cancelled.code(), "CANCELLED");
        assert_eq!(ErrorClass::ResourceExhausted.code(), "RESOURCE_EXHAUSTED");
        assert_eq!(ErrorClass::Unsupported.code(), "UNSUPPORTED");
        assert_eq!(ErrorClass::Contention.code(), "CONTENTION");
        assert_eq!(ErrorClass::NotFound.code(), "NOT_FOUND");
        assert_eq!(ErrorClass::Io.code(), "IO");
        assert_eq!(ErrorClass::Internal.code(), "INTERNAL");
    }
}
