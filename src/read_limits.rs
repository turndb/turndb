//! Runtime admission for atomic storage frames and persistent object collections.
//!
//! Cache budgets bound bytes retained after a read. They cannot protect the allocation needed to
//! read and decode one frame, because a cache must first materialize an entry before deciding what
//! to retain. These limits are the earlier boundary: persisted lengths are checked before either
//! the stored payload or its decoded destination is allocated. Count limits likewise prevent a
//! directory, WAL, or sparse fold block id from selecting unbounded collection growth.

/// Default maximum stored bytes admitted for one WAL frame, part TOC/section, or fold block:
/// 512 MiB. Existing stores with deliberately larger atomic frames can opt in to a larger value at
/// open; changing the value does not change the format.
pub const DEFAULT_MAX_STORED_FRAME_BYTES: u64 = 512 << 20;

/// Default maximum decoded bytes admitted for one part TOC/section or fold block: 512 MiB.
pub const DEFAULT_MAX_DECODED_FRAME_BYTES: u64 = 512 << 20;

/// Default maximum entries visited in one filesystem directory enumeration.
pub const DEFAULT_MAX_DIRECTORY_ENTRIES: u64 = 100_000;

/// Default maximum physical frames admitted in one unflushed WAL.
pub const DEFAULT_MAX_WAL_FRAMES: u64 = 100_000;

/// Default maximum content blocks admitted in one fold generation.
pub const DEFAULT_MAX_FOLD_BLOCKS: u64 = 1_000_000;

/// Per-handle admission policy for atomic persisted frames and persistent object collections.
///
/// The stored limit bounds input allocation and I/O for one frame. The decoded limit bounds the
/// codec destination. Uncompressed frames are checked against both. Count limits bound filesystem
/// enumeration, physical WAL frames, and fold block indexes. These are runtime policy, not
/// file-format commitments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadLimits {
    pub max_stored_frame_bytes: u64,
    pub max_decoded_frame_bytes: u64,
    pub max_directory_entries: u64,
    pub max_wal_frames: u64,
    pub max_fold_blocks: u64,
}

impl Default for ReadLimits {
    fn default() -> Self {
        ReadLimits {
            max_stored_frame_bytes: DEFAULT_MAX_STORED_FRAME_BYTES,
            max_decoded_frame_bytes: DEFAULT_MAX_DECODED_FRAME_BYTES,
            max_directory_entries: DEFAULT_MAX_DIRECTORY_ENTRIES,
            max_wal_frames: DEFAULT_MAX_WAL_FRAMES,
            max_fold_blocks: DEFAULT_MAX_FOLD_BLOCKS,
        }
    }
}

impl ReadLimits {
    /// Validate a caller-supplied policy against this process's address space.
    pub fn validate(self) -> Result<Self, ReadAdmissionError> {
        if self.max_stored_frame_bytes == 0 {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_stored_frame_bytes must be greater than zero",
            ));
        }
        if self.max_decoded_frame_bytes == 0 {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_decoded_frame_bytes must be greater than zero",
            ));
        }
        if self.max_directory_entries == 0 {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_directory_entries must be greater than zero",
            ));
        }
        if self.max_wal_frames == 0 {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_wal_frames must be greater than zero",
            ));
        }
        if self.max_fold_blocks == 0 {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_fold_blocks must be greater than zero",
            ));
        }
        if self.max_stored_frame_bytes > usize::MAX as u64 {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_stored_frame_bytes exceeds this process's address space",
            ));
        }
        if self.max_decoded_frame_bytes > usize::MAX as u64 {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_decoded_frame_bytes exceeds this process's address space",
            ));
        }
        if self.max_directory_entries > usize::MAX as u64 {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_directory_entries exceeds this process's address space",
            ));
        }
        if self.max_wal_frames > usize::MAX as u64 {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_wal_frames exceeds this process's address space",
            ));
        }
        if self.max_fold_blocks > usize::MAX as u64 || self.max_fold_blocks > u64::from(u32::MAX) {
            return Err(ReadAdmissionError::InvalidLimits(
                "max_fold_blocks exceeds TurnDB's block-id space",
            ));
        }
        Ok(self)
    }

    /// Admit the stored representation before allocating its input buffer.
    pub fn admit_stored(
        self,
        frame: impl Into<String>,
        actual: u64,
    ) -> Result<(), ReadAdmissionError> {
        if actual > self.max_stored_frame_bytes {
            return Err(ReadAdmissionError::StoredFrameTooLarge {
                frame: frame.into(),
                actual,
                allowed: self.max_stored_frame_bytes,
            });
        }
        Ok(())
    }

    /// Admit the decoded representation before allocating its output buffer.
    pub fn admit_decoded(
        self,
        frame: impl Into<String>,
        actual: u64,
    ) -> Result<(), ReadAdmissionError> {
        if actual > self.max_decoded_frame_bytes {
            return Err(ReadAdmissionError::DecodedFrameTooLarge {
                frame: frame.into(),
                actual,
                allowed: self.max_decoded_frame_bytes,
            });
        }
        Ok(())
    }

    /// Admit both dimensions of a codec frame before either allocation.
    pub fn admit(
        self,
        frame: impl Into<String>,
        stored: u64,
        decoded: u64,
    ) -> Result<(), ReadAdmissionError> {
        let frame = frame.into();
        self.admit_stored(frame.clone(), stored)?;
        self.admit_decoded(frame, decoded)
    }

    /// Admit entries visited in one filesystem directory enumeration.
    pub fn admit_directory_entries(
        self,
        directory: impl Into<String>,
        actual: u64,
    ) -> Result<(), ReadAdmissionError> {
        self.admit_objects(directory, actual, self.max_directory_entries)
    }

    /// Admit physical frames retained or scanned in one unflushed WAL.
    pub fn admit_wal_frames(self, actual: u64) -> Result<(), ReadAdmissionError> {
        self.admit_objects("WAL frames", actual, self.max_wal_frames)
    }

    /// Admit content blocks and the largest addressable block id in one fold generation.
    pub fn admit_fold_blocks(self, actual: u64) -> Result<(), ReadAdmissionError> {
        self.admit_objects("fold blocks", actual, self.max_fold_blocks)
    }

    fn admit_objects(
        self,
        collection: impl Into<String>,
        actual: u64,
        allowed: u64,
    ) -> Result<(), ReadAdmissionError> {
        if actual > allowed {
            return Err(ReadAdmissionError::ObjectCountTooLarge {
                collection: collection.into(),
                actual,
                allowed,
            });
        }
        Ok(())
    }
}

/// Stable resource-refusal causes for frame and object-count admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadAdmissionError {
    InvalidLimits(&'static str),
    StoredFrameTooLarge { frame: String, actual: u64, allowed: u64 },
    DecodedFrameTooLarge { frame: String, actual: u64, allowed: u64 },
    ObjectCountTooLarge { collection: String, actual: u64, allowed: u64 },
}

impl std::fmt::Display for ReadAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadAdmissionError::InvalidLimits(reason) => write!(f, "invalid read limits: {reason}"),
            ReadAdmissionError::StoredFrameTooLarge { frame, actual, allowed } => write!(
                f,
                "{frame} stores {actual} bytes, exceeding the configured atomic-frame limit of {allowed}"
            ),
            ReadAdmissionError::DecodedFrameTooLarge { frame, actual, allowed } => write!(
                f,
                "{frame} declares {actual} decoded bytes, exceeding the configured atomic-frame limit of {allowed}"
            ),
            ReadAdmissionError::ObjectCountTooLarge { collection, actual, allowed } => write!(
                f,
                "{collection} contains {actual} objects, exceeding the configured count limit of {allowed}"
            ),
        }
    }
}

impl std::error::Error for ReadAdmissionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_inclusive_and_classify_dimensions_separately() {
        let limits = ReadLimits {
            max_stored_frame_bytes: 10,
            max_decoded_frame_bytes: 20,
            ..ReadLimits::default()
        };
        limits.admit("frame", 10, 20).unwrap();
        assert!(matches!(
            limits.admit("frame", 11, 20),
            Err(ReadAdmissionError::StoredFrameTooLarge { actual: 11, allowed: 10, .. })
        ));
        limits.admit_directory_entries("store directory", 100_000).unwrap();
        assert!(matches!(
            limits.admit_wal_frames(100_001),
            Err(ReadAdmissionError::ObjectCountTooLarge { actual: 100_001, .. })
        ));
        assert!(matches!(
            limits.admit("frame", 10, 21),
            Err(ReadAdmissionError::DecodedFrameTooLarge { actual: 21, allowed: 20, .. })
        ));
    }
}
