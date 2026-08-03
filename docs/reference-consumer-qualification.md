# Reference consumer qualification

Reference consumers are executable evidence that TurnDB's general record model and native Node seam
can replace application-owned persistence machinery. They do not define storage vocabulary.

The harness under `bindings/node/qualification/` accepts only:

- an ordered record id;
- ordered, explicitly typed fields;
- independently named byte content.

It maps that self-described envelope to the public native binding without an OpenTelemetry SDK,
trace schema, or family enum. The same harness runs two fixtures:

- linked application/AI telemetry with activity, model-call, tool-call, raw-exchange, and ingestion
  diagnostic records;
- a deliberately non-telemetry build pipeline with job, step, artifact, and diagnostic records.

Both qualify schema discovery, arbitrary typed correlation, metadata-only timelines with zero fold
reads, identity equality for shared bytes stored under different content names, selective content
reconstruction, one-call atomic durable ingestion, and restart. A separate live-ingestion case makes
cursor semantics explicit: structured cursors are checked keyset continuations, so later writes after
the cursor can appear while later-arriving keys before it do not replay. `snapshot()` is the stable-cut
mechanism. Another case exits a child process without closing its writer after a durable atomic batch
and verifies complete WAL recovery through the public Node API.

The maintenance workflow creates repeated durable/immutable cuts, verifies the bounded retained
commit window, fully compacts without reading folded content, verifies the result, publishes and
restores a writable backup, then physically erases one record. It asserts that refold purges retained
history and leaves no dead or reclaimable content. It also asserts the deliberately narrower erasure
scope: the backup created beforehand is an external copy and still contains the record.

This is not yet a claim that Phase 6 is complete. The executable suite still needs a measured
sustained retention/compaction soak and an old-format upgrade scenario. Those should extend the same
external harness; adding consumer concepts to `src/` is not an acceptable way to make a fixture pass.
