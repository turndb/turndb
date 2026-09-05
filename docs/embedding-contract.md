# TurnDB embedding contract

Status: current design contract for the pre-1.0 implementation. Sections marked **current gap** describe
work that must land before the corresponding promise is considered supported.

This document turns TurnDB's product boundary into decisions an embedder can design
against. It is intentionally consumer-neutral: OpenTelemetry, production trace platforms, and any
other record vocabulary belong in adapters above this contract.

## 1. Architecture decision

TurnDB is an embedded, content-addressed, columnar, single-writer store with concurrent read
views. **Single-writer is enforced by the OS on Unix and is the embedder's obligation on
`wasm32-wasip1`** — see [FORMAT.md](../FORMAT.md#store-shape) and the exclusion row in
[5. Platform capabilities](#5-platform-capabilities) below.

- Rust owns storage semantics: byte ordering, record visibility, durability, content resolution,
  cursor construction, limits, and cancellation state.
- The columnar lens is the primary scan substrate. It is available with the `columnar` Cargo feature
  and depends on Arrow, but not DataFusion.
- DataFusion is the optional `sql` adapter over that lens. SQL is not the definition of visibility or
  value semantics.
- Native language bindings call Rust operations. They do not reimplement record ordering, filters,
  cursors, integer conversions, transactions, or content addressing.
- WASM is a portable, capability-reduced build. It is never a silent fallback from native locking,
  threading, or erasure guarantees.
- Consumers own semantic mappings, correlation conventions, authorization, tenancy, redaction, and
  retention policy.

The dependency direction is:

```text
consumer model -> binding -> structured scan / optional SQL -> columnar lens
                                                        -> record store -> WAL / parts / fold
```

No arrow in that diagram may point back toward a consumer model.

## 2. Record and content contract

A logical record has one non-empty UTF-8 id, zero or more named content values, and an ordered list of
typed attributes. Content names are non-empty UTF-8 map keys, unique within a record, and canonicalized
by UTF-8 byte order. Attribute order and duplicate keys are preserved.

The name `body` is a convenience convention, not a privileged storage field. Other names are
projected as `content.<name>`. Missing content is distinct from present empty content.

Content remains structurally content-addressed: part programs reference the shared fold's BLAKE3
piece identities. Part merge rewrites programs and columns, not carved piece bytes. Identical pieces
deduplicate across record ids, content names, record families, and consumers using the same store.

The current draft part and WAL persist BLAKE3 of each complete reconstructed value. The digest is computed
over ingest spans without concatenating them, survives WAL replay and part merge, and is projected
with presence, length, and piece count without reading fold blocks. Every stored content occurrence
has this identity; there is no unidentified representation. See
`docs/record-model.md`, `docs/content-identity.md`, and `FORMAT.md`.

The scalar type system includes unsigned u64, arbitrary binary metadata,
signed Unix-nanosecond timestamps interpreted in UTC, and explicit null in addition to UTF-8 string,
signed i64, bit-exact f64, and boolean. Missing means the key has no occurrence; explicit null is a
real ordered attribute occurrence. Structured `attr_exists` and equality-to-null preserve the
distinction. The Arrow lens exposes `key#null` as a nullable boolean presence marker (`true` explicit,
Arrow null missing), because an Arrow `Null` array cannot represent that distinction. See
`docs/field-types.md`.

## 3. Acceptance, durability, publication, and visibility

TurnDB distinguishes acceptance, durability acknowledgement, publication, and publication
acknowledgement.

### `put`, `put_record`, `delete`, and `apply`

These accept changes into writer order, append their replay input to the WAL, and update the pending
change set. `apply` is an atomic group for WAL replay: replay applies every member only when its
batch marker is intact. A successful acceptance call is
not a durability acknowledgement.

Before acceptance, the writer applies its per-open `WriteLimits`. The deterministic charge is a
worst-case complete WAL frame with every piece occurrence treated as novel; a batch is all member
frames plus its completion marker. Record/batch byte ceilings, batch member count, and UTF-8 identifier
bytes are inclusive and configurable. Complete validation precedes any fold mutation, and WAL replay
does not reapply newly chosen *write* policy to accepted WAL frames. The separate per-open
`ReadLimits` is deliberately applied during replay before allocating a persisted frame; reopening a
legitimate large-frame store under a stricter profile returns resource exhaustion and does not
discard it. See `docs/write-admission.md` and `docs/read-admission.md`.

The writer provides read-your-writes immediately after successful acceptance. Point reads and id
scans through that writer include its pending change set, including pending tombstones. These reads may therefore see
data that will disappear if the process exits before the `sync` durability acknowledgement.

### `sync`

`sync` is the durability-synchronization and acknowledgement point. When it returns successfully,
all preceding accepted mutations' WAL frames and novel fold bytes survive the documented crash
model. WAL replay may restore them to a writer's pending change set even if no part was created.

`sync_with_control` and Node `sync({ timeoutMs, signal })` check interruption before entering the
durability boundary. That boundary first completes any delayed acknowledgement of the selected
container authority, then fsyncs the WAL. Once it starts, the operation reports its actual outcome
rather than claiming cancellation after dependencies may have become durable. `flush_with_control` checks during
pending-change-set and locator planning, after its indivisible part-encoding unit, during bounded digest reads, and
immediately before manifest publication. Cancellation removes the unpublished part and preserves the
pending change set and current store authority; already persisted fold bytes are safe unreachable data.

### `flush`

`flush` materializes the pending change set as an immutable part and atomically publishes a new
manifest revision. When the final container durability barrier succeeds, its publication
acknowledgement makes every included mutation durable; `flush` then removes the redundant WAL input
and leaves the store settled. It is a publication, settlement, and maintenance boundary, not the
explicit synchronization acknowledgement for individual writes. If the successor is selected but
the operation obtains no publication acknowledgement, the handle adopts that published authority
without claiming crash durability from the publication and deliberately retains redundant WAL input
for a later settlement attempt.

A separately opened `ReadStore` is a read view selecting one store authority and does not replay the open writer's WAL.
It therefore sees writes only after manifest publication. This keeps lock-free readers from observing
an unpublished or partially replayed writer state.

### Query visibility

The supported writer-side structured query contract is read-your-writes: a query created from a
writer must include its pending change set with newest-wins resolution before predicates. An immutable
`ReadStore` query sees exactly its selected store authority.

`Store::scan` implements this writer-side contract without forcing a flush; `ReadStore::scan` uses the
same request and page types over its immutable read view. **Current gap:** the Arrow/DataFusion table is
still built only from immutable parts. A future writer-view Arrow implementation must overlay the pending change set rather
than flush as a side effect.

### Read views and maintenance

A `ReadStore` remains pinned to the store authority it opened and, when that authority is a manifest
revision, to its referenced fold generation. Later puts,
publications, and part merges do not change that selection. Newest record-version and tombstone
resolution happens before predicates; a filter can never reveal an older version hidden by a newer
row. Refold, content punch, or erasure may make required physical bytes unavailable, in which case
the open view fails rather than silently changing revisions.

Part merge and refold may replace physical representation, but do not select a different manifest
revision for an existing read view. Explicit erasure deliberately
makes selected content irrecoverable and reports erasure distinctly from corruption.

## 4. Query contract

The storage-native scan API will own:

- projection of ids, typed attributes, named content bytes, and unresolved content references;
- typed equality/range and presence predicates;
- newest-wins visibility before filtering;
- stable forward and reverse ordering;
- opaque cursors constructed and validated by Rust;
- bounded streaming batches, cancellation, deadlines, and memory ceilings;
- schema discovery, explanation, and measured scan statistics.

Metadata-only scans must reconstruct no content and open no fold blocks. Projecting one named content
column must not read sibling content program sections. Large values may exceed the normal batch byte
budget individually, but no value or row may be silently truncated.

The `columnar` feature now exposes the Arrow lens without DataFusion. The `sql` feature adds DataFusion
over precisely that lens. DataFusion pushdown is conservative and cannot change answers.

The initial feature-independent structured pager supports id bounds, exact typed attribute
comparisons, attribute/content presence, stable id ordering in either direction, projection of
selected attributes, content metadata without reconstruction, opt-in content bytes, checked opaque
cursors, and a bound on resolved records evaluated against predicates. A writer page is resolved from
one writer view for the duration of one call. Between pages, keyset continuation prevents duplicates
but may include a
new id inserted ahead of the cursor; callers requiring an immutable multi-page view use `ReadStore`.

`Store::schema`, `ReadStore::schema`, and the corresponding Node methods discover the store's
attribute namespace, every physical scalar type observed for each attribute name, and its separate
named-content namespace without requiring Arrow or SQL. They inspect part metadata rather than
decoding record values, content programs, or fold blocks; the writer view additionally inspects its
pending change set. Discovery across immutable parts is intentionally a conservative physical superset,
because a name may occur only in a row shadowed by a newer version or tombstone. The explicit
`may_include_shadowed_fields`/`mayIncludeShadowedFields` result flag prevents consumers from confusing
that inexpensive descriptive inventory with a required or exact resolved-row schema.

The structured pager accepts an absolute deadline and a shareable cooperative cancellation token.
Checks occur before range work, between candidate records and selected content values, and after
potentially blocking reads. Interruption returns a typed `ScanInterrupted` error and never a partial
page. The Node binding maps `timeoutMs` to a deadline before actor submission (therefore including
queue time) and maps `AbortSignal` to the same Rust token; both reject with `CANCELLED`.

The pager also bounds the selected content bytes retained by one result. Its
`max_reconstructed_bytes` default is 32 MiB and metadata-only selections spend none of it. Before
opening fold blocks, TurnDB totals every byte-projected content value in the candidate row from its
reconstruction program. A row that would cross the remaining budget is left unconsumed and the
partial page's checked cursor resumes at that row; `reconstruction_budget_exhausted` records why the
page stopped. The first matching row is always admitted whole even when it exceeds the ceiling, so a
single large record cannot deadlock pagination. Node exposes the same contract as
`maxReconstructedBytes: bigint` and `reconstructionBudgetExhausted`.

The structured pager now retains each published candidate's authoritative part and row from its
bounded k-way range merge, then decodes only attribute/content columns named by projections or
predicates. It does not point-locate the id again during projection or reconstruction, and byte
projection reuses the already decoded content program and identity. Visibility resolution precedes
projection and the writer's pending change set remains an in-memory newest overlay. Shared layout/metadata
sections are necessary, but sibling value/dictionary/program sections stay unopened. See
`docs/projected-structured-scan.md` and `docs/resolved-row-paging.md`.

Resolved immutable rows are grouped by part in bounded projection chunks. Each selected physical
attribute or content decoder is opened once per part gather and results are restored to global id
order; duplicate fields and sparse content presence remain exact. This is a storage gather seam, not
a claim that semantic predicates use SIMD or encoded-column execution. See
`docs/grouped-column-gather.md`.

Each successful page now carries exact operation-local `stats.io` evidence: distinct raw part
sections and fold blocks touched, cache hit/miss access counts, backing-reader stored bytes, and raw
bytes decoded. The collector is scoped below shared caches, so concurrent read views cannot
contaminate one another through global counter deltas. See `docs/structured-scan-io.md`.

`stats.resolution` separately reports immutable physical row occurrences, superseded occurrences,
deciding tombstones, and pending record versions consumed before predicate evaluation. This
keeps `examined`'s resolved-candidate meaning intact while exposing version-resolution amplification.

`max_resolution_entries` (1,000,000 by default) hard-bounds immutable occurrences plus pending-change-set
entries per page. Equal-id groups are atomic and the first oversized group is admitted for progress.
The cursor can advance through a fully consumed tombstone-only group, so a bounded empty page may
legitimately carry a continuation without rescanning or skipping history. The Node spelling is
`maxResolutionEntries` and `stats.resolution.budgetExhausted` reports when this ceiling stopped work.

`Store::explain_scan` and `ReadStore::explain_scan` use the same validation, checked-cursor, effective
range, and required-field preparation as execution. They report the request's work ceilings and exact
pre-resolution physical rows across initialized parts, plus pending-change-set entries. Explanation does
not resolve visibility, evaluate predicates, estimate returned rows, open value/content sections, or
read fold blocks. It does open id structures and may warm their caches. Node exposes the same method
as `explainScan()` on writer and read-view handles and declares `scanExplanation`. See
`docs/scan-explanation.md`.

**Current gap:** range initialization still visits every part referenced by the current manifest revision, sparse row occurrences are
directly indexed per requested row, and predicates evaluate partial semantic records rather than an
encoded/vector expression batch. `max_examined` separately counts resolved records evaluated against
predicates.

## 5. Platform capabilities

Bindings must expose `turndb::capabilities::capabilities()` rather than infer guarantees from the host.
The portable npm package surfaces the same profile. A WASI guest running on Linux is still WASI and
must report the reduced profile.

| capability | native | portable WASI |
|---|---|---|
| positioned I/O | yes | yes |
| single-writer exclusion | OS-enforced advisory lock | embedder-enforced convention |
| threads / worker execution | available | unavailable in the current build |
| in-place deallocation | Linux hole punching and Windows sparse-range zeroing; unavailable on other native targets | unavailable; refold only |
| columnar lens | build feature | omitted from lightweight package |
| SQL/DataFusion | optional build feature | omitted from lightweight package |
| physical format | current plane identities, sharing draft epoch 1 | current plane identities, sharing draft epoch 1 |
| configurable write admission | u64 byte ceilings | positive u32 byte ceilings |
| atomic frame/object admission | u64 ceilings within address/format spaces | positive u32 ceilings |

A production native Node package must never catch native-addon load failure and silently open the WASM
writer. Portable use must be an explicit package or entry point chosen by the caller.

## 6. Binding seam

The native Node binding uses `napi-rs` at N-API 6 for stable Node ABI compatibility. It may
carry several additional megabytes when those bytes replace bespoke scheduling, buffer, cancellation,
and error machinery that TurnDB would otherwise have to build and maintain.

The Rust/native seam is:

- one owned store actor or equivalently bounded worker per open writer;
- promise-based commands sent over a bounded queue;
- native `Buffer` for content and exact `bigint` for integer fields;
- structured scan specifications converted once at the boundary;
- streaming batches with backpressure and abort integration;
- stable machine-readable error codes plus full contextual messages;
- explicit `sync`, `flush`, `close`, and abandon semantics.

JavaScript must not hold a mutex guard across `await`, run compression or part merge on the event-loop
thread, encode binary content as base64 for the native path, or round i64 through `number`.

The WASM package remains synchronous internally and single-threaded. Its current object API is a
lightweight package surface, not the template for native concurrency.

The native binding implements one dedicated store thread and a bounded command queue.
`NativeStore.open` accepts a per-handle capacity from 1 through 65,536, defaults to 64, and exposes
the selected value on the handle; package capabilities report the default
and maximum. Open and every store operation return Promises; content crosses as `Buffer`, i64 crosses
as `bigint`, and `scan` calls the Rust structured pager directly. `write(ops, durable)` applies one
ordered atomic batch and optionally syncs it before resolving. `close` syncs by default, while
`close(false)` is an explicit no-sync close. Close submission remains possible even when the ordinary
command backlog is full. Dropping the final handle disconnects the queue and releases the writer
rather than keeping an orphan actor alive.

`NativeStore::snapshot` synchronizes and publishes the pending change set after all operations
ordered before it on the actor, settles the store, then opens an immutable `ReadStore` pinned to the
resulting current store authority.
The returned read view's scans can execute concurrently on the blocking pool and remain stable
across later writer activity. A read-only process can open the current store authority directly or
request a manifest revision still present in the bounded retention window; neither path takes the
writer lock or performs WAL replay.

The default native build includes the richer query dependencies. `querySql` runs only against an
immutable `ReadStore`: a read-view handle uses its pinned store authority, while a writer call
actor-serializes synchronization, publication, and store settlement, then opens a new read view. The isolated DataFusion session exposes one generic table named `records`,
accepts typed positional `$1` parameters, and refuses DDL, DML, and session statements before
execution. Its per-query execution-memory pool defaults to 256 MiB and is caller-configurable. A
shared aggregate budget defaults to 1 GiB and reserves each live query's configured ceiling;
writer-derived read views share their writer's governor. A pull-based `NativeSqlQuery` returns schema
IPC separately and one complete, independently decodable
Arrow IPC stream per batch; JavaScript never reconstructs a dynamic schema or walks Arrow rows.
Planning and batch pulls have absolute deadlines and AbortSignal cancellation; dropping either the
unfinished planning future or Rust execution stream aborts its query work and releases reservations.
For a writer query, an already-expired deadline refuses before actor submission. Cancellation after
submission does not retract an ordered read-view prerequisite publication that the actor may already complete.

Binding-owned failure classes, typed DataFusion failures, scan/SQL-pull interruption, writer
contention, backup/restore, and manifest promotion have stable machine-readable codes, and the
Linux x86-64 glibc and Windows x86-64 MSVC loader/package paths and same-artifact Node-major matrix
are implemented; the Linux package is published as `@turndb/native` 0.1.0 and Windows publication is
owner-gated. **Current gaps:** other native platforms remain unqualified, and the aggregate execution
budget is not a total-process RSS limit.

The package-level `TurnDbError` uses the same generic typed-cause classifier exposed to Rust
embedders. It gives stable codes to boundary/scan/cursor validation, bounded-queue overload, closed
handles, interruption, resource ceilings, typed SQL failures, writer contention, filesystem causes,
backup/restore, manifest promotion, and explicit verification-integrity failures. Only `BUSY` and
`CLOSED` are binding-owned. Messages preserve full context but are not API; unknown core failures
deliberately remain `INTERNAL` until a typed engine boundary proves otherwise. See
`docs/error-taxonomy.md`.

Writer lifecycle commands are serialized with ingest. `compact` (part merge), `verify`,
`contentPunch`, and `refold` first perform synchronization, publication, and settlement for earlier
accepted mutations, then operate on the resulting current store authority. When that authority is a
manifest revision, verification covers its retained hash chain and part pins, every referenced part
section, and every referenced fold frame; it is
not a backup. `erase(ids)` invokes the engine's strong erasure composition and purges retained
history when at least one requested slot resolves to a record; an all-absent request performs no
transition. Refold likewise performs no transition when the current authority references no parts.
Its boundary remains this store: backups, replicas, and consumer exports are
not affected. Online backup, validated no-overlay restore, and exclusive fully validated manifest
promotion are exposed in Rust and Node. Manifest promotion defaults to zero rollback, requires explicit authority
to abandon newer retained manifest revisions, and reports its validation evidence. Part merge,
verification, content punch, refold, backup, restore, and manifest promotion accept shared Rust
cancellation/deadline controls, exposed
as submission-inclusive Node `timeoutMs` and `AbortSignal` options. Unpublished
temporary part-merge/refold/backup/restore artifacts are removed on
interruption; content punch is durably resumable after cancellation or crash. Strong erasure accepts
interruption only before its atomic tombstone phase, then drives physical removal to completion so it
cannot return an ambiguous partial-erasure result. Flush part encoding remains an indivisible cancellation unit, while
durability synchronization deliberately stops observing interruption once fsync begins.

Bounded part merge is a generic actor-ordered maintenance primitive, not a built-in scheduling
policy. A caller supplies simultaneous physical input-part, row, and exact file-byte ceilings. Rust
selects the widest fitting contiguous run (oldest on ties), refuses with a typed insufficient-budget
error when even an adjacent pair cannot fit, and reports the exact executed inputs and output bytes.
Partial runs retain tombstones; only a run covering every part referenced by the current manifest revision may remove them. These
ceilings bound input work, not elapsed time. Separate reachability-aware inventory and advisory
temporary-space estimates expose disk-planning evidence without inventing a consumer policy. See
`docs/bounded-part-merge.md` and `docs/maintenance-space.md`.

`Store::health` and the Node `health()` method are constant-work operational observations. They report
the current store authority through the public numeric `commit` encoding (`0` for the canonical
origin, positive for that numbered manifest revision) and, for a manifest revision, its referenced fold generation and part rows
(physical rows, not an invented resolved-record count), pending-change-set
entries and bytes, WAL and fold disk bytes, fold and part cache counters/budgets, Tier-0 dedup
window entries, retained manifest revisions, and declared punched blocks. No record or content is decoded. Latency
histograms, slow-query events, and structured export hooks remain planned follow-on work;
consumers may poll
this generic value into their telemetry system. The separate
`space_usage` / `spaceUsage` inventory performs reachability-aware traversal and reports exact,
disjoint `live`, `retained_only`/`retainedOnly`, and `unclassified` storage report fields. Structured pages separately expose
exact section/block I/O attributable to that operation.
`Store::metrics` and Node `metrics()` separately report process-local monotonic lifecycle outcomes and
nanosecond totals plus exact folded-piece dedup counters. Verification outcomes additionally separate
typed corruption failures from cancellation and I/O. The cancellable `part_distribution` /
`partDistribution` report gives byte/row order statistics for parts referenced by the current manifest
revision, or an empty report at the canonical origin. These surfaces are pull-based
so consumer exporters never execute on the storage thread; see `docs/operation-metrics.md`.
The cancellable `content_liveness` / `contentLiveness` inventory requires an empty pending change set and
separates live-content-reachable piece bytes, dead bytes stranded inside mixed blocks, and whole-block compressed
payload eligible for content punch or refold; see `docs/content-liveness.md`.
`lifecycle_events_after` / `lifecycleEvents` provide a bounded, non-destructive structured outcome
journal with independent sequence cursors and explicit loss accounting; see
`docs/lifecycle-events.md`. Consumers own timestamps, correlation, thresholds, and export policy.
Structured scan pages expose successful execution nanoseconds beside exact work/I/O counters and a
shared-plan explanation API. SQL separates planning/stream-start time from cumulative active pull
and IPC time and supports read-only `EXPLAIN`; consumers decide what is slow.
Rust `StoreOptions` and native Node open options carry storage cache/compression/read-admission policy through the
same seam. Queue, write, scan, SQL, part-merge, and cache overload behavior is explicit and typed;
see `docs/resource-budgets.md`.

## 7. Draft format policy

Package/runtime support, semantic-version rules, deprecation, capability evolution, and on-disk
versioning are separated in the [support and compatibility policy](support-and-compatibility.md).
The physical format is an unfrozen single draft: exact current identities open, every other identity
refuses, and no reader, fixture, converter, or migration surface is retained for superseded drafts.
Narrow fixed-width fields refuse overflow rather than truncate. Unknown optional part sections may be
ignored only where [`FORMAT.md`](../FORMAT.md) explicitly permits it.

## 8. Resource and failure semantics

Existing hard limits are format-derived: part record counts and piece lengths are u32, section stored
and raw sizes are u32, and Arrow binary values are limited by i32 offsets. Encoders and parsers refuse
overflow. Arrow query batches currently target 8,192 rows or 32 MiB of reconstructed content, while
always admitting one oversized row so progress remains possible. Structured pages use the same 32
MiB default as a per-request configurable ceiling, report when the ceiling stops a page, and preserve
an exact continuation without truncating or skipping the deferred row. Structured pages also default
to 1,000,000 pre-predicate resolution entries, counted across immutable row occurrences and pending-change-set
entries. Complete equal-id groups remain atomic and one oversized first group is admitted for
progress; tombstone-only progress is represented by the ordinary checked cursor.

Writer admission adds explicit runtime ceilings before those format limits: 64 MiB worst-case framed
WAL bytes per record, 256 MiB per atomic batch, 4,096 members per batch, and 4 KiB per id/attribute
name/content name by default. All are per-open and configurable. Size and count refusals are resource
exhaustion; malformed policy and names are invalid input. Exact definitions and binding ranges are in
`docs/write-admission.md`.

Atomic data-plane admission is separate: stored and decoded WAL/part/fold frame allocations default
to 512 MiB and are configurable per open. A cache budget does not substitute for this check because
one entry must be materialized before it can be cached or evicted. Fold output splits early for
progress; indivisible piece/part output refuses before mutation/publication. See
`docs/read-admission.md`.

Persistent collection admission closes the small-object counterpart: directory enumeration, physical
WAL frames, and fold blocks/block-id span default to 100,000, 100,000, and 1,000,000. Checks precede
reader collection growth and future writer mutation. A batch charges every member frame plus its
completion marker, and a sparse checksummed block id cannot choose the fold-directory allocation. See
`docs/object-admission.md`.

The native SQL stream has a caller-configurable DataFusion execution-memory pool, while structured
scans have byte ceilings, deadlines, and cooperative cancellation and the native command backlog is
bounded and configurable. The SQL pool is not represented as a total-process RSS guarantee:
DataFusion documents allocations outside its pool, Arrow IPC output is returned to the caller, and
TurnDB's bounded caches are accounted independently. Concurrent queries reserve their per-query
ceilings against a shared aggregate governor. Lifecycle deadlines are cooperative rather than hard
real-time limits: latency is bounded by the current record, section, fold frame, piece, rebuilt part,
or independently content-punched block.
Reaching a budget returns a structured error or partial page with a cursor only where the operation
contract explicitly allows it. It never truncates a record, invents a weaker durability
acknowledgement, or silently widens a scan.

Ambiguous WAL replay or manifest promotion is an error. Corruption, deliberate erasure, unsupported capability, contention,
cancellation, invalid input, and resource exhaustion remain distinguishable machine-readable classes
even when their contextual messages vary.
