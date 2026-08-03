# Pull-based operation metrics

TurnDB exposes monotonic engine facts without choosing a telemetry SDK or invoking consumer code on
the writer thread. Rust `Store::metrics()` and native Node `store.metrics()` return a cheap snapshot
that a consumer can poll, diff, and export to OpenTelemetry, Prometheus, logs, or a local controller.

Each operation class reports:

- attempts, successes, failures, and typed cancellations;
- total, last, and maximum wall time in exact integer nanoseconds.

The outcome classes are disjoint: `attempts == succeeded + failed + cancelled`. Deadlines and aborts
are cancellations because they retain TurnDB's stable typed cause. Counters and time totals saturate
at `u64::MAX` rather than wrapping. Node transports every value as `bigint`.

The initial operation classes are successful open/recovery, sync, flush, compaction, backup, complete
store verification, content punching, refold, and format migration. `recoveredWalFrames` records the frames replayed by this
handle's successful open. A failed open returns no handle, so its error is the evidence; the snapshot
does not fabricate a failed recovery counter it has nowhere to store.

`verificationCorruptionFailures` is the subset of failed verification attempts classified as
`CORRUPTION` at the explicit integrity boundary. Typed cancellations and ordinary filesystem errors
remain separate rather than inflating a corruption alarm. Rust `Store::verify()` verifies the current
committed snapshot; the Node actor first settles accepted writes so `store.verify()` covers them.

Metrics are process- and handle-local, not persisted. An operation invoked internally is still real:
for example a Node maintenance call settles earlier writes through sync/flush, and those operations
increment their own counters. A no-op flush or migration check is a successful attempt. This makes
the numbers describe engine work rather than JavaScript method names.

`foldedContent` measures successful piece writes at the content-addressed boundary: piece attempts,
dedup hits, logical input bytes, and genuinely novel raw bytes. It counts only `Piece` spans (literal
metadata is intentionally outside the fold) and only work performed by this handle. A consumer can
derive hit and byte-avoidance ratios without TurnDB choosing an aggregation window.

`Store::part_distribution()` and Node `partDistribution(options)` are a separate inspectable snapshot
because they read every live part's file metadata. They report exact total/min/p50/p95/max file bytes
and physical rows using nearest-rank order statistics. All values are zero for an empty store, with
`parts` disambiguating emptiness. The call accepts cancellation/deadlines and does not decode rows.

Exact content reachability is likewise a separate, potentially expensive snapshot. See
`content-liveness.md` for the distinction between dead logical bytes, dead bytes stranded inside a
live compressed block, and whole-block payload that punch/refold can reclaim.

Durations cover core execution on the writer thread. They deliberately exclude time waiting in the
Node actor queue; lifecycle deadlines remain submission-inclusive and therefore separately express
queue pressure. Query and structured-scan work keeps its operation-local row, resolution, section,
fold, and byte counters in each query/page result, where concurrent readers cannot contaminate one
another through global deltas.

This pull model is the first exporter hook: polling cannot block or re-enter storage execution, and
the stable shape is straightforward to translate into an external metrics vocabulary. TurnDB may add
bounded structured event polling later; it will not add OpenTelemetry concepts to the storage core.
