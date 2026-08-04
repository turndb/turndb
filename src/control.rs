//! Cooperative interruption shared by scans and lifecycle operations.
//!
//! A token or deadline is a request to stop at the next operation-defined safe checkpoint. It is
//! deliberately not an asynchronous thread kill: storage code must never be interrupted between
//! publishing an authority and making the state that authority describes true.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// A cheap cooperative cancellation target shared by an operation and its caller.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> CancellationToken {
        CancellationToken::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptionReason {
    Cancelled,
    DeadlineExceeded,
}

/// A reusable operation control. Deadlines are absolute so time spent waiting in an embedding
/// actor's queue counts toward the caller's limit.
#[derive(Clone, Debug, Default)]
pub struct OperationControl {
    pub deadline: Option<Instant>,
    pub cancellation: Option<CancellationToken>,
}

impl OperationControl {
    pub fn check(&self, operation: &'static str) -> Result<(), OperationInterrupted> {
        match interruption_reason(self.deadline, self.cancellation.as_ref()) {
            Some(reason) => Err(OperationInterrupted { operation, reason }),
            None => Ok(()),
        }
    }
}

/// A lifecycle operation stopped at a checkpoint where no ambiguous partial success is exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationInterrupted {
    pub operation: &'static str,
    pub reason: InterruptionReason,
}

impl std::fmt::Display for OperationInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason {
            InterruptionReason::Cancelled => write!(f, "{} was cancelled", self.operation),
            InterruptionReason::DeadlineExceeded => {
                write!(f, "{} deadline exceeded", self.operation)
            }
        }
    }
}

impl std::error::Error for OperationInterrupted {}

pub(crate) fn interruption_reason(
    deadline: Option<Instant>,
    cancellation: Option<&CancellationToken>,
) -> Option<InterruptionReason> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Some(InterruptionReason::Cancelled);
    }
    deadline
        .is_some_and(|deadline| Instant::now() >= deadline)
        .then_some(InterruptionReason::DeadlineExceeded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_precedes_deadline_and_is_typed() {
        let token = CancellationToken::new();
        token.cancel();
        let error = OperationControl { deadline: Some(Instant::now()), cancellation: Some(token) }
            .check("maintenance")
            .unwrap_err();
        assert_eq!(error.operation, "maintenance");
        assert_eq!(error.reason, InterruptionReason::Cancelled);
    }
}
