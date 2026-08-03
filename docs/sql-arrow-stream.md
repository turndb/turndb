# Read-only SQL and Arrow IPC streaming

TurnDB's richer embedded query boundary lives in Rust. A binding supplies SQL text, positional typed
values, and a resource option; Rust fixes the immutable snapshot, validates a read-only plan, executes
the storage-backed columnar lens, and encodes results. This keeps DataFusion and Arrow on the database
side of the Rust/JavaScript seam while allowing any Arrow-capable consumer to interpret the result.

Every query gets an isolated DataFusion session containing one table named `records`. Its columns are
the collision-safe schema produced by `query::Lens`: `id`, named content (`body` or
`content.<name>`), and self-described typed attributes. SQL uses `$1`, `$2`, and so on. Parameters are
explicitly one of null, UTF-8 string, signed i64, f64, boolean, or binary; bindings must not interpolate
them into SQL text. DDL, DML, COPY, PREPARE/EXECUTE, SET, and other session statements are rejected by
DataFusion's typed plan validator.

`SqlQuery` is pull-based. `schema_ipc()` is a complete zero-batch Arrow IPC stream, so an empty query
still communicates its exact schema. Each `next()` result is another complete Arrow IPC stream with
the same result schema and exactly one record batch. Making batches independently decodable costs a
small schema/dictionary envelope per pull, but it gives simple ownership, bounded residency, clean
retry boundaries, and interoperability without requiring a JavaScript stream adapter to preserve an
incremental IPC writer's hidden state. End-of-stream is stable.

The native Node binding maps this to `NativeSqlQuery.schemaIpc` and
`await NativeSqlQuery.next({ timeoutMs, signal })`. The returned `ipc` is a native `Buffer`; no
base64, JSON, or row-by-row N-API conversion occurs. Only one pull may be in flight on a query handle.
Closing the handle drops its DataFusion stream. A cancelled or timed-out pull also drops the stream,
returns `CANCELLED`, and cannot be resumed as though execution state were unchanged.

Snapshot ownership is explicit:

- `NativeSnapshot.querySql` queries that immutable manifest and never publishes or mutates anything.
- `NativeStore.querySql` first actor-serializes a snapshot, syncing and flushing accepted writes, then
  releases the writer actor and executes against the resulting immutable reader.

This gives writer queries read-your-writes behavior without letting concurrent DataFusion tasks touch
the mutable store. It also makes the publication cost visible in the API contract; callers issuing
many queries should retain a snapshot instead of repeatedly querying through the writer.

Each query uses a separate DataFusion execution-memory pool, 256 MiB by default and configurable with
`maxMemoryBytes`. Resource exhaustion is returned as `RESOURCE_EXHAUSTED`. The value bounds tracked
execution allocations, not total process memory: TurnDB's fold/part caches are bounded separately,
the returned IPC buffer belongs to the caller, and DataFusion itself documents allocations that do
not participate in its pool. An aggregate budget shared by concurrent queries remains future work and
is reported as such rather than implied by the per-query option.

Query statistics accompany each batch and remain available from `stats()`: storage rows and batches,
attribute columns decoded, fold reads, rows rejected or hidden, batches skipped, and duplicate
attribute occurrences shadowed by the flat columnar view. They describe the TurnDB table scan rather
than pretending to be complete DataFusion operator metrics.
