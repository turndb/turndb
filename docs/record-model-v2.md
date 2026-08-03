# General records and named content: format revision 2

Revision 3 retains this named-column model and adds exact whole-value identities in parallel
`con.id.N` sections. See [`content-identity-v3.md`](content-identity-v3.md); this document remains the
design and compatibility record for the revision-2 generalization.

Status: accepted for implementation on `codex/turn-maturity`.

This decision is the first format-bearing step in the roadmap. It describes the logical record model,
the compatibility boundary, and the physical direction. `FORMAT.md` remains normative for every format
the code actually writes; this document becomes historical design rationale once revision 2 is fully
specified there.

## Decision

A TurnDB record consists of:

- one non-empty UTF-8 record id;
- an ordered sequence of typed attributes, preserving duplicate keys and exact values; and
- zero or more uniquely named content values, each represented by a flat reconstruction program over
  inline literals and content-addressed fold pieces.

In Rust terms, the semantic shape is:

```rust
Record {
    id: String,
    contents: Vec<Content>,
    attrs: Vec<(String, AttrValue)>,
}

Content {
    name: String,
    ops: Vec<ContentOp>,
}
```

Content names are a map key, not an ordered or repeated field. They must be non-empty and unique within
a record. Readers return them in UTF-8 byte order, which is the canonical physical column order.
Attribute order and duplicate attribute keys remain significant and are reconstructed exactly.

An empty content value differs from an absent content value. Empty content has a column occurrence and
a zero-op program; absent content has no occurrence.

The existing public `body` behavior becomes a convenience over content named `body`. Revision-0 and
revision-1 parts are read as if every live record had exactly one content value named `body`, including
an empty body. Existing `put_body` and `reconstruct` methods remain convenience APIs during the
pre-1.0 transition, while the general APIs accept and select named content explicitly.

## Why content is not an attribute

Content identity is not a tag attached to an opaque binary attribute. Each named content value is a
program whose piece references address the shared fold. The fold remains involved in:

- deduplication during ingestion;
- byte-exact reconstruction;
- integrity verification on every content read;
- compaction, which rewrites programs and columns but not content;
- reference discovery for punch and refold; and
- erasure, which reasons about reachability across every named content value.

This is the property that a general columnar database with a hash column does not acquire merely by
storing the same fields.

## Part revision 2

Revision 2 replaces the singular per-row `prog`/`prog.off` pair with sparse, named content columns.
Content columns are assigned ordinals by sorted content name, independent of input order.

`cmeta` contains:

```text
varint  n_content_columns
repeated n_content_columns times:
  varint  name_len
  bytes   utf8_name
  varint  occurrences
  u8      rid_kind       0 dense, 1 ascending delta row ids
```

Each content column `N` contains:

- `con.prog.N`: concatenated reconstruction programs in occurrence order;
- `con.off.N`: `occurrences + 1` little-endian u64 offsets into `con.prog.N`; and
- `con.rid.N`: ascending delta-varint row ids when sparse, absent when dense.

The program encoding remains the current compact part encoding. Only its placement changes. All
content columns in one part reference the same fold-ordered `pdict`.

This layout was selected over a single per-row content layout because projection is the purpose of the
columnar plane. Reading `content.response` must not decompress programs for `content.request`, much
less reconstruct either value from the fold. The cost is two or three small sections per distinct
content name, which is bounded by the content-column universe rather than record count.

`cmeta` is required in revision 2 even when it declares zero columns. Every declared content column
requires its program and offset sections. Its row-id section is required exactly when the column is
sparse. Duplicate names, unsorted names, invalid row ids, inconsistent occurrence counts, bad offsets,
and trailing metadata are corruption and are refused.

Revision 2 no longer requires `prog` or `prog.off`. Revision-0 and revision-1 parts retain their existing
requirements and interpretation. The footer part version moves to 2, so older readers refuse new parts
before applying the old required-section rules.

## WAL revision

The WAL has no payload version byte, so a new record payload cannot reuse the current record tags. A
new standalone record tag and a new in-batch record tag introduce the generalized payload. Existing
record and tombstone tags retain their exact interpretation.

The revision-2 record payload is:

```text
varint  id_len
bytes   id
varint  n_contents
repeated n_contents times, in canonical name order:
  varint  name_len
  bytes   utf8_name
  varint  n_ops
  repeated n_ops times:
    u8      op             0 literal, 1 piece
    op 0:   varint len, then len bytes
    op 1:   32 byte piece hash, then varint len
varint  n_attrs
... current attribute encoding ...
varint  n_novel
... current novel-piece encoding ...
```

Novel piece bytes remain a record-level set. The same piece referenced from several content values is
carried once when new and not at all when already durable.

A reader that predates these tags verifies their frame checksums and refuses them as unknown rather
than dropping acknowledged records. A revision-2 reader replays both old and new record tags, adapting
the old singular body to named content in memory.

## Query model

Attributes remain ordinary typed query columns. Named content appears as a separate namespace so a
consumer-created attribute cannot collide with a synthesized content column. The logical naming
convention is `content.<name>`; API query specifications represent the namespace structurally rather
than requiring callers to concatenate strings.

A content projection has two useful modes:

- reference projection returns presence, reconstructed length, and content identity information
  without reading fold blocks; and
- value projection reconstructs the selected values and performs the existing per-piece verification.

The initial implementation may expose exact reconstruction before a stable aggregate identity for a
multi-piece program. It must not pretend that one piece hash is the identity of the whole value. If a
whole-value digest is added, it is BLAKE3 of the reconstructed bytes and is either stored explicitly or
computed with the documented I/O cost.

Metadata-only scans and scans of unrelated content columns must continue to open zero fold blocks.

## Visibility and durability

General records do not change TurnDB's newest-wins rule. The newest visible record for an id supplies
all of its attributes and content values; updates do not merge fields with older versions.

A successful durability acknowledgement makes the entire record, including every content value,
recoverable. Batch acknowledgement remains all-or-nothing. Point reads and structured scans through a
writer must include acknowledged and staged memtable records according to the same read-your-writes
contract; requiring a flush for query visibility is not acceptable for live consumers.

## Migration

TurnDB promises to read the immediately preceding format revision and refold it forward. The revision-2
implementation therefore:

1. reads revision-0 and revision-1 parts through the logical `body` content adapter;
2. replays both old and new WAL frames;
3. writes revision-2 parts for every new flush, merge, and refold; and
4. rewrites old parts as revision 2 during ordinary merge or explicit refold.

A store may temporarily contain revision-1 and revision-2 parts. Version resolution operates on their
logical records and is independent of their physical content layout.

Downgrading after the first revision-2 part is published is refused by the old reader's existing part
version check. No manifest flag is required to make that refusal safe.

## Initial scope

This revision generalizes content without simultaneously expanding the attribute type system. The
existing string, i64, f64-bits, and boolean encodings remain unchanged. Unsigned integers, binary
metadata, timestamps, null, and structured attribute values require their own semantic and format
decisions; coupling them to named content would make one migration answer several independent
questions and enlarge the corruption surface unnecessarily.

Likewise, this revision does not add SQL syntax, OTel conventions, retention policy, authorization,
network ingestion, or product-specific record kinds.

## Rejected alternatives

### Store hashes in ordinary binary columns

Rejected because it makes content addressing an application convention. Compaction, integrity,
reachability, and erasure would have to rediscover which byte fields happen to be references.

### One serialized body containing all content fields

Rejected because selecting one content value would require reading and parsing every other content
value and would destroy independent deduplication.

### One per-row content program with embedded names

Rejected because a projection of one content name would decompress the shared program section for all
names. It is row-shaped storage behind a columnar API.

### Reuse the existing WAL record tags

Rejected because an old reader would parse the content count as the body-op count and could accept a
wrong record before reaching an error. New checksummed tags provide an unambiguous reject-forward
boundary.

### Add DataFusion directly to the WASM package first

Rejected as the first step because artifact size is not the only lost capability. It would not restore
native advisory locking, hole punching, threads, or event-loop isolation. The general record and query
contracts must exist independently of a particular binding target.
