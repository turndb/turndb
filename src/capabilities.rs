//! Machine-readable build and platform capabilities.
//!
//! A portable binding must not infer guarantees from the host it happens to run on. A WASI module
//! hosted by Linux still lacks `flock`, threads, and hole punching inside the guest. This profile is
//! compiled where the capability is actually available and can be surfaced unchanged by bindings.

use serde::Serialize;

/// The single-writer gate provided by this build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterExclusion {
    /// The operating system releases the advisory lock when the writer exits or crashes.
    OsEnforced,
    /// The embedder must ensure that no other writer opens the same directory.
    EmbedderEnforced,
}

/// How this build can physically reclaim dead or erased content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalErasure {
    /// Linux hole punching is available, with refold as the portable fallback.
    PunchOrRefold,
    /// Reclamation requires rewriting the live fold.
    RefoldOnly,
}

/// Capabilities of the compiled TurnDB core, independent of consumer policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub part_format_write: u8,
    pub part_format_read_max: u8,
    pub writer_exclusion: WriterExclusion,
    pub physical_erasure: PhysicalErasure,
    pub positioned_io: bool,
    pub threads: bool,
    pub columnar: bool,
    pub sql: bool,
    pub portable_wasm: bool,
    pub write_admission_limits: bool,
    pub store_space_usage: bool,
    pub allocated_space_usage: bool,
    pub format_migration: bool,
    pub max_record_bytes_default: u64,
    pub max_batch_bytes_default: u64,
    pub max_batch_records_default: usize,
    pub max_identifier_bytes_default: usize,
}

/// Report what this build can actually guarantee.
pub const fn capabilities() -> Capabilities {
    Capabilities {
        part_format_write: crate::part::PART_VERSION,
        part_format_read_max: crate::part::PART_VERSION,
        writer_exclusion: if cfg!(unix) {
            WriterExclusion::OsEnforced
        } else {
            WriterExclusion::EmbedderEnforced
        },
        physical_erasure: if cfg!(target_os = "linux") {
            PhysicalErasure::PunchOrRefold
        } else {
            PhysicalErasure::RefoldOnly
        },
        positioned_io: true,
        threads: !cfg!(target_arch = "wasm32"),
        columnar: cfg!(feature = "columnar"),
        sql: cfg!(feature = "sql"),
        portable_wasm: cfg!(target_arch = "wasm32"),
        write_admission_limits: true,
        store_space_usage: true,
        allocated_space_usage: cfg!(unix),
        format_migration: true,
        max_record_bytes_default: crate::store::DEFAULT_MAX_RECORD_BYTES,
        max_batch_bytes_default: crate::store::DEFAULT_MAX_BATCH_BYTES,
        max_batch_records_default: crate::store::DEFAULT_MAX_BATCH_RECORDS,
        max_identifier_bytes_default: crate::store::DEFAULT_MAX_IDENTIFIER_BYTES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_implications_are_reported_not_inferred_by_bindings() {
        let c = capabilities();
        assert_eq!(c.part_format_write, crate::part::PART_VERSION);
        assert!(!c.sql || c.columnar, "SQL is an adapter over the columnar lens");
        assert_eq!(c.portable_wasm, cfg!(target_arch = "wasm32"));
        assert!(c.store_space_usage);
        assert_eq!(c.allocated_space_usage, cfg!(unix));
        assert!(c.format_migration);
    }
}
