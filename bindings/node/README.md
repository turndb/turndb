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
- Attributes are an ordered array, not an object. Duplicate names and exact scalar types survive.
  Integers enter and leave as JavaScript `bigint`; binary content uses `Buffer`.
- `scan()` is the Rust structured pager. Rust owns visibility, filtering, ordering, work bounds, and
  opaque cursor validation. The writer view includes accepted unflushed writes. Content metadata
  includes `identity`, the lowercase BLAKE3 hex digest of the complete reconstructed value, when its
  record format carried one; obtaining it does not read the content.
- `snapshot()` flushes all earlier accepted writes and returns an immutable reader at that exact
  actor-serialized cut. `NativeSnapshot.open()` opens the currently published manifest without a
  writer lock; `openAt()` reopens a commit still inside the bounded retention window.
- `close()` syncs by default. Passing `false` is an explicit no-sync close.
- Calls made after close refuse. `NativeStore.open(path, { commandQueueCapacity })` sets the accepted
  backlog from 1 through 65,536; the default remains 64 and the handle reports its actual value.
  Once that many operations are queued, ordinary operations refuse with an overload error rather
  than creating an unbounded backlog. `close()` remains admissible when the queue is full.
- Rejections use `TurnDbError` with a stable `code`. The initial binding-owned classes distinguish
  `INVALID_ARGUMENT`, `BUSY`, `CLOSED`, `CONTENTION`, and `INTERNAL`; the original native error is
  retained as `cause` and the full contextual message remains available. The declared code union
  reserves `NOT_FOUND`, `CORRUPTION`, and `IO` while typed engine errors are added—unclassified core
  failures report `INTERNAL` rather than being guessed from prose.
- `compact()`, `verify()`, `punch()`, and `refold()` run on the same serialized writer actor. They
  sync and flush earlier writes before operating, so their reports cover an exact cut and their
  filesystem work stays off the event loop. `compact(true)` requests a full merge; the default uses
  the engine's measured automatic policy.
- `erase(ids)` is deliberately strong: it tombstones present ids, settles tombstones, rewrites live
  content, and purges retained manifests so this store has no snapshot path back to the erased rows.
  It cannot erase packs, backups, replicas, or any other external copy.
- `health()` is a cheap engine snapshot suitable for an embedding application's health/metrics
  endpoint: commit and fold generation, part pressure, staged entries and bytes, WAL/fold bytes,
  cache counters and budgets, dedup-window size, retained commits, and punched blocks. It decodes no
  records or content and therefore does not claim an exact live-row count.
- `schema()` discovers the attribute names and scalar types and the independently named content
  fields present in the store. It reads part metadata, not values or content, and the writer view also
  includes unflushed records. `mayIncludeShadowedFields` is true when immutable parts contribute to
  the result because metadata-only discovery can conservatively include a field that exists only in
  a shadowed or deleted physical row; the result is descriptive and never a required global schema.

The current slice does not yet expose cancellation/deadlines, Arrow IPC, SQL, backup/restore and
recovery controls, or the complete engine error taxonomy. Those remain
explicit Phase 3/4 work rather than being simulated in JavaScript.
