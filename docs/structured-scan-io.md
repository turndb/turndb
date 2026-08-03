# Structured scan I/O statistics

Every successful structured page returns exact operation-local storage accounting in `stats.io`.
The purpose is to make projection and cache behavior diagnosable without making the consumer sample
process-global cache counters or parse logs.

## Contract

Part statistics are:

- `part_sections_touched`: distinct `(part, section)` pairs that reached the raw-section cache;
- `part_section_cache_hits` and `part_section_cache_misses`: raw-section cache access counts;
- `part_stored_bytes_read`: compressed section bytes requested from a part's backing `ReadAt` source
  on misses;
- `part_raw_bytes_decoded`: uncompressed section bytes produced by those misses.

Fold statistics are:

- `fold_blocks_touched`: distinct logical block ids containing selected pieces consulted by the
  reconstruction path;
- `fold_block_cache_hits` and `fold_block_cache_misses`: decompressed block-cache access counts for
  blocks already sealed into fold segments;
- `fold_stored_bytes_read`: complete stored fold frame bytes, including the frame header, requested
  from segment readers on misses;
- `fold_raw_bytes_decoded`: uncompressed block bytes produced by those misses.

“Distinct” and “access count” are intentionally separate. Several selected pieces may share one fold
block: that is one block touched and potentially several cache hits. Likewise, multiple column
helpers may access the same raw part section during a page.

A backing-reader byte is an engine I/O request, not a claim about physical media traffic. The OS page
cache, a remote `ReadAt` implementation, or an embedding VFS may satisfy it below TurnDB. Raw decoded
bytes measure codec work and cache admission, not bytes retained in the returned page.

## Cache and concurrency semantics

Part section caches and fold block caches are shared by immutable snapshots. Global before/after
counter subtraction would therefore be contaminated by another snapshot reading concurrently.
TurnDB instead installs an internal scope around the synchronous Rust scan call. Low-level part and
fold readers report only to that scope, and nested scopes are isolated. A concurrent scan cannot add
its reads to this page's statistics.

Higher-level decoded part caches can satisfy some work before the raw-section cache is reached. A
warm page may consequently report zero part sections touched even though it projected fields. That
means no raw section access occurred in this operation; it does not mean the query had no logical
columns. Fold blocks still count as touched when a piece is served from the open writer block or the
compression pipeline, but those in-memory sources are neither fold block-cache hits nor stored-byte
reads.

Statistics are returned only with a successful page. Cancellation, deadline, corruption, and other
errors return no partial result and therefore no partial I/O report.

## Binding representation

Rust exposes `ScanIoStats` under `ScanStats::io`. The native Node binding exposes the same field names
in camel case and uses `bigint` for every value. Counts are represented as `bigint` too, avoiding a
future precision split between counters and byte totals.

These metrics describe the storage-native structured pager. They do not instrument SQL execution,
report elapsed time, measure OS-level I/O, or replace the constant-work whole-store health snapshot.
`explain_scan` separately describes logical requirements and pre-resolution physical scope; its own
id-structure reads are not returned as page I/O statistics and may warm caches before a later page.
