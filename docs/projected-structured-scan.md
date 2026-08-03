# Projected structured scans

The feature-independent structured pager now resolves visibility first and decodes only the fields
needed by projection or predicates. This is the storage-native small-query path; it does not route
through Arrow or DataFusion and does not require a flush to see the writer's memtable.

## Read shape

For each candidate id, TurnDB builds two name sets:

- attribute names explicitly projected plus names used by attribute predicates;
- content names explicitly projected plus names used by content-presence predicates.

The committed read core locates the newest part row for the id, honors a tombstone before projection,
and then constructs a partial semantic record from those sets. Attribute layout order and duplicate
occurrences remain exact. A field used only by a predicate participates in evaluation but is not
returned unless it was also projected. The same rule keeps predicate-only content out of result rows.

The writer checks its memtable first. A staged record is already resident as a semantic value, so it
is filtered in memory; a staged tombstone returns absence. Only ids not present in the memtable reach
the committed projected-read path. This preserves read-your-writes and ensures a rejected predicate
can never reveal an older committed version.

## Physical sections

Part projection always needs shared structural metadata such as ids, tombstones, attribute layout and
column metadata. After reading that structure:

- row-id, value, and string/binary dictionary sections open only for selected or predicate-bearing
  attribute names;
- program, offset, row-id, and identity sections open only for selected or predicate-bearing content
  names;
- sibling attribute and content value sections remain unopened;
- content byte projection reconstructs only the selected values;
- metadata-only content projection opens no fold blocks.

This is stronger than filtering a fully decoded point record after the fact. The regression test
damages an unselected compressed attribute value section and an unselected compressed content program
section. Selecting their healthy siblings succeeds; selecting either damaged field reaches and
rejects that section.

## Semantics preserved

Projection does not change:

- newest-wins and tombstone resolution;
- exact scalar types, float bit patterns, duplicate fields, or selected-field order;
- missing versus explicit null;
- forward/reverse checked-cursor pagination;
- live memtable visibility;
- examination and reconstructed-content byte budgets;
- cooperative cancellation/deadline behavior.

The existing structured scan request needs no compatibility adapter. The optimization lives behind
the Rust query contract rather than in a binding.

## Remaining cost gap

The pager now retains the authoritative part and row found by its bounded k-way id-range merge. It
projects that row directly and reuses an already projected content program during reconstruction,
rather than point-locating the id and decoding the program again. See
[resolved-row structured paging](resolved-row-paging.md).

It is not yet vectorized physical column execution: globally ordered candidates still invoke
selected-column decoders one row at a time rather than being gathered by part and decoded as a
batch. Query statistics report exact operation-local distinct sections and fold blocks, cache access
counts, and stored/raw bytes. See [structured scan I/O statistics](structured-scan-io.md). A grouped
physical batch primitive remains the next Phase-2 opportunity and requires no second index or format
change.
