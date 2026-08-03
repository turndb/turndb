# `@turndb/native`

This is TurnDB's native Node embedding path. It is an N-API 6 addon, so it targets stable Node ABI
compatibility rather than one V8 or Node release. Each `NativeStore` owns a dedicated Rust thread and
a bounded command queue; filesystem, compression, scan, and sync work do not run on the JavaScript
event loop.

This package is currently a source prototype, not a published package. Build and exercise it from the
workspace with:

```sh
npm run test:dev --prefix bindings/node
```

The package loader accepts `TURNDB_NATIVE_PATH` for development and otherwise looks for a packaged
platform prebuild. It intentionally does not fall back to `turndb-wasm`: native writer exclusion,
threads, and physical reclamation are capabilities, not implementation details that may disappear
silently.

## Semantics

- `write(ops, durable)` applies the ordered operations as one atomic batch. A successful call with
  `durable: true` has also synced the WAL. With `false` (the default), call `sync()` for a durability
  acknowledgement.
- Writer-open options `maxRecordBytes`, `maxBatchBytes`, `maxBatchRecords`, and
  `maxIdentifierBytes` configure Rust-owned admission policy. Batches are fully charged before the
  first fold mutation; byte limits use deterministic worst-case framed-WAL sizes rather than current
  dedup state. Size/count refusals are `RESOURCE_EXHAUSTED`, while malformed limits or names are
  `INVALID_ARGUMENT`. See [write admission limits](../../docs/write-admission.md).
- Attributes are an ordered array, not an object. Duplicate names and exact scalar types survive.
  Signed/unsigned integers and UTC nanosecond timestamps enter and leave as JavaScript `bigint`;
  binary metadata and content use `Buffer`; explicit null carries its own `kind`. See
  [the revision-4 scalar contract](../../docs/field-types-v4.md).
- `scan()` is the Rust structured pager. Rust owns visibility, filtering, ordering, work bounds, and
  opaque cursor validation. The writer view includes accepted unflushed writes. Content metadata
  includes `identity`, the lowercase BLAKE3 hex digest of the complete reconstructed value, when its
  record format carried one; obtaining it does not read the content. `timeoutMs` establishes an
  absolute deadline before submission, so actor-queue time counts, and `signal` accepts an
  `AbortSignal`. Both stop cooperatively in Rust and reject with `CANCELLED`; no partial page is
  presented as success. Byte projections are limited to 32 MiB per page by default;
  `maxReconstructedBytes` overrides the ceiling as a lossless `bigint`. TurnDB never splits a row,
  admits one oversized row so paging can progress, and sets `reconstructionBudgetExhausted` when the
  continuation resumes at a row deferred by the ceiling. Metadata-only projections spend zero bytes.
  Committed rows decode only attribute/content columns used by the projection or predicates; sibling
  value, dictionary, and content-program sections remain unopened. Every successful page's `stats.io`
  reports exact operation-local part sections and fold blocks touched, cache access counts, and
  stored/raw bytes as `bigint`; concurrent snapshots cannot contaminate those numbers.
  `stats.resolution` reports physical immutable rows, superseded rows, deciding tombstones, and
  writer-memtable entries consumed before predicates, also as `bigint`. `maxResolutionEntries`
  bounds the sum per page (1,000,000 by default); equal-id groups stay atomic, one oversized first
  group is admitted for progress, and `budgetExhausted` explains a partial page. Empty pages may carry
  `next` after bounded progress through tombstone-only groups. See
  [projected structured scans](../../docs/projected-structured-scan.md) and
  [structured scan I/O statistics](../../docs/structured-scan-io.md), plus the
  [resolved-row budget contract](../../docs/resolved-row-paging.md).
- `explainScan()` validates and prepares the same request and opaque cursor as `scan()`, then reports
  projected, predicate-only, and byte-reconstructed fields; effective bounds and budgets; and exact
  pre-resolution part/row/memtable scope. It does not estimate result counts or read value/content
  columns. See [structured scan explanation](../../docs/scan-explanation.md).
- Every rejection is normalized to `TurnDbError`. Its stable `code` comes from the Rust engine's
  typed cause classifier; `BUSY` and `CLOSED` are the only binding-owned states. Messages retain full
  diagnostic context but are not an API. See [error taxonomy](../../docs/error-taxonomy.md).
- `snapshot()` flushes all earlier accepted writes and returns an immutable reader at that exact
  actor-serialized cut. `NativeSnapshot.open()` opens the currently published manifest without a
  writer lock; `openAt()` reopens a commit still inside the bounded retention window.
- `backup(path)` settles earlier actor commands, writes and fully verifies an immutable pack, and
  atomically publishes it without replacing an existing destination. `restoreBackup(pack, dir)`
  verifies every member, extracts with bounded memory, validates the staged store, and atomically
  publishes a new writable directory without overlaying any filesystem object. Safe restore reports
  `UNSUPPORTED` on a platform without an OS no-replace directory rename; the capability is exposed
  as `backupRestore`. Both accept `timeoutMs`/`AbortSignal`; cancellation before the final atomic
  link/rename removes unpublished staging and never publishes the requested destination.
- `recoverManifest(path, { maxRollbackCommits, timeoutMs, signal })` is an offline, exclusive
  recovery control. It
  refuses a healthy store or live writer, validates the exact fold prefix, every part/section and
  every visible content value before publication, and defaults to permitting no rollback past the
  newest retained commit. The result reports the selected commit, rollback distance, and validation
  work. Cancellation during validation leaves the damaged manifest and retained history unchanged;
  promotion is the final uninterruptible boundary. See
  [the recovery procedure](../../docs/recovery.md).
- `querySql()` is the richer immutable query plane. The native package deliberately includes the
  Arrow/DataFusion dependency: Rust binds typed `$1` parameters, refuses DDL/DML/session statements,
  enforces a configurable execution-memory pool (256 MiB by default), and returns a
  `NativeSqlQuery`. Query options and pull options accept `timeoutMs` and `AbortSignal`; planning
  cancellation drops the unfinished DataFusion future and releases its memory reservation.
  `schemaIpc` is a zero-batch Arrow stream; each `next()` returns one complete, independently
  decodable Arrow IPC stream in a `Buffer`, and close/drop cancels work not yet pulled. Calling it on a writer first publishes an exact
  actor-ordered snapshot; calling it on `NativeSnapshot` never mutates the store.
  Live queries reserve those per-query ceilings from a shared aggregate budget (1 GiB by default,
  configurable with `maxConcurrentSqlMemoryBytes`). Writer-derived snapshots share their writer's
  budget; exhaustion fails immediately and reservations release on EOF, error, cancellation, close,
  or drop. Handles expose both the limit and currently reserved bytes.
- `sync({ timeoutMs, signal })` and `flush({ timeoutMs, signal })` include actor queue time. Sync
  observes interruption only before WAL fsync; flush also checks its unpublished planning and digest
  phases and removes a staged part on cancellation. Neither reports cancellation after its final
  durability/publication boundary begins.
- `close()` syncs by default. Passing `false` is an explicit no-sync close.
- Calls made after close refuse. `NativeStore.open(path, { commandQueueCapacity })` sets the accepted
  backlog from 1 through 65,536; the default remains 64 and the handle reports its actual value.
  Once that many operations are queued, ordinary operations refuse with an overload error rather
  than creating an unbounded backlog. `close()` remains admissible when the queue is full.
- Rejections use `TurnDbError` with a stable `code`. The initial classes distinguish
  `INVALID_ARGUMENT`, `BUSY`, `CLOSED`, `CONTENTION`, `CANCELLED`, write/SQL
  `RESOURCE_EXHAUSTED`, `UNSUPPORTED`, and `INTERNAL`; the original native error is retained as `cause`
  and the full contextual message remains available. The declared code union reserves `NOT_FOUND`,
  `CORRUPTION`, and broader `IO` use while typed engine errors are added—unclassified core failures
  report `INTERNAL` rather than being guessed from prose.
- `compact()`, `verify()`, `punch()`, `refold()`, and `backup()` run on the same serialized writer actor. They
  sync and flush earlier writes before operating, so their reports cover an exact cut and their
  filesystem work stays off the event loop. `compact(true)` requests a full merge; the default uses
  the engine's measured automatic policy. Each accepts queue-inclusive `timeoutMs` and
  `AbortSignal` options backed by Rust cooperative checkpoints. Cancelled compaction/refold staging
  is removed; punching retains safe resumable progress. See
  [lifecycle cancellation and deadlines](../../docs/lifecycle-control.md).
- `compactBounded({ maxInputParts, maxInputRows, maxInputBytes }, options)` publishes one contiguous
  merge within all three exact physical-input limits. It reports the executed plan, output bytes,
  and merge statistics; an insufficient budget is `RESOURCE_EXHAUSTED`, never an implicit overrun.
  Only a total-live-list step drops tombstones. See
  [bounded incremental compaction](../../docs/bounded-compaction.md).
- `erase(ids)` is deliberately strong: it tombstones present ids, settles tombstones, rewrites live
  content, and purges retained manifests so this store has no snapshot path back to the erased rows.
  It accepts cancellation during read-only planning, then deliberately defers it once tombstones are
  applied until physical erasure completes. It cannot erase packs, backups, replicas, or any other
  external copy.
- `health()` is a cheap engine snapshot suitable for an embedding application's health/metrics
  endpoint: commit and fold generation, part pressure, staged entries and bytes, WAL/fold bytes,
  cache counters and budgets, dedup-window size, retained commits, and punched blocks. It decodes no
  records or content and therefore does not claim an exact live-row count.
- `schema()` discovers the attribute names and scalar types and the independently named content
  fields present in the store. It reads part metadata, not values or content, and the writer view also
  includes unflushed records. `mayIncludeShadowedFields` is true when immutable parts contribute to
  the result because metadata-only discovery can conservatively include a field that exists only in
  a shadowed or deleted physical row; the result is descriptive and never a required global schema.

Part encoding during flush remains one uninterruptible unit. Low-level untyped invariant failures
also retain the conservative `INTERNAL` class. Those remain explicit Phase 3/4 work rather than being
simulated in JavaScript.
