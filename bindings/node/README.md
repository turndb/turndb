# `@turndb/native`

This is TurnDB's native Node embedding path. It is an N-API 6 addon, so it targets stable Node ABI
compatibility rather than one V8 or Node release. Each `NativeStore` owns a dedicated Rust thread and
a bounded command queue; filesystem, compression, scan, and durability-synchronization work do not run on the JavaScript
event loop.

Version 0.1.0 is published on npm. Prebuilt distribution targets are Linux x86-64 glibc and Windows
x86-64 MSVC, installed and exercised on Node 22, 24, and 26. On a supported host, install it with:

```sh
npm install @turndb/native
```

Build and exercise the source addon from the workspace with:

```sh
npm run test:dev --prefix bindings/node
```

The Tier-2 OpenTelemetry exporter is an optional subpath rather than a required SDK dependency:

```js
import { TurnDbSpanExporter } from '@turndb/native/otel';
provider.addSpanProcessor(new BatchSpanProcessor(new TurnDbSpanExporter('agent.turndb')));
```

It durably acknowledges each export by default, publishes after 512 spans or five seconds, and
always synchronizes durability and publishes on `forceFlush()` and `shutdown()`.

Provider SDKs stay optional. `traceGenAiCall(tracer, options, call)` is the thin client wrapper: it
runs any promise-returning SDK call inside a canonical `gen_ai` CLIENT span, moves input/output
message arrays onto the content-bearing attributes used by the exporter, and preserves the exact
return value or exception. This keeps OpenAI-, Anthropic-, and framework-specific adapters to a
one-call description instead of another storage mapping.

Its closed Node range is `>=22 <27`; the required Linux x86-64 matrix is Node 22, 24, and 26.
Support claims come from that tested matrix, not from N-API 6 alone; see the repository's
[support and compatibility policy](https://github.com/turndb/turndb/blob/main/docs/support-and-compatibility.md).

The package loader accepts `TURNDB_NATIVE_PATH` for development and otherwise looks for a packaged
platform prebuild. It intentionally does not fall back to `turndb-wasm`: native writer exclusion,
threads, and physical reclamation are capabilities, not implementation details that may disappear
silently.

The root package is platform-neutral and selects `@turndb/native-linux-x64-gnu` as an optional
dependency. Publication goes through the owner-gated staged release workflow.
See the [native prebuild and release contract](https://github.com/turndb/turndb/blob/main/docs/native-prebuilds.md) for clean-install
commands, artifact measurements, the glibc floor, and first-release gates.

## Semantics

- `write(ops, durable)` applies the ordered operations as one atomic batch. A successful call with
  `durable: true` has also synced the WAL. With `false` (the default), call `sync()` for a durability
  acknowledgement.
- Writer-open options `maxRecordBytes`, `maxBatchBytes`, `maxBatchRecords`, and
  `maxIdentifierBytes` configure Rust-owned admission policy. Batches are fully charged before the
  first fold mutation; byte limits use deterministic worst-case framed-WAL sizes rather than current
  dedup state. Size/count refusals are `RESOURCE_EXHAUSTED`, while malformed limits or names are
  `INVALID_ARGUMENT`. See [write admission limits](https://github.com/turndb/turndb/blob/main/docs/write-admission.md).
- Attributes are an ordered array, not an object. Duplicate names and exact scalar types survive.
  Signed/unsigned integers and UTC nanosecond timestamps enter and leave as JavaScript `bigint`;
  binary metadata and content use `Buffer`; explicit null carries its own `kind`. See
  [the scalar contract](https://github.com/turndb/turndb/blob/main/docs/field-types.md).
- `scan()` is the Rust structured pager. Rust owns visibility, filtering, ordering, work bounds, and
  opaque cursor validation. The writer view includes accepted mutations in the pending change set. Content metadata
  includes `identity`, the lowercase BLAKE3 hex digest of the complete reconstructed value, when its
  record format carried one; obtaining it does not read the content. `timeoutMs` establishes an
  absolute deadline before submission, so actor-queue time counts, and `signal` accepts an
  `AbortSignal`. Both stop cooperatively in Rust and reject with `CANCELLED`; no partial page is
  presented as success. Byte projections are limited to 32 MiB per page by default;
  `maxReconstructedBytes` overrides the ceiling as a lossless `bigint`. TurnDB never splits a row,
  admits one oversized row so paging can progress, and sets `reconstructionBudgetExhausted` when the
  continuation resumes at a row deferred by the ceiling. Metadata-only projections spend zero bytes.
  Rows resolved through a manifest revision decode only attribute/content columns used by the projection or predicates; sibling
  value, dictionary, and content-program sections remain unopened. Every successful page's `stats.io`
  reports exact operation-local part sections and fold blocks touched, cache access counts, and
  stored/raw bytes as `bigint`; concurrent read views cannot contaminate those numbers.
  `stats.resolution` reports physical immutable rows, superseded rows, deciding tombstones, and
  pending-change-set entries consumed before predicates, also as `bigint`. `maxResolutionEntries`
  bounds the sum per page (1,000,000 by default); equal-id groups stay atomic, one oversized first
  group is admitted for progress, and `budgetExhausted` explains a partial page. Empty pages may carry
  `next` after bounded progress through tombstone-only groups. See
  [projected structured scans](https://github.com/turndb/turndb/blob/main/docs/projected-structured-scan.md) and
  [structured scan I/O statistics](https://github.com/turndb/turndb/blob/main/docs/structured-scan-io.md), plus the
  [resolved-row budget contract](https://github.com/turndb/turndb/blob/main/docs/resolved-row-paging.md).
- `explainScan()` validates and prepares the same request and opaque cursor as `scan()`, then reports
  projected, predicate-only, and byte-reconstructed fields; effective bounds and budgets; and exact
  pre-resolution part/row/pending-change-set scope. It does not estimate result counts or read value/content
  columns. See [structured scan explanation](https://github.com/turndb/turndb/blob/main/docs/scan-explanation.md).
- `capabilities()` includes the language-neutral capability-contract-v2 profile (`operations`,
  `draftFormatEpoch`, reclamation, and cancellation) alongside the detailed native build facts. Query and scalar
  semantics are defined once in `docs/query-contract.md`; exact NaN payloads cross N-API through
  `floatBits`, while ordinary floats retain the ergonomic `floatValue` lane.
- Every rejection is normalized to `TurnDbError`. Its stable `code` comes from the Rust engine's
  typed cause classifier; `BUSY` and `CLOSED` are the only binding-owned states. Messages retain full
  diagnostic context but are not an API. See [error taxonomy](https://github.com/turndb/turndb/blob/main/docs/error-taxonomy.md).
- `snapshot()` publishes all earlier accepted mutations and returns a read view pinned to the resulting
  store authority. `NativeSnapshot.open()` opens the current store authority without a
  writer lock; `openAt()` reopens a positive-numbered manifest revision still inside the bounded
  retention window. The snapshot's `commit` property uses the public authority encoding: `0n`
  identifies the canonical origin and a positive value identifies that numbered manifest revision.
- `backup(path)` synchronizes and publishes earlier accepted mutations, writes and fully verifies a
  self-contained copy of the current store authority, and
  atomically installs it without replacing an existing destination. `restoreBackup(backup, path)`
  copies with bounded memory, fully validates the staged store, and atomically installs a new
  writable single-file store without overlaying any filesystem object. Safe restore reports
  `UNSUPPORTED` on a platform without an OS no-replace directory rename; the capability is exposed
  as `backupRestore`. Both accept `timeoutMs`/`AbortSignal`; cancellation before the final atomic
  link/rename removes uninstalled staging and never installs the requested destination. Backup and
  restore result `commit` fields use the same authority encoding: zero for the canonical origin,
  positive for a manifest revision.
- `recoverManifest(path, { maxRollbackCommits, timeoutMs, signal })` is an offline, exclusive
  manifest-promotion control. It
  refuses when the current MANIFEST is intact or another writer is open, validates the exact fold prefix, every part/section and
  every visible content value before publication, and defaults to permitting no rollback past the
  newest retained manifest revision. The result reports the selected manifest revision, rollback distance, and validation
  work. Cancellation during validation leaves the damaged manifest and retained history unchanged;
  promotion is the final uninterruptible boundary. See
  [the manifest-promotion procedure](https://github.com/turndb/turndb/blob/main/docs/manifest-promotion.md).
- `querySql()` is the richer immutable query plane. The native package deliberately includes the
  Arrow/DataFusion dependency: Rust binds typed `$1` parameters, refuses DDL/DML/session statements,
  enforces a configurable execution-memory pool (256 MiB by default), and returns a
  `NativeSqlQuery`. Query options and pull options accept `timeoutMs` and `AbortSignal`; planning
  cancellation drops the unfinished DataFusion future and releases its memory reservation.
  `schemaIpc` is a zero-batch Arrow stream; each `next()` returns one complete, independently
  decodable Arrow IPC stream in a `Buffer`, and close/drop cancels work not yet pulled. Calling it
  on a writer first publishes the pending change set in actor order and opens a read view; calling
  it on `NativeSnapshot` never mutates the store.
  Live queries reserve those per-query ceilings from a shared aggregate budget (1 GiB by default,
  configurable with `maxConcurrentSqlMemoryBytes`). Writer-derived read views share their writer's
  budget; exhaustion fails immediately and reservations release on EOF, error, cancellation, close,
  or drop. Handles expose both the limit and currently reserved bytes.
- `sync({ timeoutMs, signal })` and `flush({ timeoutMs, signal })` include actor queue time. Sync
  observes interruption only before its durability boundary (delayed authority acknowledgement when
  needed, then WAL fsync); flush also checks its unpublished planning and digest
  phases and removes a staged part on cancellation. Neither reports cancellation after its final
  durability/publication boundary begins.
- `close()` performs synchronization by default, publishes the pending change set, settles the
  store, removes its WAL sidecar, and releases the handle. Passing `false` releases the handle
  without synchronization, publication, or settlement.
- Calls made after close refuse. `NativeStore.open(path, { commandQueueCapacity })` sets the accepted
  backlog from 1 through 65,536; the default remains 64 and the handle reports its actual value.
  Once that many operations are queued, ordinary operations refuse with an overload error rather
  than creating an unbounded backlog. `close()` remains admissible when the queue is full.
- Rejections use `TurnDbError` with a stable `code`. The initial classes distinguish
  `INVALID_ARGUMENT`, `BUSY`, `CLOSED`, `CONTENTION`, `CANCELLED`, write/SQL
  `RESOURCE_EXHAUSTED`, `UNSUPPORTED`, and `INTERNAL`; the original native error is retained as `cause`
  and the full contextual message remains available. The declared code union reserves `NOT_FOUND`,
  `CORRUPTION`, and broader `IO` use while typed engine errors are added—unclassified core failures
  report `INTERNAL`.
- `compact()` (part merge), `verify()`, `contentPunch()`, `refold()`, and `backup()` run on the same serialized writer actor. They
  perform synchronization, publication, and settlement for earlier accepted mutations before operating, so their
  reports cover the resulting current store authority and their
  filesystem work stays off the event loop. `compact(true)` requests a full merge; the default uses
  the engine's measured automatic policy. Each accepts queue-inclusive `timeoutMs` and
  `AbortSignal` options backed by Rust cooperative checkpoints. Cancelled part-merge/refold staging
  is removed; content punch retains safe resumable progress. See
  [lifecycle cancellation and deadlines](https://github.com/turndb/turndb/blob/main/docs/lifecycle-control.md).
- `compactBounded({ maxInputParts, maxInputRows, maxInputBytes }, options)` publishes one contiguous
  merge within all three exact physical-input limits. It reports the executed plan, output bytes,
  and merge statistics; an insufficient budget is `RESOURCE_EXHAUSTED`, never an implicit overrun.
  Only a step covering every part referenced by the current manifest revision drops tombstones. See
  [bounded incremental part merge](https://github.com/turndb/turndb/blob/main/docs/bounded-part-merge.md).
- `spaceUsage(options)` classifies every container member exactly once in the literal `live`, `retainedOnly`, or
  `unclassified` field, with logical bytes everywhere and container allocation/free bytes where the platform can prove
  them. `estimateCompactionSpace(budget, options)` and `estimateRefoldSpace(options)` add exact source
  facts and explicitly non-binding stage estimates. TurnDB supplies evidence; the embedding
  application chooses admission and reserve policy. See
  [maintenance space accounting and preflight](https://github.com/turndb/turndb/blob/main/docs/maintenance-space.md).
- `erase(ids)` is deliberately strong when at least one requested slot resolves to a record: it
  tombstones those record IDs, performs a total merge when needed, writes a new fold generation when
  parts remain to rebuild, and purges retained manifest revisions so this store has no
  retained-revision path back to the erased rows. An all-absent request performs no transition;
  standalone `refold()` likewise performs no transition when the current authority references no parts.
  It accepts cancellation during read-only planning, then deliberately defers it once tombstones are
  applied until physical erasure completes. It cannot erase backups, replicas, or any other
  external copy.
- `health()` is a cheap engine observation suitable for an embedding application's health/metrics
  endpoint: current store authority (`commit: 0n` for the canonical origin, positive for a manifest
  revision) and any referenced fold generation, part pressure, pending-change-set entries and bytes, WAL/fold bytes,
  cache counters and budgets, dedup-window size, retained manifest revisions, and punched blocks. It decodes no
  records or content and therefore does not claim an exact resolved-record count.
- `metrics()` returns monotonic `bigint` outcomes and nanosecond totals for WAL replay, manifest promotion, and writer
  lifecycle work. It is handle-local and pull-based; actor queue wait is deliberately separate from
  core execution time. Verification reports a dedicated corruption-failure subset without conflating
  cancellation or I/O. Folded-content counters expose exact piece hits/logical/novel bytes, and
  `partDistribution(options)` reports byte/row order statistics for parts referenced by the current
  manifest revision, or an empty report at the canonical origin. See
  [pull-based operation metrics](https://github.com/turndb/turndb/blob/main/docs/operation-metrics.md).
- `contentLiveness(options)` walks visible record programs and fold headers to separate unique live-content-reachable
  piece bytes, dead bytes stranded inside mixed compressed blocks, and wholly unreferenced block
  payload eligible for content punch or refold. It requires an empty pending change set and accepts cancellation; see
  [content liveness and reclamation](https://github.com/turndb/turndb/blob/main/docs/content-liveness.md).
- `lifecycleEvents(afterSequence, limit)` reads a bounded, non-destructive journal of stable lifecycle
  operation/outcome/error-class/duration facts. Sequence gaps and cumulative eviction are explicit,
  so independent exporters can detect loss; see
  [bounded lifecycle events](https://github.com/turndb/turndb/blob/main/docs/lifecycle-events.md).
- Structured scan stats include `durationNs`; SQL stats separately expose
  `planningDurationNs` and cumulative active `executionDurationNs`. Read-only SQL `EXPLAIN` streams
  DataFusion's plan as ordinary Arrow IPC. TurnDB supplies evidence and leaves slow thresholds to the
  consumer.
- Native `open()` also accepts fold/part cache budgets, stored/decoded atomic-frame admission,
  persistent directory-entry/WAL-frame/fold-block count admission, and the write-side block target,
  segment, compression level, and compression-worker policy. Independently opened read views accept
  the same read ceilings; writer-created read views inherit them. `health()` reports the effective
  writer values, including current physical WAL frames; see
  [resource budgets and overload](https://github.com/turndb/turndb/blob/main/docs/resource-budgets.md) and
  [persistent object-count admission](https://github.com/turndb/turndb/blob/main/docs/object-admission.md).
- `schema()` discovers the attribute names and scalar types and the independently named content
  fields present in the store. It reads part metadata, not values or content, and the writer view also
  includes pending record versions. `mayIncludeShadowedFields` is true when immutable parts contribute to
  the result because metadata-only discovery can conservatively include a field that exists only in
  a shadowed or deleted physical row; the result is descriptive and never a required global schema.

Part encoding during flush remains one uninterruptible unit. Low-level untyped invariant failures
also retain the conservative `INTERNAL` class. Those remain planned engine work rather than being
simulated in JavaScript.

The external [reference-consumer qualification](https://github.com/turndb/turndb/blob/main/docs/reference-consumer-qualification.md)
uses this public surface for both linked application/AI telemetry and a non-telemetry build pipeline.
It is executable evidence for the generic record model and this binding's public surface, not an
additional package API or core schema.
