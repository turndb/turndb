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
- `snapshot()` flushes all earlier accepted writes and returns an immutable reader at that exact
  actor-serialized cut. `NativeSnapshot.open()` opens the currently published manifest without a
  writer lock; `openAt()` reopens a commit still inside the bounded retention window.
- `backup(path)` settles earlier actor commands, writes and fully verifies an immutable pack, and
  atomically publishes it without replacing an existing destination. `restoreBackup(pack, dir)`
  verifies every member, extracts with bounded memory, validates the staged store, and atomically
  publishes a new writable directory without overlaying any filesystem object. Safe restore reports
  `UNSUPPORTED` on a platform without an OS no-replace directory rename; the capability is exposed
  as `backupRestore`.
- `recoverManifest(path, { maxRollbackCommits })` is an offline, exclusive recovery control. It
  refuses a healthy store or live writer, validates the exact fold prefix, every part/section and
  every visible content value before publication, and defaults to permitting no rollback past the
  newest retained commit. The result reports the selected commit, rollback distance, and validation
  work; see [the recovery procedure](../../docs/recovery.md).
- `querySql()` is the richer immutable query plane. The native package deliberately includes the
  Arrow/DataFusion dependency: Rust binds typed `$1` parameters, refuses DDL/DML/session statements,
  enforces a configurable execution-memory pool (256 MiB by default), and returns a
  `NativeSqlQuery`. `schemaIpc` is a zero-batch Arrow stream; each `next()` returns one complete,
  independently decodable Arrow IPC stream in a `Buffer`. Pulls accept `timeoutMs` and `AbortSignal`,
  and close/drop cancels work not yet pulled. Calling it on a writer first publishes an exact
  actor-ordered snapshot; calling it on `NativeSnapshot` never mutates the store.
  Live queries reserve those per-query ceilings from a shared aggregate budget (1 GiB by default,
  configurable with `maxConcurrentSqlMemoryBytes`). Writer-derived snapshots share their writer's
  budget; exhaustion fails immediately and reservations release on EOF, error, cancellation, close,
  or drop. Handles expose both the limit and currently reserved bytes.
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
- `compact()`, `verify()`, `punch()`, and `refold()` run on the same serialized writer actor. They
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

The current slice does not yet expose cancellation for backup, restore, offline recovery, SQL
planning, sync, or flush, or the complete engine error taxonomy. Those remain explicit Phase 3/4
work rather than being simulated in JavaScript.
