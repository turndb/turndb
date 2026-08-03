# Structured scan explanation

`Store::explain_scan` and `ReadStore::explain_scan` prepare a storage-native `ScanRequest` without
resolving newest-wins rows or evaluating predicates. The native Node spellings are
`NativeStore.explainScan()` and `NativeSnapshot.explainScan()`.

Explanation and execution call the same request preparer. Consequently, explanation validates the
same limits, field names, projection rules, cursor checksum, cursor direction, and cursor request
fingerprint. A continuation produces the same effective inclusive lower and exclusive upper bounds
that the next page will use. Changing bounds or predicates under a cursor is refused rather than
described as a different plan.

## Logical field plan

The response separates:

- projected attributes from all required attributes;
- attributes needed only by predicates;
- projected named content, retaining request order and metadata/bytes mode;
- all required content names and content needed only for presence predicates;
- content names that may be reconstructed after predicate acceptance and byte-budget admission;
- counts of id, attribute, and content predicate forms.

Required name lists are canonical sorted sets. Attribute projection is name-based and still gathers
every physical `(name, scalar type)` column encountered in requested rows. Explanation does not claim
that a semantic predicate is an index or that a byte-selected value will necessarily be present.

The effective `limit`, `max_examined`, `max_resolution_entries`, and
`max_reconstructed_bytes` are returned verbatim so an embedder can log or reject a plan without
reimplementing defaults.

## Physical scope

Physical scope is exact for the effective id range at the handle's consistency point:

- `immutable_parts_considered` is the number of parts whose sorted id range is initialized;
- `immutable_parts_with_rows` is the subset containing at least one row in range;
- `immutable_rows_in_bounds` includes every physical occurrence, including older versions and
  tombstones;
- `memtable_entries_in_bounds` includes staged puts and deletes for a writer and is zero for an
  immutable snapshot.

These are pre-resolution work facts, not estimated visible candidates or result cardinality. The
gap between physical occurrences and later `stats.resolution`/`stats.examined` is useful evidence of
version amplification and predicate selectivity.

The writer method is actor-ordered: it describes the read-your-writes view at its command position.
An immutable reader describes its pinned manifest. Explanation honors scan cancellation/deadlines
before and after physical scope collection and returns no partial explanation.

## Read behavior and non-guarantees

To produce exact range scope, explanation performs the same binary-search range initialization over
every live part. It opens sorted id and restart structures and may warm those shared caches. It does
not open tombstones, attribute layout/value/rid/dictionary sections, named-content metadata/programs,
or fold blocks. It performs no content reconstruction and makes no durable mutation.

Explanation is not a cost optimizer, timing estimate, secondary-index recommendation, or promise of
which row will match. Future indexes or encoded predicate execution can extend the physical plan,
but consumer schemas and telemetry policy remain outside this contract.
