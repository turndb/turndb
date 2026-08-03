//! Pull-based, telemetry-neutral operation metrics.
//!
//! TurnDB records facts and never invokes consumer callbacks on its storage thread. Embedders can
//! poll these monotonic process-lifetime counters and export deltas to any telemetry system.

use std::time::Duration;

/// Monotonic outcomes and wall time for one operation class on one writer handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OperationMetrics {
    pub attempts: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub total_duration_ns: u64,
    pub last_duration_ns: u64,
    pub max_duration_ns: u64,
}

impl OperationMetrics {
    pub(crate) fn observe<T>(&mut self, duration: Duration, result: &anyhow::Result<T>) {
        self.attempts = self.attempts.saturating_add(1);
        match result {
            Ok(_) => self.succeeded = self.succeeded.saturating_add(1),
            Err(error) if crate::error::classify(error) == crate::error::ErrorClass::Cancelled => {
                self.cancelled = self.cancelled.saturating_add(1);
            }
            Err(_) => self.failed = self.failed.saturating_add(1),
        }
        let elapsed = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.total_duration_ns = self.total_duration_ns.saturating_add(elapsed);
        self.last_duration_ns = elapsed;
        self.max_duration_ns = self.max_duration_ns.max(elapsed);
    }

    pub(crate) fn observe_success(&mut self, duration: Duration) {
        let result: anyhow::Result<()> = Ok(());
        self.observe(duration, &result);
    }
}

/// Cumulative lifecycle metrics since this writer handle opened.
///
/// These counters are not persisted and do not pretend to be a histogram. `open_recovery` can only
/// describe a successful open because a failed open returns no handle; the error itself carries the
/// failure evidence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreMetrics {
    pub open_recovery: OperationMetrics,
    pub recovered_wal_frames: u64,
    pub sync: OperationMetrics,
    pub flush: OperationMetrics,
    pub compaction: OperationMetrics,
    pub backup: OperationMetrics,
    pub verification: OperationMetrics,
    pub verification_corruption_failures: u64,
    pub punch: OperationMetrics,
    pub refold: OperationMetrics,
    pub format_migration: OperationMetrics,
    pub folded_content: FoldedContentMetrics,
}

/// Successful content-piece work observed at the content-addressed write boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FoldedContentMetrics {
    pub pieces: u64,
    pub dedup_hits: u64,
    pub logical_bytes: u64,
    pub novel_bytes: u64,
}

impl FoldedContentMetrics {
    pub(crate) fn observe(&mut self, bytes: usize, deduped: bool) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.pieces = self.pieces.saturating_add(1);
        self.logical_bytes = self.logical_bytes.saturating_add(bytes);
        if deduped {
            self.dedup_hits = self.dedup_hits.saturating_add(1);
        } else {
            self.novel_bytes = self.novel_bytes.saturating_add(bytes);
        }
    }
}

/// Exact file-size and physical-row distribution of the current live immutable parts.
///
/// All values are zero when `parts == 0`; the count disambiguates an empty distribution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PartDistribution {
    pub parts: usize,
    pub total_bytes: u64,
    pub min_bytes: u64,
    pub p50_bytes: u64,
    pub p95_bytes: u64,
    pub max_bytes: u64,
    pub total_rows: u64,
    pub min_rows: u64,
    pub p50_rows: u64,
    pub p95_rows: u64,
    pub max_rows: u64,
}

/// Compressed-block storage occupied by one content-liveness class.
///
/// `raw_bytes` is decompressed fold content and `stored_bytes` is compressed payload length. Frame
/// headers/checksums and filesystem allocation granularity are intentionally excluded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FoldBlockSpace {
    pub blocks: u64,
    pub raw_bytes: u64,
    pub stored_bytes: u64,
}

impl FoldBlockSpace {
    pub(crate) fn checked_observe(&mut self, raw_bytes: u32, stored_bytes: u32) -> Option<()> {
        self.blocks = self.blocks.checked_add(1)?;
        self.raw_bytes = self.raw_bytes.checked_add(u64::from(raw_bytes))?;
        self.stored_bytes = self.stored_bytes.checked_add(u64::from(stored_bytes))?;
        Some(())
    }
}

/// Exact content reachability for a settled store snapshot.
///
/// A live block contains at least one piece referenced by a currently visible record. Reclaimable
/// blocks contain no live pieces and can be removed by punching or refold. Dead bytes inside a live
/// block are stranded until refold because block compression makes the block the reclamation unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentLiveness {
    pub live_pieces: u64,
    pub live_logical_bytes: u64,
    pub dead_logical_bytes: u64,
    pub stranded_dead_logical_bytes: u64,
    pub live_blocks: FoldBlockSpace,
    pub reclaimable_blocks: FoldBlockSpace,
}
