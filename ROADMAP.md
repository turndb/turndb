# TurnDB roadmap

TurnDB's target is not to become CommandSuite's trace database. Its target is to become a credible,
general-purpose, embedded, content-addressed columnar store for trace-shaped data. Projects such as
CommandSuite are demanding qualification workloads: they should expose missing database capabilities
without contributing their product vocabulary or schemas to the storage engine.

This roadmap is organized around maturity gates rather than calendar dates. The order matters: the
record model constrains the query surface, the query surface constrains the bindings, and those pieces
must be sound before a consumer can demonstrate production readiness. Work within a phase may happen
in parallel, but passing a later gate depends on passing the earlier ones.

TurnDB is currently pre-1.0 and its format is not frozen. That is an opportunity to make the record and
content model right before compatibility turns today's choices into permanent promises.

## Progress on `codex/turn-maturity`

- `553c59f` implements the first Phase-1 format slice: general named content in semantic records,
  revision-2 WAL and sparse part columns, mixed-version reads, lifecycle propagation, and lazy
  `content.<name>` query projection.
- `docs/embedding-contract.md` records the Phase-0 architectural, consistency, compatibility,
  capability, limit, and Rust-versus-binding decisions, including current gaps rather than presenting
  them as completed guarantees.
- The columnar lens is separated from DataFusion as the first Phase-2 seam. Writer memtable visibility
  is now available through a feature-independent structured pager with checked Rust-owned cursors,
  exact typed predicates, bounded live-record examination, reverse paging, and opt-in named-content
  reconstruction. Pushing that API down into selected physical columns is the next query-core step.
- The first Phase-3 native Node slice lives in `bindings/node`: a stable N-API 6 addon, one bounded
  Rust store actor per writer, Promise operations, atomic batches with explicit durability, native
  buffers and bigint, the Rust structured pager, explicit capability reporting, and no silent WASM
  fallback. Writer-created immutable cuts and independently opened live/retained reader snapshots now
  carry stable multi-page reads without taking a writer lock. `TurnDbError` classifies binding
  validation, overload, closed handles, and typed writer contention without consumer-side prose
  matching. Compaction, whole-store verification, in-place reclamation, refold, and physical record
  erasure are actor operations rather than CLI dependencies. A constant-work generic health snapshot
  now exposes write-state, part/fold growth, cache, retention, and reclamation facts without choosing
  a telemetry backend. Feature-independent schema discovery now inventories the separate attribute
  and named-content namespaces from part metadata plus the live writer memtable, preserving observed
  scalar types without reading values or fold blocks. Its explicit conservative-result flag admits
  when shadowed physical rows may contribute names. The Node binding exposes the same structured
  contract. Revision-3 WAL and parts now persist exact whole-value BLAKE3 identities computed during
  ingest, expose them through metadata-only structured scans and Node, preserve them through replay
  and streaming compaction, and report them unavailable for legacy values instead of confusing piece
  or program hashes with byte identity. The Node package remains a source prototype, not yet a
  production prebuild matrix. Its accepted command backlog is now a validated per-store open option
  with an explicit default and maximum, deterministic overload behavior, and an actual-capacity
  handle property. Structured scans now have a shareable Rust cancellation token and absolute
  deadline; Node maps `AbortSignal` and queue-inclusive `timeoutMs` onto them and classifies both as
  `CANCELLED` without returning partial success. Structured pages now also enforce a configurable
  whole-row reconstruction ceiling (32 MiB by default), expose when it caused a partial page, admit
  one oversized row for progress, and resume before rather than after a deferred row.

## Product boundary

TurnDB should own:

- Durable storage, acknowledgement, and crash recovery.
- Self-describing typed fields.
- First-class content-addressed values.
- Record versioning, tombstones, and visibility.
- Projection, filtering, ordering, and pagination.
- Query-engine integration without surrendering storage semantics to the query engine.
- Compaction, erasure, verification, backup, and recovery.
- Native and portable embedding interfaces with explicit capability differences.

Consumers should own:

- OpenTelemetry and other external semantic-convention mappings.
- The domain meaning of activities, generations, messages, tool calls, and similar concepts.
- Correlation policy beyond generic field predicates and joins.
- Authorization, tenancy, and redaction policy.
- Product APIs, data-transfer objects, and UI behavior.
- Decisions about what data is retained and for how long.

OpenTelemetry can provide useful recommended conventions through an adapter or companion package, but
TurnDB core should neither require OTel nor encode its current conventions into the file format.

## Architectural direction

The intended layering is:

```text
consumer application
        |
consumer/semantic adapter (for example, OTel mapping)
        |
language binding and ergonomic API
        |
structured query API ---- optional SQL/DataFusion adapter
        |                         |
        +------ columnar lens ----+
                    |
          record and content store
                    |
             WAL / parts / fold
```

The fold, record visibility, projection behavior, and durability semantics belong to Rust. JavaScript
should not recreate byte ordering, cursor rules, integer types, transaction semantics, or content
resolution. Conversely, product-specific normalization and correlation should not migrate into Rust
merely because the native binding is written there.

The existing separation between TurnDB's columnar lens and its DataFusion adapter is the right
direction. DataFusion is a planner and execution engine over TurnDB's queryable view; it is not the
definition of TurnDB's storage or visibility semantics.

## Phase 0: define the contract

Before widening the implementation, document the promises an embedder can depend on.

### Deliverables

- An architectural decision record defining TurnDB as embedded, content-addressed, columnar, and
  single-writer with concurrent readers.
- A precise consistency model covering:
  - What `commit`, `sync`, and `flush` mean.
  - The durability acknowledgement point.
  - When an acknowledged record becomes visible to point reads and queries.
  - Read-your-writes behavior.
  - Snapshot isolation and the effect of compaction on existing readers.
- An ownership boundary between the storage core, query lens, query-engine adapters, bindings, and
  consumer adapters.
- A compatibility policy covering on-disk format versions, API versions, upgrades, downgrade refusal,
  and migration tooling.
- A native-versus-WASM capability matrix. Missing facilities must be reported, not silently emulated
  with weaker guarantees.
- Initial resource and limit semantics: maximum record size, maximum batch size, bounded query memory,
  and cancellation behavior.

### Maturity gate

Two independent prospective consumers can design integrations from the written contract without
depending on private implementation details or contradictory assumptions about durability and query
visibility.

## Phase 1: generalize the record and content model

The current privileged `body` should become one instance of a more general content value. A logical
record should have an identity, arbitrary typed fields, and zero or more named content-addressed
values:

```text
record
  id
  fields
    timestamp -> timestamp
    trace_id  -> string
    tokens    -> integer
    sampled   -> boolean
    ...
  content
    request   -> content reference
    response  -> content reference
    raw_event -> content reference
    ...
```

This is not a static trace schema. Field and content names come from the consumer. Self-describing does
not mean untyped or unconstrained: the physical value types and their encodings remain a deliberate,
versioned part of TurnDB.

Named content must remain structurally content-addressed. A content field in a part is a reference
program into the shared fold, not an opaque byte column with deduplication added after the fact. This
keeps content identity involved in writing, querying, compaction, integrity checking, deletion, and
erasure.

### Deliverables

- Multiple named content fields per record rather than one privileged body.
- Content references represented directly in parts and manifests.
- APIs that expose a content reference's identity, logical length, and presence without reconstructing
  its bytes.
- Deduplication across records, content names, record families, and consumers sharing a store.
- A defined initial field type system, including at least:
  - UTF-8 string.
  - Signed and unsigned integer with exact widths or ranges.
  - Floating point with the existing byte-exact guarantees for special values.
  - Boolean.
  - Binary.
  - Timestamp with an explicit unit and timezone interpretation.
- A deliberate decision about structured values. Lists and maps may become native columnar types, or
  they may initially be explicitly encoded content; the API must never blur the two representations.
- Defined behavior for:
  - Missing versus null.
  - Duplicate fields and field ordering.
  - The same field name appearing with different types.
  - Empty content versus absent content.
  - Field-name and content-name validation.
- Schema discovery across parts without treating a discovered schema as a required global schema.
- A versioned migration from the current `body` representation.
- Byte-exact and content-identity tests for every content position and supported field type.

### Maturity gate

Mixed record families coexist in one store without reserved domain schemas. Identical content
deduplicates regardless of the record family or content name that references it, and reconstruction
remains byte-exact.

## Phase 2: make the columnar lens a stable query core

TurnDB already has an important architectural seam: the lens determines how stored bytes become
projectable Arrow columns, while the DataFusion table provider teaches an external engine how to ask
for them. That separation should become a supported internal and public contract.

The structured query API is the primary embedded interface. SQL is a valuable optional interface for
analytics and uncommon queries, but consumers should not need to express every hot-path operation as a
SQL string.

### Deliverables

A storage-native scan contract supporting:

- Arbitrary field projection.
- Named content projection.
- Returning content references without resolving their bytes.
- ID and field predicates.
- Existence, missing, and null predicates.
- Stable ordering.
- Forward and reverse cursor pagination.
- Bounded, streaming result batches.
- Cancellation, time limits, and memory limits.
- Schema discovery and query explanation.
- Query statistics that report rows, sections, bytes, and fold blocks touched.

The implementation must establish these invariants:

- Metadata-only queries perform zero content reconstruction and zero fold reads.
- A query reads only projected and predicate-bearing column sections where the format permits it.
- Committed but not-yet-flushed records participate in queries under the documented consistency model.
- The newest visible record version is considered before applying predicates. A predicate cannot reveal
  an older version rejected or hidden by a newer version or tombstone.
- Cursor behavior remains complete and duplicate-free while writes and compactions occur.
- Query results have the same value semantics as point reads, including exact integers and unusual
  floating-point values.
- Query execution is bounded even when projected content values are individually large.

Arrow should be separable from DataFusion so embedders can use the columnar lens without carrying the
entire SQL engine. DataFusion remains an optional native feature layered over the same lens, with
predicate and projection pushdown tested rather than assumed.

### Maturity gate

A consumer can implement trace listing, filtering, correlation, and stable pagination without fetching
full payloads, forcing a flush for query visibility, or maintaining a second searchable metadata
database.

## Phase 3: provide a production native Node interface

The native Node binding should become the supported server-side embedding path. The WASM build remains
valuable as a portable, capability-constrained package, not as a silent fallback from the native
guarantees.

The current package deliberately omits SQL and assumes that point lookup plus ID paging is sufficient.
That is a useful lightweight profile, but not a sufficient general production interface for consumers
that need projected and filtered reads over live data.

### Deliverables

- A native N-API binding, preferably using `napi-rs`.
- Prebuilt artifacts for a declared OS, architecture, and libc support matrix.
- Stable N-API compatibility rather than builds coupled to each Node release.
- A dedicated Rust store actor or bounded worker execution model.
- Promise-based operations that do not perform compression, compaction, or large reconstruction on
  Node's event-loop thread.
- Native `Buffer` transport instead of JSON/base64 for content.
- Exact JavaScript `bigint` conversion for integer fields.
- A structured scan API backed by the Rust query core.
- A streaming Arrow IPC interface for columnar consumers.
- An ergonomic object-row wrapper for smaller application queries.
- Optional parameterized SQL backed by DataFusion.
- Abort-signal integration, cancellation, backpressure, and bounded queues.
- Stable structured error classes and machine-readable error codes.
- Native access to snapshots, compaction, erasure, refold, verification, and recovery diagnostics.
- Clean shutdown semantics that distinguish closing, syncing, and abandoning unacknowledged work.

A representative API shape is:

```ts
await db.commit(records, { durability: "sync" });

const page = await db.scan({
  select: ["id", "timestamp", "model", "content.request"],
  where: [{ field: "trace_id", op: "eq", value: traceId }],
  orderBy: [{ field: "timestamp", direction: "asc" }],
  cursor,
  limit: 100,
});

const request = await db.readContent(page.rows[0]["content.request"]);
```

The exact syntax is not the commitment; the ownership is. Rust constructs ranges and cursors, applies
visibility, preserves types, and resolves content. JavaScript supplies a query specification and maps
the generic result into its product model.

The WASM interface should have an explicit package or entry point and a tested capability manifest. A
production writer must never silently fall back from native OS locking to WASI's unenforced
single-writer convention. The same applies to threads, hole punching, and any future platform
facility.

### Current prototype evidence

The first native slice implements open, atomic ordered write/delete batches, sync, flush, immutable
current/retained snapshots, structured scan, named-content reconstruction, and close. One dedicated
Rust thread owns the writer; a bounded per-store command queue defaults to 64, permits an explicit
capacity from 1 through 65,536, and refuses overload rather than accumulating unbounded Promises.
Node integration tests load the addon and cover exact `i64::MIN` bigint, NaN,
ordered duplicate attributes, named and empty content, projection, predicates, paging/cursor misuse,
snapshot isolation/publication, retained commits, deletion, and close refusal.

Measured on Linux x86-64 from the 2026-08-02 tree with the workspace release profile and GNU `strip`:

| native build | stripped addon | gzip -9 |
|---|---:|---:|
| structured core, no Arrow/DataFusion | 2,947,024 bytes | 1,211,755 bytes |
| compiled with `turndb-node/sql` | 7,818,112 bytes | 3,181,197 bytes |

The full feature build therefore adds 4,871,088 installed bytes and 1,969,442 gzip bytes on this
host. This measurement does **not** prove a SQL API—the prototype does not expose one yet—and does
not include npm metadata or per-platform duplication. It does show that keeping the structured core
profile is useful while the additional query-engine dependency is affordable when its capabilities
are actually exposed.

Remaining Phase-3 gaps are prebuilt platform artifacts, Arrow IPC and SQL, cancellation/deadlines for
long-running lifecycle operations, a complete typed engine error taxonomy, backup/restore and recovery
controls, and measured event-loop/query overhead under a representative mixed workload.

### Maturity gate

A long-running Node application can use all documented durability, query, and lifecycle operations
without invoking a CLI, blocking the event loop, losing integer precision, or depending on one process
to compensate for a missing OS writer lock.

## Phase 4: complete the lifecycle machinery

A database is not operationally mature when it can merely write and query. It must remain healthy
through years of writes, deletes, crashes, upgrades, and changing retention decisions.

### Deliverables

- Bounded incremental compaction with a measurable work budget.
- Explicit full compaction for controlled maintenance windows.
- Physical erasure of unreferenced content.
- Refold fallback where hole punching is unsupported.
- Safe interruption and restart of compaction and erasure.
- Consistent snapshot and backup APIs.
- Restore validation and documented restore procedures.
- Offline and, where safe, online integrity verification.
- Corruption localization by WAL frame, manifest, part, section, fold block, and content object.
- Recovery behavior that refuses ambiguity rather than silently discarding data.
- Disk-space estimation before maintenance operations that may temporarily duplicate data.
- Format migration tooling with resumability and preflight checks.
- Generic deletion and erasure primitives that accept records selected by the consumer. TurnDB may
  execute a retention decision, but it should not invent the consumer's retention policy.

### Maturity gate

TurnDB survives crash injection throughout the write and maintenance paths, can verify and back up a
live store, restores into an equivalent queryable state, and demonstrates bounded disk usage under a
sustained write-and-retention workload.

## Phase 5: expose production observability and control

TurnDB should expose evidence about its own behavior without prescribing the telemetry system to which
that evidence is sent.

### Deliverables

- Metrics and snapshots covering:
  - Commit, sync, flush, and recovery latency.
  - WAL and in-memory write-state size.
  - Part count and part-size distribution.
  - Query rows examined and returned.
  - Projected sections and bytes read.
  - Fold reads and reconstructed bytes.
  - Compression and compaction time.
  - Deduplication ratio.
  - Live, dead, and reclaimable content.
  - Verification and corruption failures.
- Structured lifecycle and health events.
- Slow-query reporting and query plans suitable for diagnosis.
- Configurable resource budgets and overload behavior.
- A health snapshot API suitable for an embedding application's health endpoint.
- Hooks through which a consumer can export OpenTelemetry without introducing OTel into the storage
  core.

### Maturity gate

An embedding application can diagnose latency, disk growth, failed maintenance, contention, and query
amplification without parsing prose logs or inspecting TurnDB's files manually.

## Phase 6: qualify through reference consumers

Reference consumers prove whether TurnDB's abstractions are sufficient. They are not invitations to
move consumer concepts into the database.

At least one reference adapter should exercise several interrelated record families in one store:

- Activity-like events.
- Model and generation calls.
- Tool calls.
- Raw provider exchanges.
- Capture and ingestion diagnostics.
- Large request and response content.

The workload should exercise:

- Shared content across record families.
- Metadata-only timelines.
- Correlation through arbitrary typed fields.
- Pagination during active ingestion.
- Projection of selected content fields.
- Late-arriving related records.
- Atomic batches and durable acknowledgement.
- Deletion followed by physical erasure.
- Restart and crash recovery.
- Sustained retention and compaction.
- Backup, restore, and upgrade.

An OpenTelemetry adapter is a useful reference, but it must live outside the storage core. At least one
deliberately non-OTel workload should also pass the same qualification suite so generality is
demonstrated rather than asserted.

### Maturity gate

A consumer can replace its trace-specific persistence machinery with TurnDB without:

- Mirroring searchable metadata into another database.
- Fetching large payloads for list or health queries.
- Inventing its own compaction, verification, or erasure system.
- Depending on undocumented file-format behavior.
- Adding consumer-specific concepts to TurnDB core.

## Phase 7: stabilize the format, APIs, and releases

This phase turns demonstrated behavior into promises that downstream projects can safely adopt.

### Deliverables

- A complete public format specification.
- A supported-platform and capability policy.
- Semantic-versioning policies for the Rust and Node APIs.
- Format-version compatibility fixtures retained across releases.
- Upgrade fixtures containing stores written by older versions.
- Property, fuzz, corruption, and deterministic crash testing for the generalized record model.
- Differential tests between point reads, structured scans, and DataFusion queries.
- Cross-runtime tests proving native and WASM implementations read the same stores byte-exactly where
  their capability sets overlap.
- Performance baselines for ingestion, durable commit, metadata scans, content reconstruction,
  compaction, verification, recovery, and open time.
- Published artifact sizes and measured costs for lightweight and full-featured packages.
- An operational handbook covering backup, recovery, maintenance, upgrades, and failure diagnosis.
- A security review of file parsing, malformed inputs, binding boundaries, and resource exhaustion.

### 1.0 gate

TurnDB 1.0 should mean all of the following:

- The format is stable, documented, and migratable.
- A successful durable commit survives the documented crash model.
- Query visibility, ordering, and pagination semantics are stable.
- Content addressing remains intact through queries, compaction, deletion, and erasure.
- Metadata-only projections are proven not to reconstruct content.
- The native Node interface is supported for production embedding.
- WASM's reduced capabilities are explicit, queryable, and tested.
- A consumer can operate the database without private knowledge from its authors.

## Cross-cutting verification

Every phase needs evidence proportional to the claim it introduces. The following work should grow
with the implementation rather than arrive only before 1.0:

- Deterministic crash simulation for each new acknowledgement or maintenance transition.
- Corruption mutation tests for each new on-disk structure and parser.
- Property tests for field typing, schema discovery, content references, cursors, and record visibility.
- Completeness tests for pagination: no missing or duplicated eligible rows across page boundaries.
- Nearest-valid and nearest-invalid tests for every new validation rule.
- Differential query tests against a simple reference evaluator.
- Tests proving projections do not touch unrequested sections or fold blocks.
- Benchmarks that separate storage-engine time, query-engine time, binding overhead, and application
  conversion overhead.
- Long-running workloads that mix ingestion, reads, snapshots, compaction, deletion, and restart.

Performance thresholds should be chosen from measurements on representative corpora. The roadmap does
not invent latency, throughput, or artifact-size targets before those measurements exist.

## Explicit non-goals

This roadmap does not turn TurnDB into:

- An OpenTelemetry collector or a canonical OTel storage schema.
- A CommandSuite service, API server, or product backend.
- A network daemon required for ordinary use.
- A distributed database, cluster, or consensus system.
- An authorization or tenant-policy engine.
- A scheduler that decides retention policy for its consumers.
- A replacement query planner built in-house when Arrow and DataFusion meet the requirement.
- A second metadata index whose correctness can drift from the content-addressed record store.

A generic companion process may eventually be useful for isolation or non-Node consumers, but it should
remain optional and speak TurnDB operations. It should not invent a domain ingestion protocol or make a
daemon mandatory for an embedded database.

## Recommended first implementation cycle

The first cycle should answer the expensive architectural questions before broad implementation:

1. Specify the generalized field and named-content record model, including the type system and format
   migration strategy.
2. Prototype the structured scan API, particularly projection, cursor behavior, and visibility of
   committed but unflushed records.
3. Prototype a native `napi-rs` binding carrying Arrow and DataFusion on the intended deployment
   platforms.
4. Measure native artifact size, installation complexity, query overhead, and event-loop behavior
   against the current WASM package.
5. Exercise the prototypes with a mixed-record reference workload without adding its vocabulary to
   TurnDB.
6. Record the decisions and rejected alternatives before committing to a new format version.

Once those decisions are supported by evidence, implementation should proceed in this dependency
order:

```text
general record and named-content format
                  |
                  v
       query core and visibility
                  |
                  v
        production Node binding
                  |
                  v
      lifecycle and operations APIs
                  |
                  v
    reference-consumer qualification
                  |
                  v
       format and API stabilization
```

The central discipline throughout is that a consumer may reveal a capability TurnDB lacks, but it does
not get to name that capability in product terms. Projection over named content, typed correlation,
durable batch visibility, stable pagination, and physical erasure belong in TurnDB. Generation traces,
activity timelines, capture health, and provider exchanges belong to the applications built on it.
