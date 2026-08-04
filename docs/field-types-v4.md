# General scalar field types: part of format version 2

Version 2 completes TurnDB's initial self-described scalar field system without introducing a
global schema or trace-specific vocabulary. A record still carries an ordered sequence of
`(name, typed value)` attributes; duplicate names and their interleaving remain exact.

## Types

| Rust value | part/WAL tag | meaning |
|---|---:|---|
| `AttrValue::Str` | 0 | UTF-8 string |
| `AttrValue::Int` | 1 | signed 64-bit integer |
| `AttrValue::Float` | 2 | exact f64 bit pattern |
| `AttrValue::Bool` | 3 | boolean |
| `AttrValue::UInt` | 4 | unsigned 64-bit integer |
| `AttrValue::Bytes` | 5 | arbitrary binary metadata |
| `AttrValue::TimestampNs` | 6 | signed nanoseconds since the Unix epoch, UTC |
| `AttrValue::Null` | 7 | an explicit null occurrence |

Binary attributes are intended for compact queryable identifiers, hashes, and protocol metadata.
Large opaque values still belong in named content, where content addressing, deduplication, lazy
reconstruction, and physical erasure apply.

Lists, maps, and other nested documents are deliberately not scalar attributes in version 2. They
belong in named content under a consumer-chosen explicit encoding (for example canonical JSON, CBOR,
or protobuf), with ordinary scalar fields carrying any encoding/version metadata the consumer needs
to query. TurnDB never guesses that arbitrary bytes are JSON or normalizes a document during ingest.
A future native nested column type would require its own versioned tag and exact ordering/null rules;
it cannot silently reinterpret content written under this contract.

A timestamp has exactly one interpretation: its i64 is nanoseconds since `1970-01-01T00:00:00Z`.
TurnDB stores no local timezone and performs no implicit unit conversion. Consumers that receive
milliseconds or timezone-less civil time must normalize deliberately before writing.

## Missing and null

Missing and explicit null are different storage states:

- missing: no attribute occurrence with that name exists on the record;
- null: an occurrence exists at an exact position and has `AttrValue::Null`.

`attr_exists(name)` is true for explicit null. A structured equality predicate with a null literal
matches explicit null and never missing; ordering comparisons against null match nothing. Projection
returns the null occurrence, including duplicates and its position among other attributes.

The Arrow lens cannot use `DataType::Null` directly because both present and missing rows would then
be Arrow null and the distinction would disappear. It therefore exposes the null-type column as
`key#null`, a nullable boolean presence marker: `true` means explicit null and Arrow null means
missing. The suffix is used even when null is the only observed type so the boolean cannot be
mistaken for the consumer's field value.

## Column representation

Columns remain keyed by `(name, type tag)`. A name observed with several types becomes several
homogeneous columns rather than a union that rounds or guesses values.

- u64 and timestamps use fixed-width eight-byte little-endian values.
- Binary values use u32 ordinals into a byte-sorted distinct dictionary. They stay dictionary
  encoded through the Arrow boundary as `Dictionary(Int32, Binary)`.
- Explicit-null columns have zero value bytes. Their sparse/dense row-id stream and the per-row
  layout carry all required information.
- Numeric zones cover u64 and timestamps in their own ordering. Binary dictionary order already
  bounds binary values; null is unordered and has no zone.

Full part construction and streaming compaction use the same dictionaries and produce byte-identical
parts for the same logical rows. Compaction never decodes binary metadata as UTF-8 and never changes
the timestamp unit.

## Binding representation

Native Node keeps the discriminant explicit:

```ts
{ name: "count", kind: "uint", uintValue: 18446744073709551615n }
{ name: "trace_id", kind: "binary", binaryValue: Buffer.from(bytes) }
{ name: "at", kind: "timestamp_ns", timestampNsValue: 1710000000000000000n }
{ name: "result", kind: "null" }
```

Integers and timestamps cross as `bigint`; binary crosses as `Buffer`. SQL parameters use the same
`uint`, `binary`, and `timestamp_ns` kinds.

The portable package accepts `Uint8Array`, `null`, `{ u: bigint }`, and
`{ timestampNs: bigint }`. Its tagged JSON ABI uses decimal text for exact integers and arrays of
byte numbers for binary. Reads retain the unsigned/timestamp wrappers so a read-modify-write cycle
does not silently turn either type into signed i64.

## Compatibility

Part writers now emit version 2. Readers continue to accept versions 0 and 1; original tags
have unchanged bytes. WAL writers use record tags `0x5C`/`0x5D`. The legacy version-1 tags
`0x57`/`0x5A` retain their layout but accept only attribute tags 0 through 3. Older
binaries encounter either an unsupported part version or a checksummed unknown WAL frame and refuse
rather than skipping or mis-decoding committed records.
