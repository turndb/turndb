# Pull-based operation metrics

TurnDB exposes monotonic engine facts without choosing a telemetry SDK or invoking consumer code on
the writer thread. Rust `Store::metrics()` and native Node `store.metrics()` return a cheap observation
that a consumer can poll, diff, and export to OpenTelemetry, Prometheus, logs, or a local controller.

Each operation class reports:

- attempts, successes, failures, and typed cancellations;
- total, last, and maximum wall time in exact integer nanoseconds.

The outcome classes are disjoint: `attempts == succeeded + failed + cancelled`. Deadlines and aborts
are cancellations because they retain TurnDB's stable typed cause. Counters and time totals saturate
at `u64::MAX` rather than wrapping. Node transports every value as `bigint`.

The initial operation classes are successful open/WAL replay, durability synchronization,
publication, part merge, backup, complete store verification, content punch, refold, and erasure.
`recoveredWalFrames` records the frames replayed by this handle's successful open. A failed open
returns no handle, so its error is the evidence; the observation does not fabricate a failed replay
counter it has nowhere to store.

`verificationCorruptionFailures` is the subset of failed verification attempts classified as
`CORRUPTION` at the explicit integrity boundary. Typed cancellations and ordinary filesystem errors
remain separate rather than inflating a corruption alarm. Rust `Store::verify()` verifies the
current store authority; the Node actor first synchronizes and publishes accepted mutations, then
settles the store so `store.verify()` covers them.

Metrics are process- and handle-local, not persisted. An operation invoked internally is still real:
for example a Node maintenance call synchronizes and publishes earlier accepted mutations, then
settles the store, and those operations increment their own counters. A no-op flush is a successful attempt. This makes
the numbers describe engine work rather than JavaScript method names.

`foldedContent` measures successful piece writes at the content-addressed boundary: piece attempts,
dedup hits, logical input bytes, and genuinely novel raw bytes. It counts only `Piece` spans (literal
metadata is intentionally outside the fold) and only work performed by this handle. A consumer can
derive hit and byte-avoidance ratios without TurnDB choosing an aggregation window.

`Store::part_distribution()` and Node `partDistribution(options)` are a separate inspectable report
because they read metadata for every part referenced by the current manifest revision. They report exact total/min/p50/p95/max member bytes
and physical rows using nearest-rank order statistics. All values are zero for an empty store, with
`parts` disambiguating emptiness. The call accepts cancellation/deadlines and does not decode rows.

Exact content reachability is likewise a separate, potentially expensive observation. See
`content-liveness.md` for the distinction between dead logical bytes, dead bytes stranded inside a
compressed block holding a live-content-reachable piece, and whole-block payload that content punch or refold can reclaim.

Durations cover core execution on the writer thread. They deliberately exclude time waiting in the
Node actor queue; lifecycle deadlines remain submission-inclusive and therefore separately express
queue pressure. Query and structured-scan work keeps its operation-local row, resolution, section,
fold, and byte counters in each query/page result, where concurrent readers cannot contaminate one
another through global deltas.

This pull model also includes bounded structured lifecycle event polling. The sequence cursor and
explicit eviction/gap fields let exporters detect loss without callbacks or re-entry; see
`lifecycle-events.md`. TurnDB does not add OpenTelemetry concepts to the storage core.
