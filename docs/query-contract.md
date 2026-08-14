# Structured query contract v1

Status: normative Phase 3 contract. The storage-native structured scan is TurnDB's portable query
surface. SQL is an optional lens over the same visibility and value semantics.

The machine-readable request and result representation is defined by
[`conformance/v1/query.schema.json`](../conformance/v1/query.schema.json). JSON is the conformance
and transport representation; bindings translate it to native values without changing meaning.

## Scalar representation

Attribute occurrences are ordered `{ name, value }` entries. An object/map is not the canonical
shape because it cannot preserve duplicate names or their interleaving.

| TurnDB value | contract JSON | JavaScript | Python |
|---|---|---|---|
| UTF-8 string | `{ "type": "string", "value": "..." }` | `string` | `str` |
| signed i64 | `{ "type": "i64", "decimal": "-1" }` | `bigint` | `int` |
| bit-exact f64 | `{ "type": "f64", "bitsHex": "8000000000000000" }` | `number` plus boundary tagging where needed | `float` plus boundary tagging where needed |
| boolean | `{ "type": "bool", "value": true }` | `boolean` | `bool` |
| unsigned u64 | `{ "type": "u64", "decimal": "18446744073709551615" }` | explicitly tagged `bigint` | explicitly tagged `int` |
| binary | `{ "type": "binary", "base64": "AP8=" }` | `Buffer`/`Uint8Array` | `bytes` |
| UTC Unix nanoseconds | `{ "type": "timestampNs", "decimal": "-1" }` | explicitly tagged `bigint` | explicitly tagged `int` |
| explicit null | `{ "type": "null" }` | `null` | `None` |

The native Node attribute lane accepts `floatBits` with this same sixteen-hex encoding and emits it
for NaNs, whose payload a JavaScript `number` cannot reliably carry. `floatValue` remains the
ordinary finite/infinite/signed-zero representation. If both are supplied they must agree (for NaN,
the explicit bits are authoritative); ambiguity is `INVALID_ARGUMENT`.

`decimal` is canonical base-10: `0` or an optional leading `-` followed by a non-zero digit and
digits. A leading `+`, leading zeroes, exponent, fraction, or negative unsigned value is invalid.
`bitsHex` is exactly sixteen lowercase hexadecimal digits encoding the stored `u64` bits in normal
hexadecimal notation. It therefore preserves every NaN payload and distinguishes `-0.0` from
`0.0`. Binary uses padded RFC 4648 base64.

Missing and explicit null are distinct. A missing attribute has no occurrence. Explicit null is an
ordered occurrence whose value has type `null`.

## Request

Bounds are over UTF-8 byte order. `from` is inclusive and `to` exclusive. An omitted bound is
unbounded. `direction` is `forward` or `reverse`; it changes order, not the half-open range.

`cursor`, when present, is opaque and created by Rust. It is valid only with the same bounds,
direction, and predicates. Projection and `limit` may change between pages. A malformed cursor or a
semantic mismatch is `INVALID_ARGUMENT`, never an empty page or a restart from the beginning.

`attrs` selects attribute names; every occurrence of a selected name is returned in original record
order. `contents` selects named content independently in `metadata` or `bytes` mode. Metadata reports
presence, reconstructed length, piece count, and whole-value BLAKE3 when the record format carries
it, without opening fold blocks. Bytes mode additionally reconstructs and verifies the exact value.
Absent content is represented explicitly with `present: false`; present empty content has length and
piece count zero and bytes equal to empty base64.

Predicates are typed:

- `id` compares an id with a UTF-8 string;
- `attr` compares occurrences of one attribute name with one exact scalar type;
- `attrExists` tests whether any occurrence of a name exists;
- `contentExists` tests named-content presence.

Visibility resolves before predicates. The newest physical occurrence of an id wins; a newest
tombstone makes the id absent, and a predicate cannot reveal an older version. A writer scan overlays
its memtable and provides read-your-writes. An immutable snapshot sees exactly its published cut.

For floats, `eq`/`ne` compare stored bits, so NaN payloads can match and the two zero signs differ.
Ordering uses IEEE partial order: no NaN satisfies an inequality and the two zero signs order equal.
Predicates of a different scalar type do not coerce a stored value.

## Paging and work bounds

`limit` bounds returned rows. `maxExamined` bounds live records evaluated against predicates.
`maxResolutionEntries` bounds physical newest-wins work; complete equal-id groups are atomic and the
first oversized group is admitted so progress cannot deadlock. A tombstone-only page may contain no
rows and still return `next`.

`maxReconstructedBytes` bounds selected content bytes retained by one page. A row is never split or
truncated. The first matching row is admitted even when it alone exceeds the limit. Otherwise, the
row that would cross the limit remains unconsumed and `next` resumes before it.

A successful page is complete for the work it reports. Cancellation or deadline expiry returns
`CANCELLED` and no partial page.

## Result and statistics

Rows are in id order for the selected direction. Each row contains its id, ordered projected
attribute occurrences, and projected contents in request order. Exact counters and byte sizes use
canonical decimal strings in JSON so no transport rounds them through IEEE-754.

The statistics distinguish:

- returned and predicate-examined live rows;
- live rows proven impossible from part metadata before value projection;
- duplicate projected attribute occurrences;
- content reconstruction work and its byte ceiling;
- physical versions, superseded rows, tombstones, and writer-memtable entries used for resolution;
- part sections and fold blocks touched, cache hits/misses, stored bytes read, and raw bytes decoded.

A metadata-only scan must report zero fold blocks and zero fold stored/raw bytes. Projecting one
named content value must not open sibling content program sections. These are contract properties,
not optional optimizations, because remote readers turn each unnecessary read into network traffic.

Durations are evidence rather than deterministic conformance values and therefore do not appear in
golden result equality.

## Errors and extensions

Invalid structure, inverted bounds, invalid values, cursor damage, and cursor mismatch are
`INVALID_ARGUMENT`. Declared work or memory ceilings use `RESOURCE_EXHAUSTED` except where this
contract explicitly returns a partial page and cursor. Persisted integrity violations use
`CORRUPTION`; absence of a record/content value is an ordinary result, not `NOT_FOUND`.

Contract-v1 objects may gain optional fields. Consumers ignore fields they do not understand.
Changing ordering, visibility, scalar encoding, cursor binding, or an existing field's meaning
requires a new contract version.

## Conformance

[`conformance/v1/corpus.json`](../conformance/v1/corpus.json) is an independent sequence of writes
and expected logical views. It exercises every scalar, duplicate attributes, missing versus null,
empty and absent content, updates, tombstones, writer overlay, immutable snapshots, forward/reverse
paging, cursor validation, and metadata-only I/O. Rust, Node, Python, and browser runners consume the
same file; binding-specific tests cover only scheduling and runtime mechanics around it.

[`conformance/v1/fixture.turndb.hex`](../conformance/v1/fixture.turndb.hex) is the same corpus after
its final publication, transported as reviewable lowercase hex. The Rust gate proves that replaying
the operations produces those exact container bytes and that opening those bytes reproduces the
published-v2 view. Read-only runners materialize the hex as a `.turndb`; they do not need a writer.
