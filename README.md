# turndb

A content-addressed columnar store for AI traces. Embedded, single-writer, no daemon — a store is
a directory you can `tar`, and reading one needs nothing but the files.

**Status: pre-1.0, format version 4, not frozen.** See [FORMAT.md](FORMAT.md), which is normative:
where it and the code disagree, one of them is a bug.

## What problem it solves

Agent and chat APIs are stateless, so every request re-sends the whole conversation. A trace store
therefore receives the same message text over and over — on a real corpus, **38.9× duplication**.
General stores pay for those bytes repeatedly; archival dedup stores collapse them but stop being
queryable.

turndb splits the two planes:

* the **fold** holds content, addressed by BLAKE3 of its bytes, written once and never rewritten;
* **parts** hold record ids, named content programs, and typed attribute columns — references, not
  bytes.

Because a part holds no content, **compaction never touches content**: merging rewrites references
and columns, which on trace data is a small fraction of the bytes. That is what lets a trace store
behave like a database instead of a write-once archive.

Measured on 40k records of real agent traffic (1.89 GiB of message bodies): 38.9× piece dedup,
**308× total on-disk collapse**, every record byte-exact. Against a production trace store's own
SQLite path on 3,000 full-resend calls — same records, their schema and insert code — 320.24 MiB
became 1.56 MiB.

> **On these numbers.** They come from corpora that are not yet public, so you cannot reproduce
> them today, and they are provisional pending a larger and more rigorous run. Treat them as our
> measurements rather than as published results. What you *can* reproduce is the shape of the
> claim: point turndb at your own traces and compare. The ratio depends entirely on how much your
> traffic re-sends, which is the whole thesis.

## The cardinal invariant

**Byte-exact reconstruction.** Reading a record reproduces every named content value and attribute
exactly — including content boundaries, attribute order, duplicate keys, NaN payloads, and `-0.0`.
Everything else is in service of it.

## Try it

```sh
cargo build --release

# JSONL in: each line needs a "body" string; every other scalar becomes an attribute
head -4000 traces.jsonl | ./target/release/turndb import mystore -

./target/release/turndb inspect mystore
./target/release/turndb verify  mystore --deep     # every record, piece, frame, and pin
./target/release/turndb query   mystore "SELECT model, count(*) FROM t GROUP BY model"
./target/release/turndb pack    mystore snap.turndb
./target/release/turndb query   snap.turndb "SELECT count(*) FROM t"   # SQL over one file
./target/release/turndb unpack  snap.turndb restored                   # validated, no overlay
# If MANIFEST is damaged: validates the newest retained commit and permits no rollback by default
./target/release/turndb recover mystore
```

As a library:

```rust
use turndb::{fold::FoldCfg, store::Store};

let mut s = Store::open("mystore".as_ref(), FoldCfg::default())?;
s.put_body("trace:1#input", body, vec![])?;   // carved by the engine's default opinion
s.sync()?;                                    // the ACK point — durable from here
s.flush()?;                                   // seal into an immutable part
```

## What it does

| | |
|---|---|
| **Durability** | WAL with an explicit ACK point; batches replay all-or-nothing; one commit point (the manifest) with a checksummed commit log, snapshots, and explicit recovery |
| **Query** | Bounded structured paging with Rust-owned cursors plus an optional DataFusion lens and read-only SQL-to-Arrow stream — named content is independently projectable, and metadata queries open zero fold blocks |
| **Compaction** | Total merge at eight parts plus exact input-part/row/byte-bounded work units; merges provably touch zero content bytes |
| **Deletion** | Tombstone → settle → re-fold removes content *and* metadata; `punch` reclaims dead blocks in place without moving a single offset |
| **Integrity** | Per-piece BLAKE3 on every read, per-section checksums, footer and TOC chains, manifest-pinned parts, and a `scrub` that walks every frame |
| **Shipping** | `pack` puts a whole store in one file that reads — and answers SQL — identically |

The pack command takes the writer role, settles a recovered WAL, fully verifies its staged artifact,
and refuses to replace an output path. Restore likewise verifies before extraction and atomically
publishes only to a destination that does not exist; see [backup and restore](docs/backup-restore.md).
Manifest recovery is likewise exclusive and validates the complete candidate before publication;
rollback past the newest retained commit requires explicit authorization. See
[manifest recovery](docs/recovery.md).

The query layers are independently selectable: `--features columnar --no-default-features` provides
the Arrow scan lens without DataFusion, while the default `sql` feature adds DataFusion over that same
lens. Storage, visibility, and content semantics remain in TurnDB either way.

`Store::scan` is available without either query feature. It provides id-ordered forward/reverse pages,
typed predicates, selected attributes, named-content metadata or bytes, opaque checked cursors, and a
per-call examination bound. Reconstructed pages default to a 32 MiB content ceiling, can override it
per request, never split a row, and return a cursor before the row that would cross the ceiling.
Named-content metadata includes the BLAKE3 identity of the exact whole value without reconstruction
for revision-3 records. Writer scans include the memtable;
`ReadStore::scan` remains pinned to its manifest snapshot.
Committed rows are projected from physical columns: sibling attribute value/dictionary and named
content program sections remain unopened. See
[projected structured scans](docs/projected-structured-scan.md).

Format revision 4 adds exact unsigned u64, arbitrary binary metadata, UTC Unix-nanosecond
timestamps, and explicit null to the existing scalar fields. Missing and null remain distinct, and
bindings never route exact integers through JavaScript `number`; see
[general scalar field types](docs/field-types-v4.md).

Writer opens also carry generic admission policy: worst-case framed-WAL bytes per record and atomic
batch, batch member count, and UTF-8 identifier/name bytes. Defaults are 64 MiB, 256 MiB, 4,096, and
4 KiB respectively; Rust, native Node, and portable Node can override them. Complete batches are
charged before the first fold mutation, and charging is independent of dedup history. See
[write admission limits](docs/write-admission.md).

Long-running compaction, verification, punching, and refold operations accept reusable Rust
cancellation tokens and absolute deadlines through their controlled variants. The native Node methods
map these to queue-inclusive `timeoutMs` and `AbortSignal` options while preserving each operation's
publication and restart invariants; see
[lifecycle cancellation and deadlines](docs/lifecycle-control.md).
Incremental compaction accepts simultaneous exact physical input-part, row, and file-byte limits,
reports the executed plan and output bytes, and refuses rather than exceeding an insufficient budget;
see [bounded incremental compaction](docs/bounded-compaction.md).

With `sql` enabled, `query::sql::SqlQuery` runs positional-parameter SQL against the generic
`records` table under a configurable DataFusion execution-memory ceiling. DDL, DML, and session
statements are refused. Results are pulled one bounded batch at a time as complete Arrow IPC streams;
bindings therefore transport dynamic columnar results without translating them through JSON or
JavaScript objects. The ownership, snapshot, cancellation, and memory semantics are detailed in
[Read-only SQL and Arrow IPC streaming](docs/sql-arrow-stream.md).

## What it does not do

No daemon, no network, no cluster, no consensus. Scale-out is more stores, not a bigger one. No
encryption (the format reserves a flag bit and refuses it — see FORMAT.md). No parity/erasure
coding: corruption is detected at every level and repair is the storage layer's job.

**One writer per store.** Readers need no lock and see a consistent committed snapshot. On Unix
the engine enforces this with `flock`, which the kernel releases when the process dies — so a
stale lock cannot outlive its owner. **Under WASI there is no advisory locking and the engine
cannot enforce it**: the lock file is created and gates nothing, so the obligation is the
embedder's — at most one open writer per store directory, across every process and every WASM
instance. Two writers will interleave their write-ahead logs and corrupt the store, and detection
is not guaranteed. See [FORMAT.md](FORMAT.md#the-writer-lock).

## Platforms

The crate's **native** build is Unix only — it needs positioned reads, `flock`, and (for `punch`)
Linux hole punching.

`bindings/node` is the in-progress native server-side interface. Its `napi-rs` addon gives each open
writer a dedicated Rust actor and bounded queue, exposes Promise-based batch/durability/scan/content
operations with `Buffer` and exact `bigint`, and refuses to fall back to WASM when a native artifact
is unavailable. It is currently a source prototype; its README states the supported slice and the
remaining production gaps.

It also builds for **`wasm32-wasip1`**, which is what the [`turndb` npm package](npm/turndb)
ships: one `.wasm`, no native addon, no prebuild matrix, no postinstall. A store written by either
build is readable by the other, byte for byte. That target gives up three things, and the first is
the one that matters: **no advisory locking** (see above), no `punch` (`refold` reclaims the same
space by rewriting), and no threads, so compression runs inline. `src/sys.rs` is the single place
that states what turndb needs from an operating system and what happens where it isn't there.

## Testing

The engine is tested harder than its size suggests, because a storage engine that loses data is
worthless however elegant it is.

```sh
cargo test                              # 12 suites
cargo test --features dst --test dst    # 1,344 crash states, strict-POSIX durability model
cargo test --test corruption            # ~40k mutants across every parser
STORM_XOR=$RANDOM cargo test --test corruption   # fresh mutant space
```

The **deterministic simulation** harness records every write and fsync, then replays every crash
point under a model where file content and directory entries become durable independently, and
asserts the recovered store equals some prefix of the acknowledged writes. It found three real
bugs in its first hour, including an ACK that was backed by a WAL whose directory entry was not
yet durable.

The **corruption storm** mutates every on-disk structure and requires errors, never panics. It
found five parser bug classes, including bounds checks of the form `at + n > len` that *overflow
and therefore pass*.

## License

Apache-2.0. Copyright 2026 Efficacious, Inc. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
