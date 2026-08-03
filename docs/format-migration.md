# Resumable format migration

TurnDB migrates immutable metadata in small manifest-published steps. It does not require re-ingest,
does not attach consumer schema semantics to a format revision, and does not silently read every
content value merely to manufacture a guarantee an old format never stored.

## Progress model

`Store::format_migration_status()` and Node `formatMigrationStatus(options)` report the current writer
revision and disjoint progress facts:

- `currentParts` and `legacyParts` describe the live manifest;
- `legacyRows` and `legacyBytes` measure live work still to do;
- `retainedLegacy*` measures unique old-format parts pinned only by retained time-travel manifests.

Live migration is complete when `legacyParts` is zero. Retained legacy state is intentionally not
rewritten in place: doing so would mutate historical authorities. It ages out through the ordinary
retention window, remains readable by the compatibility reader meanwhile, and is visible so an
operator knows when the older reader can truly be retired.

Status reads retained manifests and part footers. It is not a constant-work health counter, can fail
on damaged retention metadata, and accepts cooperative cancellation/deadlines.

## One durable step

`Store::migrate_format_step()` rewrites the oldest remaining live legacy part into the current part
format. Node `migrateFormatStep(options)` first syncs and flushes earlier actor work so old WAL frames
also disappear behind a current immutable part. The rewrite:

- preserves the part's sequence range, rows, tombstones, attributes, and named-content programs;
- reads and rewrites no fold content bytes;
- preserves an unavailable legacy whole-content identity as unavailable;
- writes an unreachable output, verifies and hashes it, then publishes one manifest commit.

Cancellation before publication removes the output. Once the manifest commit begins, ordinary crash
recovery owns the result and cancellation is no longer observed. A successful step is therefore a
durable progress unit; repeat until no step is returned. A crash or process restart resumes from the
remaining live legacy parts rather than from an auxiliary migration journal.

## Space preflight

`Store::estimate_format_migration_space()` and Node `estimateFormatMigrationSpace(options)` select
the same oldest legacy part and report its version, sequence range, exact file/row/section sizes,
retained-input pinning, and filesystem availability. `estimatedStageBytes` uses raw section bytes
plus explicit row, section, and format allowance. As with compaction and refold, it is marked
`estimateIsHardBound: false`: recompression and rebuilt indexes are not known before execution.

Preflight is evidence rather than a reservation or a combined plan-and-execute transaction. A
consumer chooses reserve thresholds and admission policy; TurnDB owns format correctness and atomic
publication.
