# Bounded incremental part merge

TurnDB supports one schedulable part-merge work unit at a time without embedding a retention or
maintenance policy in the storage engine. The Rust entry points are `Store::plan_compaction`,
`Store::compact_bounded`, and `Store::compact_bounded_with_control`; the native Node entry point is
`NativeStore.compactBounded`. Those public spellings select the part-merge transition; they do not
denote a broader maintenance action.

This complements, rather than changes, the existing operations:

- `auto_compact` retains TurnDB's measured total-at-eight default policy.
- `merge_range` and `compact(true)` remain explicit maintenance-window controls.
- Bounded part merge lets an embedder choose when to admit another measured unit of background work.

## Budget contract

A `CompactionBudget` has three mandatory limits:

- `max_input_parts`: number of immutable part members, at least two;
- `max_input_rows`: physical rows across those members, including superseded rows and tombstones;
- `max_input_bytes`: exact current logical lengths of those part members.

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

Only a run covering every part referenced by the current manifest revision may drop tombstones. Every partial merge
retains them, including a run starting at the oldest part, because a newer part outside that run may
still rely on the delete marker for visibility. Repeated bounded steps can therefore consolidate a
store safely; the final two-part total step can remove tombstones.

A merge writes an unpublished staging part member, checks interruption, atomically publishes a manifest revision that
replaces exactly the planned run, opens that output, and stages only unreachable container members
as free. Cancellation before publication abandons staging and leaves the current manifest revision's part references unchanged. Once publication
occurs, the logical work unit is selected. Its final durability barrier separately produces the
publication acknowledgement; a successor selected without that acknowledgement is reported as an
error without pretending that crash durability was established. Existing readers remain pinned to their
selected manifest revisions and input parts through the ordinary retention rules.

`compact_bounded` plans and executes under the `Store`'s exclusive mutable ownership. In the Node
binding, the writer actor first synchronizes durability and publishes every earlier accepted mutation,
then plans the
run. Commands accepted later wait behind it. The returned input measurements therefore describe the
exact actor-ordered part set referenced by the current manifest revision, not a speculative JavaScript-side estimate.

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

A successful result reports whether the prelude published a pending-change-set part, part counts
before and after, the exact
selected plan, output bytes, and ordinary merge statistics. With fewer than two parts, `plan`,
`outputBytes`, and `merge` are absent. Errors are stable scheduler inputs:

- malformed or zero limits: `INVALID_ARGUMENT`;
- no adjacent pair fits: `RESOURCE_EXHAUSTED`;
- deadline or abort at a safe checkpoint: `CANCELLED`.

The capability profile reports `boundedCompaction: true`. A consumer can implement periodic,
load-aware, or disk-pressure-aware scheduling above this primitive. TurnDB intentionally does not
choose that policy or attach OpenTelemetry, trace, tenant, or retention semantics to it.

## Space preflight

`Store::estimate_compaction_space` and Node `estimateCompactionSpace` expose the exact selected
input plan, compressed and raw section bytes, retained-manifest-revision pinning, and filesystem availability.
They also report an explicitly advisory stage estimate; compression and rebuilt index layout prevent
it from being a hard admission bound. See [maintenance space accounting and preflight](maintenance-space.md)
for the inventory categories, estimate basis, and consumer policy boundary.
