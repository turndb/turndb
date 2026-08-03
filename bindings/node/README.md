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
  opaque cursor validation. The writer view includes accepted unflushed writes.
- `snapshot()` flushes all earlier accepted writes and returns an immutable reader at that exact
  actor-serialized cut. `NativeSnapshot.open()` opens the currently published manifest without a
  writer lock; `openAt()` reopens a commit still inside the bounded retention window.
- `close()` syncs by default. Passing `false` is an explicit no-sync close.
- Calls made after close refuse. When 64 operations are already queued, ordinary operations refuse
  with an overload error rather than creating an unbounded backlog.

The current slice does not yet expose cancellation/deadlines, Arrow IPC, SQL, lifecycle maintenance,
configurable queue depth, or structured error codes. Those remain explicit Phase 3/4 work rather than
being simulated in JavaScript.
