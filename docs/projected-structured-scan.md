# Projected structured scans

The feature-independent structured pager resolves visibility first and decodes only the fields
needed by projection or predicates. This is the storage-native small-query path; it does not route
through Arrow or DataFusion and does not require a flush to see the writer's pending change set.

## Read shape

For each candidate id, TurnDB builds two name sets:

- attribute names explicitly projected plus names used by attribute predicates;
- content names explicitly projected plus names used by content-presence predicates.

The published read core locates the newest part row for the id, honors a tombstone before projection,
and then constructs a partial semantic record from those sets. Attribute layout order and duplicate
occurrences remain exact. A field used only by a predicate participates in evaluation but is not
returned unless it was also projected. The same rule keeps predicate-only content out of result rows.

Float predicates follow the store's byte-exactness rather than pure IEEE semantics: `Eq`/`Ne`
compare bit patterns (the exact stored NaN payload matches; `-0.0` and `0.0` are distinct), while
the ordering operators use IEEE partial order (no NaN satisfies any inequality; `-0.0` orders equal
to `0.0`). Consequently `Eq` does not imply `LtEq`; IEEE equality is expressible as `LtEq && GtEq`.

The writer checks its pending change set first. A pending record version is already resident as a semantic value, so it
is filtered in memory; a pending tombstone returns absence. Only ids not present in the pending change set reach
the published projected-read path. This preserves read-your-writes and ensures a rejected predicate
can never reveal an older record version from a selected manifest revision.

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
- pending-change-set visibility;
- resolved-candidate examination, pre-predicate resolution, and reconstructed-content byte budgets;
- cooperative cancellation/deadline behavior.

The existing structured scan request needs no compatibility adapter. The optimization lives behind
the Rust query contract rather than in a binding.

## Grouped physical gather

The pager retains the authoritative part and row found by its bounded k-way id-range merge. It
projects that row directly and reuses an already projected content program during reconstruction,
rather than point-locating the id and decoding the program again. See
[resolved-row structured paging](resolved-row-paging.md).

Resolved immutable candidates are gathered by physical part in chunks of at most 64, then restored to
their original global id order. Within each part gather, attribute layout and column metadata are
parsed once; each selected rid, value, and dictionary section is opened once. Named-content metadata,
sparse row ids, programs, offsets, and identities follow the same rule. Program decoding remains
row-selective: a gather never materializes a whole content column.

The chunk is also capped by remaining output demand, so a limit-one request does not decode read-ahead
rows. Every gathered candidate is therefore semantically examined before a full page can stop;
predicate rejection triggers another gather. Cancellation and deadline checks bracket each chunk and
each semantic row.

This is a grouped sparse gather, not SIMD expression execution or a materialized Arrow batch.
Predicates still evaluate against partial semantic records one row at a time, and sparse occurrence
lookup remains directly indexed per requested row. Query statistics report exact operation-local
distinct sections and fold blocks, cache access counts, and stored/raw bytes. See
[structured scan I/O statistics](structured-scan-io.md) and
[grouped column gather](grouped-column-gather.md).
