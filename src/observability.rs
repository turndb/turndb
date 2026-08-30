//! Pull-based, telemetry-neutral operation metrics.
//!
//! TurnDB records facts and never invokes consumer callbacks on its storage thread. Embedders can
//! poll these monotonic process-lifetime counters and export deltas to any telemetry system.

use std::collections::VecDeque;
use std::time::Duration;

/// Number of lifecycle outcomes retained per writer handle.
pub const EVENT_JOURNAL_CAPACITY: usize = 256;

/// Stable lifecycle operation names carried by [`LifecycleEvent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleOperation {
    OpenRecovery,
    Sync,
    Flush,
    Compaction,
    Backup,
    Verification,
    Punch,
    Refold,
    Erase,
    FormatMigration,
}

impl LifecycleOperation {
    pub const fn name(self) -> &'static str {
        match self {
            LifecycleOperation::OpenRecovery => "open_recovery",
            LifecycleOperation::Sync => "sync",
            LifecycleOperation::Flush => "flush",
            LifecycleOperation::Compaction => "compaction",
            LifecycleOperation::Backup => "backup",
            LifecycleOperation::Verification => "verification",
            LifecycleOperation::Punch => "punch",
            LifecycleOperation::Refold => "refold",
            LifecycleOperation::Erase => "erase",
            LifecycleOperation::FormatMigration => "format_migration",
        }
    }
}

/// Stable terminal outcome for a lifecycle event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

impl LifecycleOutcome {
    pub const fn name(self) -> &'static str {
        match self {
            LifecycleOutcome::Succeeded => "succeeded",
            LifecycleOutcome::Failed => "failed",
            LifecycleOutcome::Cancelled => "cancelled",
        }
    }
}

/// One bounded, telemetry-neutral lifecycle fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub sequence: u64,
    pub operation: LifecycleOperation,
    pub outcome: LifecycleOutcome,
    pub error_class: Option<crate::error::ErrorClass>,
    pub duration_ns: u64,
}

/// Non-destructive journal read for one independent consumer cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LifecycleEventBatch {
    pub events: Vec<LifecycleEvent>,
    pub oldest_available_sequence: Option<u64>,
    pub latest_sequence: u64,
    pub dropped_events: u64,
    pub gap: bool,
}

pub(crate) struct EventJournal {
    events: VecDeque<LifecycleEvent>,
    next_sequence: u64,
    dropped_events: u64,
}

impl Default for EventJournal {
    fn default() -> Self {
        EventJournal {
            events: VecDeque::with_capacity(EVENT_JOURNAL_CAPACITY),
            next_sequence: 1,
            dropped_events: 0,
        }
    }
}

impl EventJournal {
    pub(crate) fn observe<T>(
        &mut self,
        operation: LifecycleOperation,
        duration: Duration,
        result: &anyhow::Result<T>,
    ) {
        let error_class = result.as_ref().err().map(crate::error::classify);
        let outcome = match error_class {
            None => LifecycleOutcome::Succeeded,
            Some(crate::error::ErrorClass::Cancelled) => LifecycleOutcome::Cancelled,
            Some(_) => LifecycleOutcome::Failed,
        };
        if self.events.len() == EVENT_JOURNAL_CAPACITY {
            self.events.pop_front();
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.events.push_back(LifecycleEvent {
            sequence,
            operation,
            outcome,
            error_class,
            duration_ns: u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
        });
    }

    pub(crate) fn read_after(&self, after_sequence: u64, limit: usize) -> LifecycleEventBatch {
        let oldest_available_sequence = self.events.front().map(|event| event.sequence);
        let latest_sequence = self.next_sequence.saturating_sub(1);
        let gap = oldest_available_sequence
            .is_some_and(|oldest| after_sequence.saturating_add(1) < oldest);
        let events = self
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .take(limit)
            .copied()
            .collect();
        LifecycleEventBatch {
            events,
            oldest_available_sequence,
            latest_sequence,
            dropped_events: self.dropped_events,
            gap,
        }
    }
}

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
    pub erase: OperationMetrics,
    pub format_migration: OperationMetrics,
    pub folded_content: FoldedContentMetrics,
    /// Transient names (`store::DebrisKind`) a successful writer open removed because the
    /// protocol proved them dead — the one disposition a returned `Store` can truthfully report:
    /// a removal that fails is the open's error, and nothing is counted. Set at open, not
    /// persisted.
    pub debris_removed: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_event_journal_reports_cursor_gaps_without_destructive_reads() {
        let mut journal = EventJournal::default();
        let result: anyhow::Result<()> = Ok(());
        for _ in 0..EVENT_JOURNAL_CAPACITY + 2 {
            journal.observe(LifecycleOperation::Sync, Duration::from_nanos(7), &result);
        }
        let first = journal.read_after(0, 3);
        assert_eq!(first.events.len(), 3);
        assert_eq!(first.events[0].sequence, 3);
        assert_eq!(first.oldest_available_sequence, Some(3));
        assert_eq!(first.latest_sequence, (EVENT_JOURNAL_CAPACITY + 2) as u64);
        assert_eq!(first.dropped_events, 2);
        assert!(first.gap);
        assert_eq!(first.events[0].duration_ns, 7);

        let again = journal.read_after(0, 3);
        assert_eq!(again, first, "reads must not consume another observer's events");
        let current = journal.read_after(2, 1);
        assert!(!current.gap);
        assert_eq!(current.events[0].sequence, 3);
    }
}
