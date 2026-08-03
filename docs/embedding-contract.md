# TurnDB embedding contract

Status: accepted direction for the pre-1.0 implementation. Sections marked **current gap** describe
work that must land before the corresponding promise is considered supported.

This document turns the product boundary in `ROADMAP.md` into decisions an embedder can design
against. It is intentionally consumer-neutral: OpenTelemetry, CommandSuite, and any other record
vocabulary belong in adapters above this contract.

## 1. Architecture decision

TurnDB is an embedded, content-addressed, columnar, single-writer store with concurrent snapshot
readers.

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

The name `body` is a compatibility convention, not a privileged storage field. Other names are
projected as `content.<name>`. Missing content is distinct from present empty content.

Content remains structurally content-addressed: part programs reference the shared fold's BLAKE3
piece identities. Compaction rewrites programs and columns, not carved piece bytes. Identical pieces
deduplicate across record ids, content names, record families, and consumers using the same store.

Part format revision 2 and WAL tags `0x5C`/`0x5D` introduced named content. Revision 3 and WAL tags
`0x5E`/`0x5F` additionally persist BLAKE3 of each complete reconstructed value. The digest is computed
over ingest spans without concatenating them, survives WAL replay and compaction, and is projected
with presence, length, and piece count without reading fold blocks. Revision-0/1 parts and legacy WAL
records are read as one dense content value named `body`; revision-0/1/2 values honestly report their
whole-value identity unavailable rather than substituting a program or piece hash. See
`docs/record-model-v2.md`, `docs/content-identity-v3.md`, and `FORMAT.md`.
Revision 4 and WAL tags `0x60`/`0x61` add the remaining scalar attribute types without changing named
content identity or the original tag encodings.

Revision 4 completes the initial scalar type system with unsigned u64, arbitrary binary metadata,
signed Unix-nanosecond timestamps interpreted in UTC, and explicit null in addition to UTF-8 string,
signed i64, bit-exact f64, and boolean. Missing means the key has no occurrence; explicit null is a
real ordered attribute occurrence. Structured `attr_exists` and equality-to-null preserve the
distinction. The Arrow lens exposes `key#null` as a nullable boolean presence marker (`true` explicit,
Arrow null missing), because an Arrow `Null` array cannot represent that distinction. See
`docs/field-types-v4.md`.

## 3. Write, durability, and visibility

TurnDB distinguishes staging, durability acknowledgement, and columnar publication.

### `put`, `put_record`, `delete`, and `apply`

These stage changes in the writer's WAL buffer and memtable. `apply` is an atomic group for recovery:
replay applies every member only when its batch commit marker is intact. A successful staging call is
not a durability acknowledgement.

Before staging, the writer applies its per-open `WriteLimits`. The deterministic charge is a
worst-case complete WAL frame with every piece occurrence treated as novel; a batch is all member
frames plus its commit marker. Record/batch byte ceilings, batch member count, and UTF-8 identifier
bytes are inclusive and configurable. Complete validation precedes any fold mutation, and recovery
does not reapply newly chosen policy to accepted WAL frames. See `docs/write-admission.md`.

The writer provides read-your-writes immediately after successful staging. Point reads and id scans
through that writer include its memtable, including staged tombstones. These reads may therefore see
data that will disappear if the process exits before `sync`.

### `sync`

`sync` is the durability acknowledgement point. When it returns successfully, all preceding staged
WAL frames and novel fold bytes survive the documented crash model. Recovery may restore them to a
writer memtable even if no part was created.

The future binding-level operation named `commit(records, durability: "sync")` is composition, not a
new storage transition: apply the atomic batch, then `sync`, and resolve only after both succeed.

### `flush`

`flush` seals the current memtable into an immutable part, atomically publishes a new manifest, and
then makes the redundant WAL truncatable. It is a publication and maintenance boundary, not the
durability acknowledgement for individual writes.

A separately opened `ReadStore` reads a manifest snapshot and does not replay the live writer's WAL.
It therefore sees writes only after manifest publication. This keeps lock-free readers from observing
an uncommitted or partially replayed writer state.

### Query visibility

The supported writer-side structured query contract is read-your-writes: a query created from a
writer must include its memtable with newest-wins resolution before predicates. An immutable
`ReadStore` query sees exactly its manifest snapshot.

`Store::scan` implements this writer-side contract without forcing a flush; `ReadStore::scan` uses the
same request and page types over its immutable snapshot. **Current gap:** the Arrow/DataFusion table is
still built only from immutable parts. A future live Arrow snapshot must overlay the memtable rather
than flush as a side effect.

### Snapshots and compaction

A `ReadStore` is pinned to the manifest and fold generation it opened. Later puts, flushes, merges,
refolds, and erasures do not alter its logical row set. Newest record version and tombstone resolution
happens before predicates; a filter can never reveal an older version hidden by a newer row.

Compaction and refold may replace physical files, but do not change the logical result of a pinned
snapshot within the documented retention window. Explicit erasure is the exception: it deliberately
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
cursors, and a bound on live records evaluated against predicates. A writer page is a consistent view
for the duration of one call. Between pages, keyset continuation prevents duplicates but may include a
new id inserted ahead of the cursor; callers requiring an immutable multi-page view use `ReadStore`.

`Store::schema`, `ReadStore::schema`, and the corresponding Node methods discover the store's
attribute namespace, every physical scalar type observed for each attribute name, and its separate
named-content namespace without requiring Arrow or SQL. They inspect part metadata rather than
decoding record values, content programs, or fold blocks; the writer view additionally inspects its
live memtable. Discovery across immutable parts is intentionally a conservative physical superset,
because a name may occur only in a row shadowed by a newer version or tombstone. The explicit
`may_include_shadowed_fields`/`mayIncludeShadowedFields` result flag prevents consumers from confusing
that inexpensive descriptive inventory with a required or exact live-row schema.

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

The structured pager now retains each committed candidate's authoritative part and row from its
bounded k-way range merge, then decodes only attribute/content columns named by projections or
predicates. It does not point-locate the id again during projection or reconstruction, and byte
projection reuses the already decoded content program and identity. Visibility resolution precedes
projection and the writer's memtable remains an in-memory newest overlay. Shared layout/metadata
sections are necessary, but sibling value/dictionary/program sections stay unopened. See
`docs/projected-structured-scan.md` and `docs/resolved-row-paging.md`.

Resolved immutable rows are grouped by part in bounded projection chunks. Each selected physical
attribute or content decoder is opened once per part gather and results are restored to global id
order; duplicate fields and sparse content presence remain exact. This is a storage gather seam, not
a claim that semantic predicates use SIMD or encoded-column execution. See
`docs/grouped-column-gather.md`.

Each successful page now carries exact operation-local `stats.io` evidence: distinct raw part
sections and fold blocks touched, cache hit/miss access counts, backing-reader stored bytes, and raw
bytes decoded. The collector is scoped below shared caches, so concurrent snapshots cannot
contaminate one another through global counter deltas. See `docs/structured-scan-io.md`.

`stats.resolution` separately reports immutable physical row occurrences, superseded occurrences,
deciding tombstones, and live-writer memtable entries consumed before predicate evaluation. This
keeps `examined`'s live-candidate meaning intact while exposing version-resolution amplification.

`max_resolution_entries` (1,000,000 by default) hard-bounds immutable occurrences plus memtable
entries per page. Equal-id groups are atomic and the first oversized group is admitted for progress.
The cursor can advance through a fully consumed tombstone-only group, so a bounded empty page may
legitimately carry a continuation without rescanning or skipping history. The Node spelling is
`maxResolutionEntries` and `stats.resolution.budgetExhausted` reports when this ceiling stopped work.

`Store::explain_scan` and `ReadStore::explain_scan` use the same validation, checked-cursor, effective
range, and required-field preparation as execution. They report the request's work ceilings and exact
pre-resolution physical rows across initialized parts, plus writer-memtable entries. Explanation does
not resolve visibility, evaluate predicates, estimate returned rows, open value/content sections, or
read fold blocks. It does open id structures and may warm their caches. Node exposes the same method
as `explainScan()` on writer and snapshot handles and declares `scanExplanation`. See
`docs/scan-explanation.md`.

**Current gap:** range initialization still visits every live part, sparse row occurrences are
directly indexed per requested row, and predicates evaluate partial semantic records rather than an
encoded/vector expression batch. `max_examined` separately counts live records evaluated against
predicates.

## 5. Platform capabilities

Bindings must expose `turndb::capabilities::capabilities()` rather than infer guarantees from the host.
The portable npm package surfaces the same profile. A WASI guest running on Linux is still WASI and
must report the reduced profile.

| capability | native Unix | portable WASI |
|---|---|---|
| positioned I/O | yes | yes |
| single-writer exclusion | OS-enforced advisory lock | embedder-enforced convention |
| threads / worker execution | available | unavailable in the current build |
| in-place hole-punch erasure | Linux only | unavailable; refold only |
| columnar lens | build feature | omitted from lightweight package |
| SQL/DataFusion | optional build feature | omitted from lightweight package |
| format interoperability | revision 4 | revision 4 |
| configurable write admission | u64 byte ceilings | positive u32 byte ceilings |

A production native Node package must never catch native-addon load failure and silently open the WASM
writer. Portable use must be an explicit package or entry point chosen by the caller.

## 6. Binding seam

The native Node source prototype uses `napi-rs` at N-API 6 for stable Node ABI compatibility. It may
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

JavaScript must not hold a mutex guard across `await`, run compression or compaction on the event-loop
thread, encode binary content as base64 for the native path, or round i64 through `number`.

The WASM package remains synchronous internally and single-threaded. Its current object API is a
lightweight compatibility profile, not the template for native concurrency.

The current native prototype implements one dedicated store thread and a bounded command queue.
`NativeStore.open` accepts a per-handle capacity from 1 through 65,536, defaults to 64 for
compatibility, and exposes the selected value on the handle; package capabilities report the default
and maximum. Open and every store operation return Promises; content crosses as `Buffer`, i64 crosses
as `bigint`, and `scan` calls the Rust structured pager directly. `write(ops, durable)` applies one
ordered atomic batch and optionally syncs it before resolving. `close` syncs by default, while
`close(false)` is an explicit no-sync close. Close submission remains possible even when the ordinary
command backlog is full. Dropping the final handle disconnects the queue and releases the writer
rather than keeping an orphan actor alive.

`NativeStore::snapshot` flushes all operations ordered before it on the actor, then opens an immutable
`ReadStore` at that exact published cut. The snapshot's scans can execute concurrently on the blocking
pool and remain stable across later writer activity. A read-only process can open the current published
manifest directly or request a commit still present in the bounded retained-manifest window; neither
path takes the writer lock or replays an unflushed WAL.

The default native build includes the richer query dependencies. `querySql` runs only against an
immutable `ReadStore`: a snapshot handle uses its existing cut, while a writer call actor-serializes a
new published cut first. The isolated DataFusion session exposes one generic table named `records`,
accepts typed positional `$1` parameters, and refuses DDL, DML, and session statements before
execution. Its per-query execution-memory pool defaults to 256 MiB and is caller-configurable. A
shared aggregate budget defaults to 1 GiB and reserves each live query's configured ceiling;
writer-derived snapshots share their writer's governor. A pull-based `NativeSqlQuery` returns schema
IPC separately and one complete, independently decodable
Arrow IPC stream per batch; JavaScript never reconstructs a dynamic schema or walks Arrow rows.
Batch pulls have timeouts and AbortSignal cancellation and dropping the Rust execution stream aborts
unfinished work.

**Current gaps:** binding-owned failure classes, typed DataFusion failures, scan/SQL-pull interruption,
writer contention, backup/restore, and manifest recovery have stable machine-readable codes; prebuilt
artifact selection is not implemented yet. SQL planning and offline recovery are not interruptible,
and the aggregate execution budget is not a total-process RSS limit. The package is a tested source
prototype and must not be described as a production distribution.

The package-level `TurnDbError` uses the same generic typed-cause classifier exposed to Rust
embedders. It gives stable codes to boundary/scan/cursor validation, bounded-queue overload, closed
handles, interruption, resource ceilings, typed SQL failures, writer contention, filesystem causes,
backup/restore, manifest recovery, and explicit verification-integrity failures. Only `BUSY` and
`CLOSED` are binding-owned. Messages preserve full context but are not API; unknown core failures
deliberately remain `INTERNAL` until a typed engine boundary proves otherwise. See
`docs/error-taxonomy.md`.

Writer lifecycle commands are serialized with ingest. `compact`, `verify`, `punch`, and `refold`
first sync and flush earlier writes, then operate on the resulting published cut. Verification covers
the retained manifest hash chain and part pins, every live part section, and every fold frame; it is
not a backup. `erase(ids)` invokes the engine's strong erasure composition and purges retained history.
Its boundary remains this store: previously written packs, backups, replicas, and consumer exports are
not affected. Online backup, validated no-overlay restore, and exclusive fully validated manifest
recovery are exposed in Rust and Node. Recovery defaults to zero rollback, requires explicit authority
to abandon newer retained commits, and reports its validation evidence. Compaction, verification,
punching, and refold now accept shared Rust cancellation/deadline controls, exposed as queue-inclusive
Node `timeoutMs` and `AbortSignal` options. Unpublished compaction/refold staging is removed on
interruption; punching is durably resumable after cancellation or crash. Strong erasure accepts
interruption only before its atomic tombstone phase, then drives physical removal to completion so it
cannot return an ambiguous partial-erasure result. Preflight space estimates and resumable format
migration remain current gaps. Backup, restore, offline recovery, SQL planning, sync, and flush remain
non-cancellable.

Bounded compaction is a generic actor-ordered maintenance primitive, not a built-in scheduling
policy. A caller supplies simultaneous physical input-part, row, and exact file-byte ceilings. Rust
selects the widest fitting contiguous run (oldest on ties), refuses with a typed insufficient-budget
error when even an adjacent pair cannot fit, and reports the exact executed inputs and output bytes.
Partial runs retain tombstones; only a run covering the complete live list may settle them. These
ceilings bound input work, not elapsed time or temporary disk usage. See
`docs/bounded-compaction.md`.

`Store::health` and the Node `health()` method are constant-work operational snapshots. They report
the current commit/fold generation, part rows (physical rows, not an invented live-row count), staged
memtable entries and bytes, WAL and fold disk bytes, fold and part cache counters/budgets, Tier-0 dedup
window entries, retained commits, and punched blocks. No record or content is decoded. Latency
histograms, slow-query events, dedup ratios, reclaimable-byte estimation, and export hooks remain
Phase-5 work; consumers may poll this generic value into their telemetry system. Structured pages
separately expose exact section/block I/O attributable to that operation.

## 7. Compatibility policy

TurnDB is pre-1.0 and the format is not frozen, but every accepted revision follows these rules:

- A reader refuses a part version above its maximum before parsing version-specific sections.
- Current readers retain fixtures and adapters for supported older parts and WAL tags.
- Unknown optional part sections are ignored; required layout changes use a version or new WAL tag.
- Narrow fixed-width fields refuse overflow rather than truncate.
- New writers do not emit legacy layouts merely to preserve downgrade compatibility.
- Older readers may safely refuse a new store; they must never silently misread it.
- Migrations are explicit operations with preflight space checks and resumable commit points before
  format stability is declared.
- Rust and Node APIs follow semantic versioning once public stability is claimed. Until then, breaking
  changes are called out in release notes and compatibility adapters are preferred where they do not
  weaken the model.

## 8. Resource and failure semantics

Existing hard limits are format-derived: part record counts and piece lengths are u32, section stored
and raw sizes are u32, and Arrow binary values are limited by i32 offsets. Encoders and parsers refuse
overflow. Arrow query batches currently target 8,192 rows or 32 MiB of reconstructed content, while
always admitting one oversized row so progress remains possible. Structured pages use the same 32
MiB default as a per-request configurable ceiling, report when the ceiling stops a page, and preserve
an exact continuation without truncating or skipping the deferred row. Structured pages also default
to 1,000,000 pre-predicate resolution entries, counted across immutable row occurrences and memtable
entries. Complete equal-id groups remain atomic and one oversized first group is admitted for
progress; tombstone-only progress is represented by the ordinary checked cursor.

Writer admission adds explicit runtime ceilings before those format limits: 64 MiB worst-case framed
WAL bytes per record, 256 MiB per atomic batch, 4,096 members per batch, and 4 KiB per id/attribute
name/content name by default. All are per-open and configurable. Size and count refusals are resource
exhaustion; malformed policy and names are invalid input. Exact definitions and binding ranges are in
`docs/write-admission.md`.

The native SQL stream has a caller-configurable DataFusion execution-memory pool, while structured
scans have byte ceilings, deadlines, and cooperative cancellation and the native command backlog is
bounded and configurable. The SQL pool is not represented as a total-process RSS guarantee:
DataFusion documents allocations outside its pool, Arrow IPC output is returned to the caller, and
TurnDB's bounded caches are accounted independently. Concurrent queries reserve their per-query
ceilings against a shared aggregate governor. Lifecycle deadlines are cooperative rather than hard
real-time limits: latency is bounded by the current record, section, fold frame, piece, rebuilt part,
or independently punched block.
Reaching a budget returns a structured error or partial page with a cursor only where the operation
contract explicitly allows it. It never truncates a record, invents a weaker durability
acknowledgement, or silently widens a scan.

Ambiguous recovery is an error. Corruption, deliberate erasure, unsupported capability, contention,
cancellation, invalid input, and resource exhaustion remain distinguishable machine-readable classes
even when their contextual messages vary.
