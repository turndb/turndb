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

Part format revision 2 and WAL tags `0x5C`/`0x5D` carry named content. Revision-0/1 parts and legacy WAL
records are read as one dense content value named `body`. See `docs/record-model-v2.md` and
`FORMAT.md` for the normative physical layout.

**Current gap:** the core exposes per-piece identities and logical content length, but it does not yet
persist a BLAKE3 identity for the fully reconstructed named value. A whole-value reference API must
not pretend a program hash is the byte identity. The next content-reference revision must either
persist the exact-byte digest at ingest or explicitly report it unavailable for legacy values.

**Current gap:** the initial attribute types remain UTF-8 string, signed i64, f64, and boolean. The
roadmap's unsigned integer, binary, timestamp, and explicit null semantics require a deliberate format
revision; they must not be simulated with lossy strings or floats in a binding.

## 3. Write, durability, and visibility

TurnDB distinguishes staging, durability acknowledgement, and columnar publication.

### `put`, `put_record`, `delete`, and `apply`

These stage changes in the writer's WAL buffer and memtable. `apply` is an atomic group for recovery:
replay applies every member only when its batch commit marker is intact. A successful staging call is
not a durability acknowledgement.

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

**Current gap:** the existing Arrow/DataFusion table is built only from immutable parts. It therefore
requires a flush to see writer memtable data. The native binding must not paper over this by forcing
flushes; the structured scan core needs a writer snapshot source that overlays the memtable.

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
budget individually, but no value may be silently truncated.

The `columnar` feature now exposes the Arrow lens without DataFusion. The `sql` feature adds DataFusion
over precisely that lens. DataFusion pushdown is conservative and cannot change answers.

**Current gaps:** field predicates do not yet include id, null/missing, or existence operations;
ordering is physical part order rather than a public stable cursor contract; cancellation and
deadlines are absent; scan statistics do not yet report section bytes or distinct fold blocks.

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
| format interoperability | revision 2 | revision 2 |

A production native Node package must never catch native-addon load failure and silently open the WASM
writer. Portable use must be an explicit package or entry point chosen by the caller.

## 6. Binding seam

The supported native Node path will use N-API, preferably `napi-rs`, for stable Node ABI compatibility.
It may carry several additional megabytes when those bytes replace bespoke scheduling, buffer,
cancellation, and error machinery that TurnDB would otherwise have to build and maintain.

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
overflow. Query batches currently target 8,192 rows or 32 MiB of reconstructed content, while always
admitting one oversized value so progress remains possible.

Before production binding maturity, the API must add configurable queue depth, query memory, deadline,
and cancellation budgets. Reaching a budget returns a structured error or partial page with a cursor
only where the operation contract explicitly allows it. It never truncates a record, invents a weaker
durability acknowledgement, or silently widens a scan.

Ambiguous recovery is an error. Corruption, deliberate erasure, unsupported capability, contention,
cancellation, invalid input, and resource exhaustion remain distinguishable machine-readable classes
even when their contextual messages vary.
