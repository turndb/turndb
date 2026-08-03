//! Runtime admission for atomic storage frames.
//!
//! Cache budgets bound bytes retained after a read. They cannot protect the allocation needed to
//! read and decode one frame, because a cache must first materialize an entry before deciding what
//! to retain. These limits are the earlier boundary: persisted lengths are checked before either
//! the stored payload or its decoded destination is allocated.

/// Default maximum stored bytes admitted for one WAL frame, part TOC/section, or fold block:
/// 512 MiB. Existing stores with deliberately larger atomic frames can opt in to a larger value at
/// open; changing the value does not change the format.
pub const DEFAULT_MAX_STORED_FRAME_BYTES: u64 = 512 << 20;

/// Default maximum decoded bytes admitted for one part TOC/section or fold block: 512 MiB.
pub const DEFAULT_MAX_DECODED_FRAME_BYTES: u64 = 512 << 20;

/// Per-handle admission policy for atomic persisted frames.
///
/// The stored limit bounds input allocation and I/O for one frame. The decoded limit bounds the
/// codec destination. Uncompressed frames are checked against both. These are runtime policy, not
/// file-format commitments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadLimits {
    pub max_stored_frame_bytes: u64,
    pub max_decoded_frame_bytes: u64,
}

impl Default for ReadLimits {
    fn default() -> Self {
        ReadLimits {
            max_stored_frame_bytes: DEFAULT_MAX_STORED_FRAME_BYTES,
            max_decoded_frame_bytes: DEFAULT_MAX_DECODED_FRAME_BYTES,
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
}

/// Stable resource-refusal causes for persisted-frame admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadAdmissionError {
    InvalidLimits(&'static str),
    StoredFrameTooLarge { frame: String, actual: u64, allowed: u64 },
    DecodedFrameTooLarge { frame: String, actual: u64, allowed: u64 },
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
        }
    }
}

impl std::error::Error for ReadAdmissionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_inclusive_and_classify_dimensions_separately() {
        let limits = ReadLimits { max_stored_frame_bytes: 10, max_decoded_frame_bytes: 20 };
        limits.admit("frame", 10, 20).unwrap();
        assert!(matches!(
            limits.admit("frame", 11, 20),
            Err(ReadAdmissionError::StoredFrameTooLarge { actual: 11, allowed: 10, .. })
        ));
        assert!(matches!(
            limits.admit("frame", 10, 21),
            Err(ReadAdmissionError::DecodedFrameTooLarge { actual: 21, allowed: 20, .. })
        ));
    }
}
