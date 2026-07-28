# turndb

A content-addressed columnar store for AI traces. Embedded, single-writer, no daemon — a store is
a directory you can `tar`, and reading one needs nothing but the files.

**Status: pre-1.0, format version 1, not frozen.** See [FORMAT.md](FORMAT.md), which is normative:
where it and the code disagree, one of them is a bug.

## What problem it solves

Agent and chat APIs are stateless, so every request re-sends the whole conversation. A trace store
therefore receives the same message text over and over — on a real corpus, **38.9× duplication**.
General stores pay for those bytes repeatedly; archival dedup stores collapse them but stop being
queryable.

turndb splits the two planes:

* the **fold** holds content, addressed by BLAKE3 of its bytes, written once and never rewritten;
* **parts** hold record ids, body programs, and typed attribute columns — references, not bytes.

Because a part holds no content, **compaction never touches content**: merging rewrites references
and columns, which on trace data is a small fraction of the bytes. That is what lets a trace store
behave like a database instead of a write-once archive.

Measured on 40k records of real agent traffic (1.89 GiB of message bodies): 38.9× piece dedup,
**308× total on-disk collapse**, every record byte-exact.

## The cardinal invariant

**Byte-exact reconstruction.** Reading a record reproduces the original bytes exactly — attribute
order, duplicate keys, NaN payloads, `-0.0`. Everything else is in service of it.

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
| **Query** | DataFusion lens over the columnar plane — `body` is a projectable column, so attribute queries open zero fold blocks (measured, not asserted) |
| **Compaction** | Total merge at eight parts, chosen by benchmark; merges provably touch zero content bytes |
| **Deletion** | Tombstone → settle → re-fold removes content *and* metadata; `punch` reclaims dead blocks in place without moving a single offset |
| **Integrity** | Per-piece BLAKE3 on every read, per-section checksums, footer and TOC chains, manifest-pinned parts, and a `scrub` that walks every frame |
| **Shipping** | `pack` puts a whole store in one file that reads — and answers SQL — identically |

## What it does not do

No daemon, no network, no cluster, no consensus. One writer per store, enforced by `flock`;
readers need no lock and see a consistent committed snapshot. Scale-out is more stores, not a
bigger one. No encryption (the format reserves a flag bit and refuses it — see FORMAT.md). No
parity/erasure coding: corruption is detected at every level and repair is the storage layer's
job. Unix only.

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

TBD.
