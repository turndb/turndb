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
the cursor can appear while later-arriving keys before it do not replay. `snapshot()` is the API
spelling that returns the stable read view. Another case exits a child process without closing its
writer after a durable atomic batch and verifies complete WAL replay through the public Node API.

The maintenance workflow creates repeated durable/immutable cuts, verifies the bounded retained
manifest-revision retention window, performs a total part merge without reading folded content, verifies the result, publishes and
restores a writable backup, then physically erases one record. It asserts that refold purges retained
history and leaves no dead or reclaimable content. It also asserts the deliberately narrower erasure
scope: the backup created beforehand is an external copy and still contains the record.

## Sustained profile

`bindings/node/qualification/soak.cjs` is a reusable workload driver. Each cycle durably acknowledges
one atomic batch, publishes one manifest revision, creates both overwritten and tombstoned ids, and
periodically drains part pressure through part-merge units capped at eight inputs. It restarts the
writer, pages and compares every record ID whose slot resolves to a record, measures dead content, preflights and performs a
refold, verifies zero remaining dead/reclaimable bytes, and runs complete store verification. The
ordinary Node suite runs 64 cycles as a bounded regression profile.

A development-host run of the larger local profile, measured on 2026-08-03:

```text
node qualification/soak.cjs <empty-store-dir> 512
512 cycles; 8 records/cycle; 2 KiB payloads
5,630 acknowledged ops; 4,106 record slots resolving to records
127 bounded part merges; 31 writer restarts; 9 current-manifest part references at high water
2,074,624 dead logical bytes before refold
25,165 fold bytes before refold; 201 after
21,472 final logical store bytes; 22,298.89 ms elapsed
```

This is lifecycle evidence, not a throughput benchmark: the payload is deliberately compressible and
the result includes local filesystem/fsync behavior. Running only one eight-input part merge every
eight produced parts is insufficient — the merge output is itself a part, so that cadence
accumulates one part per interval (a high-water of 71 in this profile); draining through additional
bounded units holds the high-water at 9 without replacing bounded work with an unbounded operation.
Part-merge cadence remains consumer policy; TurnDB supplies exact backlog, plans, limits, and outcomes.

The qualification matrix exercises every listed workload property through public consumer seams.
Consumer concepts must not be added to `src/` to keep it passing.
