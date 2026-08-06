# Differential query correctness

TurnDB has three public ways to observe a committed record: point reads, storage-native structured
scans, and the Arrow/DataFusion lens. Sharing storage code reduces their opportunity to disagree but
does not prove agreement. They have different projection machinery, predicate evaluation, paging,
and result shapes, so a plausible result from each path is weaker evidence than one shared oracle.

The test
`point_structured_and_datafusion_paths_agree_on_versioned_general_records` in `tests/query.rs`
constructs 48 logical ids across eight independently flushed mutation layers. A separate in-memory
map applies every put and delete as the reference live state. The store deliberately retains:

- many physical versions of the same id and newest tombstones;
- signed integers, finite floats, booleans, unsigned integers, binary fields, nanosecond timestamps,
  strings, and explicit null distinct from missing;
- duplicate attribute names in occurrence order;
- conventional `body` plus independently optional `attachment` content;
- repeated and unique bytes that exercise content addressing rather than an opaque payload column.

The gate first anchors every live and deleted id through point reads. It compares the exact ordered
attribute sequence and reconstructs each expected named content value byte-for-byte. Structured scans
then project the same fields and content through deliberately tight candidate, examination, and page
limits. The test requires multiple cursors, tombstone-only empty pages that still make progress,
superseded-row resolution, and complete forward and reverse sequences with no gaps or duplicates.

DataFusion projects every scalar column and both content columns from the same immutable snapshot.
Its flat view is compared with the reference field-by-field. For duplicate names, the documented
first occurrence is expected while the hidden occurrence must be counted. String equality, signed
integer ordering, content presence, and explicit-null predicates are each run through both structured
scans and SQL and compared with independently selected reference ids.

Run the gate directly with:

```sh
cargo test --test query point_structured_and_datafusion_paths_agree_on_versioned_general_records
```

This deterministic test is the initial three-path differential gate, not a complete query-testing
strategy. It does not replace randomized property generation, parser fuzzing, malformed-store mutation,
or differential coverage of future indexes and orderings. A new query-visible scalar type, visibility
rule, predicate class, or result representation should extend this corpus or add an equally strong
independent oracle before being considered stabilized.
