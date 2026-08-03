# Bounded incremental compaction

TurnDB supports one schedulable compaction work unit at a time without embedding a retention or
maintenance policy in the storage engine. The Rust entry points are `Store::plan_compaction`,
`Store::compact_bounded`, and `Store::compact_bounded_with_control`; the native Node entry point is
`NativeStore.compactBounded`.

This complements, rather than changes, the existing operations:

- `auto_compact` retains TurnDB's measured total-at-eight default policy.
- `merge_range` and `compact(true)` remain explicit maintenance-window controls.
- Bounded compaction lets an embedder choose when to admit another measured unit of background work.

## Budget contract

A `CompactionBudget` has three mandatory limits:

- `max_input_parts`: number of immutable part files, at least two;
- `max_input_rows`: physical rows across those files, including superseded rows and tombstones;
- `max_input_bytes`: exact current file lengths of those part files.

All three limits apply simultaneously. They bound input work, not elapsed time, peak memory, output
file size, or temporary disk space. Output bytes are reported after a successful merge so an
operator can measure amplification and refine scheduling. Deadlines and cancellation provide a
separate cooperative latency control, but are not hard real-time limits.

The planner considers contiguous runs only. Visibility is newest-wins by part sequence, so merging
an arbitrary non-contiguous subset could expose a version that should remain shadowed. Among eligible
runs, the widest run wins and equal-width runs prefer the oldest. This gives useful work to cold data
without making file age, trace type, or consumer vocabulary part of the API.

If fewer than two parts exist, planning and execution return no work. If parts exist but no adjacent
pair fits, the operation returns `CompactionError::BudgetTooSmall` with the rows and exact bytes of a
concrete smallest-byte adjacent pair. It never silently exceeds the caller's budget. Structurally
invalid limits return `CompactionError::InvalidBudget`.

## Tombstones and publication

Only a run covering the complete current live part list may drop tombstones. Every partial merge
retains them, including a run starting at the oldest part, because a newer part outside that run may
still rely on the delete marker for visibility. Repeated bounded steps can therefore consolidate a
store safely; the final two-part total step settles tombstones.

A merge writes an unpublished staging part, checks interruption, atomically commits a manifest that
replaces exactly the planned run, opens that output, and sweeps only unreachable files. Cancellation
before publication removes staging and leaves the committed part list unchanged. Once publication
occurs, the work unit is complete. Existing readers remain pinned to their prior manifests and input
parts through the ordinary retention rules.

`compact_bounded` plans and executes under the `Store`'s exclusive mutable ownership. In the Node
binding, the writer actor first syncs and flushes every earlier accepted write and only then plans the
run. Commands accepted later wait behind it. The returned input measurements therefore describe the
exact actor-ordered cut that was executed, not a speculative JavaScript-side estimate.

## Native Node use

```js
const result = await store.compactBounded({
  maxInputParts: 4,
  maxInputRows: 250_000n,
  maxInputBytes: 256n * 1024n * 1024n,
}, {
  timeoutMs: 5_000,
  signal: abortController.signal,
});
```

A successful result reports whether settling flushed a part, part counts before and after, the exact
selected plan, output bytes, and ordinary merge statistics. With fewer than two parts, `plan`,
`outputBytes`, and `merge` are absent. Errors are stable scheduler inputs:

- malformed or zero limits: `INVALID_ARGUMENT`;
- no adjacent pair fits: `RESOURCE_EXHAUSTED`;
- deadline or abort at a safe checkpoint: `CANCELLED`.

The capability profile reports `boundedCompaction: true`. A consumer can implement periodic,
load-aware, or disk-pressure-aware scheduling above this primitive. TurnDB intentionally does not
choose that policy or attach OpenTelemetry, trace, tenant, or retention semantics to it.

## Known limit

The input-byte limit is exact and useful, but it is not yet a preflight temporary-space guarantee.
A future lifecycle slice should estimate output and retained-snapshot pinning before maintenance that
may temporarily duplicate metadata. Until then, an operator must leave disk headroom beyond the
input budget, especially when manifest retention keeps replaced inputs alive.
