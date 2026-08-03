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
    pub punch: OperationMetrics,
    pub refold: OperationMetrics,
    pub format_migration: OperationMetrics,
}
