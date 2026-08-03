# Resolved-row structured paging

The structured pager carries each candidate's authoritative storage origin out of the bounded range
merge. It no longer discards that information and repeats newest-first point searches during
projection and content reconstruction.

## Committed range resolution

Every immutable part contributes its contiguous `[from, to)` id run. The committed read core performs
a bounded k-way merge over those sorted runs. Equal ids are grouped and the newest contributing part
decides; a tombstone suppresses the id. A live result now contains:

- the id;
- the immutable part index;
- the row ordinal in that part.

The part index is valid for the lifetime of the `Store` or `ReadStore` borrow that produced the batch.
It is an internal execution reference, not a durable locator, cursor field, or public storage address.
The checked continuation remains based only on the last consumed id and the request fingerprint.

`scan_ids` is now a projection of this same resolver, so the id-only and structured paths cannot
quietly acquire different visibility rules.

## Writer overlay

A writer overlays its ordered memtable after committed rows have been resolved. A staged record
replaces any committed origin with a memtable origin; a staged deletion removes it. The existing
bounded over-fetch accounts for committed ids hidden by staged deletions, and the merged result is
then truncated in the requested direction. Projection of a memtable origin remains entirely in
memory and preserves read-your-writes.

The scan call holds an immutable borrow of the writer, so a resolved memtable origin cannot change
between range resolution and projection. The native actor additionally serializes writer commands.

## Projection and reconstruction

Committed projection indexes directly into the resolved part and row, then opens only named
attribute/content columns required by projection or predicates. It does not search the id column
again. Byte projection also reuses the `Content` program and whole-value identity already decoded for
predicate/projection work. Reconstruction resolves its piece hashes through that same part's piece
dictionary and verifies the final whole-value identity; it does not decode the selected content
program or identity section a second time.

Memtable byte projection similarly reconstructs from the already projected `Content`, using the
writer's two-tier piece locator. Exact byte reconstruction, content hashing, duplicate attributes,
tombstones, cursor behavior, and reconstruction budgets are unchanged.

## What remains

This is a bounded resolved-row batch, not yet vectorized column execution. Rows retain global id
order across parts and each row currently invokes the selected-column decoders separately. Those
decoders share section and decoded-column caches, but a future physical batch primitive can group
resolved ordinals by part, decode/gather selected columns once, and restore global result order.

Range setup still visits every live part, and the writer overlay currently counts staged deletions in
the requested range before choosing committed over-fetch. Secondary indexes, alternate sort orders,
logical query plans, and SQL execution are separate roadmap work. This slice changes no on-disk
format and adds no consumer-specific semantics.
