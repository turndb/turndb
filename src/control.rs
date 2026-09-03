//! Cooperative interruption shared by scans and lifecycle operations.
//!
//! A token or deadline is a request to stop at the next operation-defined safe checkpoint. It is
//! deliberately not an asynchronous thread kill: storage code must never be interrupted between
//! publishing an authority and making the state that authority describes true.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A cheap cooperative cancellation target shared by an operation and its caller.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<TokenState>);

#[derive(Default)]
struct TokenState {
    cancelled: AtomicBool,
    /// Whether `when` holds a condition, so the common poll never takes the lock.
    armed: AtomicBool,
    /// A condition evaluated at each checkpoint; the first at which it holds cancels.
    when: Mutex<Option<Box<dyn Fn() -> bool + Send + Sync>>>,
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.0.cancelled.load(Ordering::Acquire))
            .field("conditional", &self.0.armed.load(Ordering::Acquire))
            .finish()
    }
}

impl CancellationToken {
    pub fn new() -> CancellationToken {
        CancellationToken::default()
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
    }

    /// Cancel at the first checkpoint at which `ready` holds.
    ///
    /// The condition is evaluated by the operation's own cooperative check, on the operation's
    /// thread, so "cancel once the output exists" is decided *at* a checkpoint. A [`cancel`]
    /// from another thread races the operation instead: it can land after the last checkpoint
    /// and be honoured by completion — correct for a caller, and exactly what a test of the
    /// cancelled path cannot rely on. `ready` is dropped once it has fired.
    ///
    /// [`cancel`]: CancellationToken::cancel
    pub fn cancel_when(&self, ready: impl Fn() -> bool + Send + Sync + 'static) {
        *self.0.when.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(ready));
        self.0.armed.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        if self.0.cancelled.load(Ordering::Acquire) {
            return true;
        }
        if self.0.armed.load(Ordering::Acquire) {
            let mut when = self.0.when.lock().unwrap_or_else(|p| p.into_inner());
            if when.as_ref().is_some_and(|ready| ready()) {
                *when = None;
                self.0.armed.store(false, Ordering::Release);
                self.0.cancelled.store(true, Ordering::Release);
                return true;
            }
        }
        false
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

    /// The condition is consulted at each check and fires exactly at the first check where it
    /// holds — not before, and it stays cancelled after.
    #[test]
    fn a_conditional_cancellation_fires_at_the_first_checkpoint_where_it_holds() {
        use std::sync::atomic::AtomicUsize;
        let polls = Arc::new(AtomicUsize::new(0));
        let token = CancellationToken::new();
        let seen = polls.clone();
        token.cancel_when(move || seen.fetch_add(1, Ordering::SeqCst) + 1 == 3);
        let control = OperationControl { deadline: None, cancellation: Some(token.clone()) };
        assert!(control.check("op").is_ok(), "first checkpoint");
        assert!(control.check("op").is_ok(), "second checkpoint");
        let error = control.check("op").unwrap_err();
        assert_eq!(error.reason, InterruptionReason::Cancelled);
        assert!(control.check("op").is_err(), "cancellation is sticky");
        assert_eq!(polls.load(Ordering::SeqCst), 3, "the condition is dropped once it fired");
        assert!(token.is_cancelled());
    }

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
