# Bounded lifecycle event journal

TurnDB records completed lifecycle work in a bounded, process-local journal. Rust
`Store::lifecycle_events_after(after_sequence, limit)` and native Node
`store.lifecycleEvents(afterSequence, limit)` return stable structured facts without invoking a
consumer callback on the storage thread.

Each event contains:

- a monotonically increasing handle-local sequence;
- a stable operation name and terminal outcome;
- the stable TurnDB error class for failures/cancellations; and
- core execution duration in integer nanoseconds.

The journal currently covers successful open/WAL replay plus durability synchronization,
publication, part merge, backup, verification, content punch, refold, and erasure attempts. As with operation metrics, a Node
method may cause several core events when it first synchronizes, publishes, and settles earlier accepted mutations. Durations exclude actor
queue wait. Consumers should add their own wall-clock receipt time and correlation context rather
than asking the storage core to own a clock or tracing vocabulary.

Reads are non-destructive, so independent exporters keep independent cursors. The default journal
retains 256 events. A read reports the oldest available and latest sequence, cumulative evictions,
and `gap: true` when the requested next sequence has already aged out. The consumer should emit its
own loss signal, advance to the returned oldest event, and continue. A small `limit` pages through
retained events without changing the journal.

The event journal is deliberately not durable. Persistent audit history belongs in ordinary TurnDB
records or an external telemetry sink; persisting internal events back into the same writer would
create recursion and alter the workload being observed. The capability descriptor exposes journal
availability and capacity for both native and portable builds.
