# Resolved-row structured paging

The structured pager carries each candidate's authoritative storage origin out of the bounded range
merge, so projection and content reconstruction index directly into the resolved part and row
rather than repeating newest-first point searches.

## Published range resolution

Every immutable part contributes its contiguous `[from, to)` id run. The published read core performs
a bounded k-way merge over those sorted runs. Equal ids are grouped and the newest contributing part
decides; a tombstone suppresses the id. A resolved result now contains:

- the id;
- the immutable part index;
- the row ordinal in that part.

The part index is valid for the lifetime of the `Store` or `ReadStore` borrow that produced the batch.
It is an internal execution reference, not a durable locator, cursor field, or public storage address.
The checked continuation remains based only on the last consumed id and the request fingerprint.

`scan_ids` is now a projection of this same resolver, so the id-only and structured paths cannot
quietly acquire different visibility rules.

## Writer pending change set

A writer's ordered pending change set participates as one more source in the same bounded k-way range merge
as the immutable parts, so pending record versions and tombstones resolve through the identical
newest-wins pipeline — there is no separate overlay pass or materialized pending-change-set range (see the
"Hard resolution bound" section below). Projection of a
pending origin remains entirely in memory and preserves read-your-writes.

The scan call holds an immutable borrow of the writer, so a resolved pending origin cannot change
between range resolution and projection. The native actor additionally serializes writer commands.

## Projection and reconstruction

Published projection indexes directly into the resolved part and row, then opens only named
attribute/content columns required by projection or predicates. It does not search the id column
again. Byte projection also reuses the `Content` program and whole-value identity already decoded for
predicate/projection work. Reconstruction resolves its piece hashes through that same part's piece
dictionary and verifies the final whole-value identity; it does not decode the selected content
program or identity section a second time.

Pending byte projection similarly reconstructs from the already projected `Content`, using the
writer's two-tier piece locator. Exact byte reconstruction, content hashing, duplicate attributes,
tombstones, cursor behavior, and reconstruction budgets are unchanged.

## Resolution statistics

Every successful page reports exact pre-predicate work in `stats.resolution`:

- `physical_rows`: immutable part-row occurrences consumed by id groups during k-way resolution;
- `superseded_rows`: older occurrences hidden by the deciding occurrence for the same id;
- `tombstones`: deciding immutable tombstones that yielded no resolved candidate;
- `memtable_entries`: ordered pending-change-set entries inspected in the requested range.
- `budget_exhausted`: resolution stopped at a complete id-group boundary because the configured
  ceiling was reached while more source entries remained.

These are operation-local counters, not estimates derived from part metadata. They include resolver
over-fetch even when the scan later stops on a result limit or predicate budget, because that work
actually occurred. `stats.examined` remains the number of resolved candidates evaluated against
predicates; it deliberately does not pretend to measure physical version-resolution work. Native Node
uses `bigint` for all four counters and a boolean exhaustion flag.

## Hard resolution bound

`ScanRequest::max_resolution_entries` bounds the sum of immutable part-row occurrences and pending-change-set
entries consumed by one page before predicate evaluation. It defaults to 1,000,000 and is accepted in
`1..=10,000,000`. Native Node exposes the same option as `maxResolutionEntries: number` and publishes
the compiled default and maximum in capabilities.

An equal-id group is atomic because every occurrence must be considered before newest-wins can be
decided. TurnDB stops before a group that would cross the remaining ceiling. As with the content-byte
budget, the first group is admitted whole even when it alone exceeds the ceiling, so an id repeated
across many parts cannot deadlock pagination.

The resolver returns the last complete id group it consumed independently of whether that group
produced a resolved row. The ordinary opaque cursor uses that id boundary. Consequently, a page may be
empty and still carry `next`: it made real, bounded progress across tombstone-only groups. Forward and
reverse traversal use the same rule, and changing the resolution ceiling between pages does not
invalidate the cursor.

The writer's pending change set participates as the newest ordered source in the same k-way merge. A pending record version
costs one resolution entry and is decided atomically with every immutable occurrence of that
id, and no longer requires materializing the complete requested pending-change-set range before paging.

## Projection handoff and remaining work

The bounded resolved-row batch now feeds the [grouped column gather](grouped-column-gather.md).
Immutable rows are grouped by part, selected physical decoders are shared within a bounded chunk, and
records are restored to global id order before predicate evaluation. Pending writer candidates keep
their direct in-memory projection path.

Range setup still visits every part referenced by the current manifest revision to find its `[from, to)` bounds. Predicates remain
row-oriented rather than vectorized over encoded columns. Secondary indexes, alternate sort orders,
logical query plans, and SQL execution are separate roadmap work. This slice changes no on-disk
format and adds no consumer-specific semantics.
