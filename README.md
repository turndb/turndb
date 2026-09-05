# turndb

A content-addressed columnar store for AI traces. Embedded, single-writer, no daemon — its published
state is one self-contained file. Use the backup operation for a consistent copy while a writer is
open; a settled store can be copied directly and read from that file alone.

Single-writer enforcement is platform-dependent: the engine enforces it on Unix and cannot on
`wasm32-wasip1`, where the embedder must guarantee it. See *One writer per store* under
[What it does not do](#what-it-does-not-do) and [FORMAT.md](FORMAT.md#store-shape) for the
normative statement.

**Status: pre-1.0, physical format draft epoch 1, not frozen.** [ONTOLOGY.md](ONTOLOGY.md) is the
closed project vocabulary; [FORMAT.md](FORMAT.md) is the normative physical specification. Where
either and the implementation disagree, one of them is a bug.

## What problem it solves

Agent and chat APIs are stateless, so every request re-sends the whole conversation. A trace store
therefore receives the same message text over and over — on a real corpus, **38.9× duplication**.
General stores pay for those bytes repeatedly; archival dedup stores collapse them but stop being
queryable.

turndb splits the two planes:

* the **fold** holds content at stable, content-addressed locations; only manifest-declared content
  punch may make an unreachable payload unavailable within a generation;
* **parts** hold record ids, named content programs, and typed attribute columns — references, not
  bytes.

Where the planes meet is content decomposition: dedup can only recognise repetition where piece
boundaries land, and full-resend traffic repeats exactly at message boundaries. The engine's
default decomposition algorithm—called carving in the API—is therefore structural, not
content-defined: the elements of a top-level JSON array become pieces,
and the punctuation between them stays inline. Turn *k*'s messages and turn *k+1*'s re-sent
messages resolve to the same pieces, so each message is stored once however many later requests
carry it, and the storage cost of a full-resend conversation drops from quadratic in its turn
count to linear. This boundary choice, not content addressing by itself, is what produces the
dedup ratios reported below. Bodies that are not JSON arrays fall back to content-defined
chunking, and the default is not a lock-in: select another decomposition algorithm per write, or
hand the store your own spans and bypass the helper entirely ([`src/carve.rs`](src/carve.rs)).

Because a part holds no content, **part merging never touches content**: merging rewrites references
and columns, which on trace data is a small fraction of the bytes. That is what lets a trace store
behave like a database instead of a write-once archive.

Measured on 40k records of real agent traffic (1.89 GiB of message bodies): 38.9× piece dedup,
**308× total on-disk collapse**, every record byte-exact. Against a production trace store's own
SQLite path on 3,000 full-resend calls — same records, their schema and insert code — 320.24 MiB
became 1.56 MiB.

> **On these numbers.** They were measured on corpora that are not public, and they are
> provisional pending a larger and more rigorous run — our measurements, not independently
> reproducible results. The ratio also depends entirely on how much your traffic re-sends. The
> reproducible form of the claim is comparative: point turndb at your own traces and measure.

## The cardinal invariant

**Byte-exact reconstruction.** Reading a record reproduces every named content value and attribute
exactly — including content boundaries, attribute order, duplicate keys, NaN payloads, and `-0.0`.
Everything else is in service of it.

## Try it

```sh
cargo build --release

# JSONL in: each line needs a "body" string; every other scalar becomes an attribute
head -4000 traces.jsonl | ./target/release/turndb import mystore.turndb -

./target/release/turndb inspect mystore.turndb
./target/release/turndb verify  mystore.turndb --deep   # every record, piece, frame, and pin
./target/release/turndb query   mystore.turndb "SELECT model, count(*) FROM t GROUP BY model"
./target/release/turndb backup  mystore.turndb snap.turndb   # current store authority, self-contained
./target/release/turndb query   snap.turndb "SELECT count(*) FROM t"
# Only if the manifest member is damaged: validates the newest retained manifest revision, with no
# rollback by default. When the current MANIFEST is intact it refuses and exits 1: manifest promotion is never an
# accidental rollback.
./target/release/turndb recover mystore.turndb
```

As a library:

```rust
use turndb::{fold::FoldCfg, store::Store};

let mut s = Store::open_file("mystore.turndb".as_ref(), FoldCfg::default())?;
s.put_body("trace:1#input", body, vec![])?;   // carved by the engine's default opinion
s.sync()?;                                    // the ACK point — durable from here
s.flush()?;                                   // publishes the pending change set
s.close()?;                                   // leaves the store settled; one file remains
```

## SDKs and browser viewer

The consumer's design surface — what an embedder can rely on and what is still a gap — is the
[embedding contract](docs/embedding-contract.md); every document under `docs/` is listed in
[docs/README.md](docs/README.md).
Node and Python expose the same versioned [capability contract](docs/capability-contract.md) and
[structured query contract](docs/query-contract.md), including ordered duplicate attributes,
bit-exact floats, byte-exact named content, backup, and bounded maintenance. Their OpenTelemetry
exporters and provider-independent client-call wrappers implement one
[trace mapping and cadence policy](docs/trace-mapping.md).

The checked [self-contained browser viewer](bindings/browser/turndb-viewer.html) opens a local
`.turndb` with no network traffic or fetches one from a static host using strict HTTP Range. It is
read-only and runs the same structured scans in wasm; the measured multi-GiB cold-open cost and
subsequent point-query cost are recorded separately in [the browser read report](docs/browser.md).

## Storing gen_ai traces

[`examples/genai_dogfood.rs`](examples/genai_dogfood.rs) is the working mapping from LLM API calls
to turndb records, with the reasoning behind each choice in its doc comment;
[`examples/genai_query.rs`](examples/genai_query.rs) runs a trace UI's actual reads — a member's
page, a `responseId` lookup, aggregates — against the result, and checks byte-exact reconstruction
first. The shape:

* **One record per API call, three named contents** — `gen_ai.system_instructions`,
  `gen_ai.input.messages`, `gen_ai.output.messages` — the shape the
  [trace mapping](docs/trace-mapping.md) and both OpenTelemetry exporters write. A record can hold any
  number of independently named content values ([record model](docs/record-model.md)). Each value
  is the message array verbatim, which lets the structural decomposition above resolve re-sent turns to the
  same pieces.
* **Ids are `member/ts/responseId#kind`** with the timestamp zero-padded, so ids sort
  lexicographically into member-then-time order — the access pattern a trace UI actually has — and
  the front-coded id column stays both compressible and range-scannable.
* **Attributes are flattened to OpenTelemetry `gen_ai.*` semantic-convention names**, not stored
  as nested JSON: token usage becomes `gen_ai.usage.*` integer columns that SQL can filter and
  aggregate directly.
* **Arrays become repeated attributes.** turndb preserves duplicate keys in order, so
  `finish_reasons` round-trips without inventing a join separator that could collide.
* **Custom fields pass through with inferred types** — one column per (key, type), so a deployment
  adding its own fields needs no schema change anywhere.

## What it does

| | |
|---|---|
| **Durability** | WAL with an explicit durability acknowledgement; batches replay all-or-nothing; manifest-revision logical authority, superblock-selected container-state authority, retained manifest revisions, WAL replay, and explicit manifest promotion |
| **Query** | Bounded structured paging with Rust-owned cursors plus an optional DataFusion lens and read-only SQL-to-Arrow stream — named content is independently projectable, and metadata queries open zero fold blocks |
| **Part merge** | Total merge at eight parts plus exact input-part/row/byte-bounded work units; merges provably touch zero content bytes |
| **Deletion** | For record slots that resolve present: deletion → synchronization → publication → total merge when needed → non-empty refold removes content *and* metadata; an all-absent request is a no-op, while content punch can deallocate declared unreachable block payloads without moving offsets |
| **Integrity** | Per-piece BLAKE3 on every read, per-section checksums, footer-to-TOC checksum coverage, manifest-pinned parts, and explicit verification that walks every frame |
| **Shipping** | `backup` atomically installs a verified, self-contained file holding the current store authority, which readers and writers open directly |

The backup command takes the writer role (see [FORMAT.md](FORMAT.md#store-shape)), synchronizes and
publishes pending mutations, fully verifies its staged artifact,
and refuses to replace an output path. Restore likewise verifies the staged copy and atomically
installs it only at a destination that does not exist; see [backup and restore](docs/backup-restore.md).
Manifest promotion likewise takes the writer role — excluding another open writer only where that role is
enforced — and validates the complete candidate before publication; rollback past the newest
retained manifest revision requires explicit authorization. See [manifest promotion](docs/manifest-promotion.md).

The query layers are independently selectable: `--features columnar --no-default-features` provides
the Arrow scan lens without DataFusion, while the default `sql` feature adds DataFusion over that same
lens. Storage, visibility, and content semantics remain in TurnDB either way.

`Store::scan` is available without either query feature. It provides id-ordered forward/reverse pages,
typed predicates, selected attributes, named-content metadata or bytes, opaque checked cursors, and a
per-call examination bound. Reconstructed pages default to a 32 MiB content ceiling, can override it
per request, never split a row, and return a cursor before the row that would cross the ceiling.
Named-content metadata includes the BLAKE3 identity of the exact whole value without reconstruction
for every record resolved through the selected store authority. Writer scans include the pending change set;
`ReadStore::scan` remains pinned to its selected store authority.
Rows resolved through a manifest revision are projected from physical columns: sibling attribute value/dictionary and named
content program sections remain unopened. See
[projected structured scans](docs/projected-structured-scan.md).
`Store::explain_scan` and `ReadStore::explain_scan` run the same request/cursor preparation and report
required versus predicate-only fields, work ceilings, effective bounds, and exact physical rows in
scope before visibility resolution. See [structured scan explanation](docs/scan-explanation.md).
Rust embedders can classify rich error chains through the stable, domain-neutral
[`ErrorClass`](docs/error-taxonomy.md); the native Node binding exposes the same engine codes through
`TurnDbError` and adds only its actor-owned `BUSY`/`CLOSED` states.

Scalar fields include exact unsigned u64, arbitrary binary metadata, UTC Unix-nanosecond
timestamps, and explicit null. Missing and null remain distinct, and
bindings never route exact integers through JavaScript `number`; see
[general scalar field types](docs/field-types.md).

Writer opens also carry generic admission policy: worst-case framed-WAL bytes per record and atomic
batch, batch member count, and UTF-8 identifier/name bytes. Defaults are 64 MiB, 256 MiB, 4,096, and
4 KiB respectively; Rust, native Node, and portable Node can override them. Complete batches are
charged before the first fold mutation, and charging is independent of dedup history. See
[write admission limits](docs/write-admission.md).

Reads independently admit every atomic WAL, part-TOC/section, and fold-block frame before stored or
decoded allocation. Both ceilings default to 512 MiB, are configurable per Rust/native/portable
handle, classify refusal as `RESOURCE_EXHAUSTED`, and are distinct from cache residency. Writers close
fold blocks early under strict profiles and refuse oversized part outputs before publication; see
[atomic frame read admission](docs/read-admission.md).

Persistent collection growth is bounded separately within that same per-open profile: 100,000
directory entries, 100,000 physical WAL frames, and 1,000,000 fold blocks by default. Checks precede
directory/vector growth and future writer output, including atomic-batch member frames and sparse
fold ids; see [persistent object-count admission](docs/object-admission.md).

Long-running part merge, verification, content punch, refold, backup, and restore operations accept
reusable Rust cancellation tokens and absolute deadlines through their controlled variants. The
native Node methods map these to submission-inclusive `timeoutMs` and `AbortSignal` options while
preserving each operation's publication and restart invariants; see
[lifecycle cancellation and deadlines](docs/lifecycle-control.md).
Incremental part merge accepts simultaneous exact physical input-part, row, and file-byte limits,
reports the executed plan and output bytes, and refuses rather than exceeding an insufficient budget;
see [bounded incremental part merge](docs/bounded-part-merge.md).
Reachability-aware storage inventory separates the literal `live`, `retainedOnly`, and `unclassified` report fields, while
part merge and refold expose exact source facts plus explicitly advisory staging estimates; see
[maintenance space accounting and preflight](docs/maintenance-space.md).
Writer handles expose process-local, monotonic lifecycle outcomes and nanosecond totals through a
telemetry-neutral polling surface; see [pull-based operation metrics](docs/operation-metrics.md).

With `sql` enabled, `query::sql::SqlQuery` runs positional-parameter SQL against the generic
`records` table under a configurable DataFusion execution-memory ceiling. DDL, DML, and session
statements are refused. Results are pulled one bounded batch at a time as complete Arrow IPC streams;
bindings therefore transport dynamic columnar results without translating them through JSON or
JavaScript objects. The ownership, read-view, cancellation, and memory semantics are detailed in
[Read-only SQL and Arrow IPC streaming](docs/sql-arrow-stream.md).
Point reads, storage-native structured scans, and DataFusion are held to one versioned-record
reference model by the [three-path differential gate](docs/differential-query-testing.md).

## What it does not do

No daemon, no network, no cluster, no consensus. Scale-out is more stores, not a bigger one. No
encryption (the format reserves a flag bit and refuses it — see FORMAT.md). No parity/erasure
coding: corruption is detected at every level and repair is the storage layer's job.
The parser/binding threat model, completed hardening, and remaining availability risks are recorded
in the [security review](docs/security-review.md); checksums are integrity evidence, not authentication.

**One writer per store.** Readers need no lock and see a consistent read view pinned to one store authority. Native
builds enforce this on the container handle with `flock` on Unix and `LockFileEx` on Windows; the
operating system releases the lock when the process dies, so a stale lock cannot outlive its
owner. **Under WASI there is no advisory locking and the engine cannot enforce it**: no lock
sidecar exists and the lock attempt gates nothing, so the obligation is the
embedder's — at most one open writer per store file, across every process and every WASM
instance. The guest cannot enforce or detect a violation. In four measured
overlapping-writer runs on Node 24, both writers received successful `sync()` acknowledgements and
one writer's complete record set was silently discarded. The surviving store was internally
consistent and every remaining record was readable, so a clean read or verification cannot prove
that an acknowledged write survived. Other overlap patterns may fail differently.
See [FORMAT.md](FORMAT.md#store-shape) for the normative statement.

## Platforms

The exact tested targets, Node majors, capability rules, semantic-version policy, and current
prototype/publication status are defined in the
[support and compatibility policy](docs/support-and-compatibility.md).

The crate's **native** build is qualified on Linux and Windows x86-64. The capability profile names
the platform locking and reclamation mechanisms instead of assuming they are identical.

`bindings/node` is the native server-side release candidate. Its `napi-rs` addon gives each open
writer a dedicated Rust actor and bounded queue, exposes Promise-based batch/durability/scan/content
operations with `Buffer` and exact `bigint`, and refuses to fall back to WASM when a native artifact
is unavailable. Prebuilt slices cover Linux x86-64 glibc and Windows x86-64 MSVC across Node 22, 24,
and 26; their
package manifests carry npm's `"private": true`, which makes `npm publish` refuse outright, so
publication can only happen through the staged release path. The
[native prebuild contract](docs/native-prebuilds.md) states the exact artifact, install, provenance,
and publication gates.

It also builds for **`wasm32-wasip1`**, which is what the [`turndb` npm package](npm/turndb)
ships: one `.wasm`, no native addon, no prebuild matrix, no postinstall. A store written by either
build is readable by the other, byte for byte; the two-way executable proof is documented under
[cross-runtime compatibility](docs/cross-runtime-compatibility.md). That target gives up three things,
and the first is the one that matters: **no advisory locking** (see above), no content-punch API
(`refold`
reclaims the same space by rewriting), and no threads, so compression runs inline. `src/sys.rs` is
the single place that states what turndb needs from an operating system and what happens where it
isn't there.

## Testing

The test suite is large relative to the engine, deliberately: a storage engine that loses data is
worthless.

```sh
cargo test                              # the ordinary suites
cargo test --features dst --test dst    # every crash state of eleven recorded protocols, strict POSIX
cargo test --test corruption            # ~48k mutants across every parser
STORM_XOR=$RANDOM cargo test --test corruption   # fresh mutant space
```

The **deterministic simulation** harness records every write, fsync, rename, link, unlink, and
content punch, then replays every crash point under a model where file content and directory entries
become durable independently, and asserts the reopened store equals some prefix of the
acknowledged writes. Eleven sweeps cover the mixed write path; backup, restore, manifest promotion,
content-hole punching; and the single-file session, merge,
erasure, and free-space-punch protocols. Each prints the exact number of crash states it checked,
so the suite reports its own coverage. It has found real bugs, including an ACK backed by a WAL
whose directory entry was not yet durable, and a declared hole punch whose partially landed
deallocation bricked writer reopen and WAL replay.

The **corruption storm** mutates every on-disk structure and requires errors, never panics. It
found five parser bug classes, including bounds checks of the form `at + n > len` that *overflow
and therefore pass*.

## License

Apache-2.0. Copyright 2026 Efficacious, Inc. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
