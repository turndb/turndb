//! Operation-local physical read accounting.
//!
//! Part and fold caches are shared by snapshots, so subtracting their global counters before and
//! after a query cannot describe one query when readers overlap. Structured scans are synchronous
//! inside the Rust core. A thread-local scope therefore gives the low-level readers a cheap,
//! operation-local observer without threading an instrumentation parameter through every decoded
//! column helper. Scopes are stacked so a nested operation cannot contaminate its caller.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReadTrace {
    part_sections: HashSet<(u64, String)>,
    pub(crate) part_section_cache_hits: u64,
    pub(crate) part_section_cache_misses: u64,
    pub(crate) part_stored_bytes_read: u64,
    pub(crate) part_raw_bytes_decoded: u64,
    fold_blocks: HashSet<u32>,
    pub(crate) fold_block_cache_hits: u64,
    pub(crate) fold_block_cache_misses: u64,
    pub(crate) fold_stored_bytes_read: u64,
    pub(crate) fold_raw_bytes_decoded: u64,
}

impl ReadTrace {
    pub(crate) fn part_sections_touched(&self) -> usize {
        self.part_sections.len()
    }

    pub(crate) fn fold_blocks_touched(&self) -> usize {
        self.fold_blocks.len()
    }
}

thread_local! {
    static ACTIVE: RefCell<Vec<Rc<RefCell<ReadTrace>>>> = const { RefCell::new(Vec::new()) };
}

/// A synchronous operation's trace scope. Its `Rc` intentionally makes moving the scope to another
/// thread impossible: low-level observations and scope completion must happen on the same thread.
pub(crate) struct ReadTraceScope {
    trace: Rc<RefCell<ReadTrace>>,
    active: bool,
}

impl ReadTraceScope {
    pub(crate) fn start() -> Self {
        let trace = Rc::new(RefCell::new(ReadTrace::default()));
        ACTIVE.with(|active| active.borrow_mut().push(trace.clone()));
        ReadTraceScope { trace, active: true }
    }

    pub(crate) fn finish(mut self) -> ReadTrace {
        self.pop();
        self.trace.borrow().clone()
    }

    fn pop(&mut self) {
        if !self.active {
            return;
        }
        ACTIVE.with(|active| {
            let popped = active.borrow_mut().pop().expect("an active read trace has a stack entry");
            debug_assert!(Rc::ptr_eq(&popped, &self.trace), "read trace scopes must be LIFO");
        });
        self.active = false;
    }
}

impl Drop for ReadTraceScope {
    fn drop(&mut self) {
        self.pop();
    }
}

fn with_active(f: impl FnOnce(&mut ReadTrace)) {
    ACTIVE.with(|active| {
        let trace = active.borrow().last().cloned();
        if let Some(trace) = trace {
            f(&mut trace.borrow_mut());
        }
    });
}

pub(crate) fn part_section(part_id: u64, name: &str, cache_hit: bool, stored: u32, raw: u32) {
    with_active(|trace| {
        trace.part_sections.insert((part_id, name.to_owned()));
        if cache_hit {
            trace.part_section_cache_hits += 1;
        } else {
            trace.part_section_cache_misses += 1;
            trace.part_stored_bytes_read += u64::from(stored);
            trace.part_raw_bytes_decoded += u64::from(raw);
        }
    });
}

pub(crate) fn fold_block_touched(block_id: u32) {
    with_active(|trace| {
        trace.fold_blocks.insert(block_id);
    });
}

pub(crate) fn fold_block_cache_hit() {
    with_active(|trace| trace.fold_block_cache_hits += 1);
}

pub(crate) fn fold_block_cache_miss(stored_frame_bytes: u64, raw: u32) {
    with_active(|trace| {
        trace.fold_block_cache_misses += 1;
        trace.fold_stored_bytes_read += stored_frame_bytes;
        trace.fold_raw_bytes_decoded += u64::from(raw);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_scopes_are_isolated_and_resources_are_distinct() {
        let outer = ReadTraceScope::start();
        part_section(1, "ids", false, 10, 20);
        part_section(1, "ids", true, 10, 20);

        let inner = ReadTraceScope::start();
        part_section(2, "seq", false, 3, 4);
        fold_block_touched(7);
        fold_block_cache_miss(30, 40);
        let inner = inner.finish();
        assert_eq!(inner.part_sections_touched(), 1);
        assert_eq!(inner.part_stored_bytes_read, 3);
        assert_eq!(inner.fold_blocks_touched(), 1);
        assert_eq!(inner.fold_stored_bytes_read, 30);

        fold_block_touched(8);
        fold_block_touched(8);
        fold_block_cache_hit();
        let outer = outer.finish();
        assert_eq!(outer.part_sections_touched(), 1);
        assert_eq!(outer.part_section_cache_hits, 1);
        assert_eq!(outer.part_section_cache_misses, 1);
        assert_eq!(outer.part_stored_bytes_read, 10);
        assert_eq!(outer.fold_blocks_touched(), 1);
        assert_eq!(outer.fold_block_cache_hits, 1);
    }
}
