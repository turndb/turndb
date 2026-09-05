# TurnDB physical format

**Status: draft epoch 1; not frozen.**

This document is normative. Where it and the implementation disagree, one of them is wrong.

TurnDB currently has one physical format. It is identified by the exact magic values and draft
epoch specified here. These are identities, not compatibility ranges. A reader accepts exactly the
current identity and refuses everything else. No alternate layout, wrapper, migration path, or
implicit upgrade is part of the model.

While the format remains unfrozen, an incompatible physical change replaces this specification,
rotates the affected magic, and replaces the current fixtures. It does not create a promise to read
the bytes it superseded.

All fixed-width integers are little-endian. Varints are canonical unsigned LEB128, seven payload
bits per byte, least-significant group first: at most ten bytes for u64, no unrepresentable high
bits, and no redundant leading zero group. Unknown flags, tags, epochs, non-zero reserved bytes, malformed
lengths, overlaps, and trailing bytes are refused unless this document explicitly calls a field
advisory.

## Store shape

A store is one container file. A writer with accepted but unpublished mutations may also have one
WAL sidecar:

```text
trace.turndb       container: bytes accumulated across container-state publications and retained manifest revisions
trace.turndb-wal   accepted-mutation replay input and its durability frontier; absent after a clean close
```

Temporary files used by backup, restore, reclaim, and merge are protocol state, not alternate store
formats. Their exact current names are documented in `docs/lifecycle-control.md` and recognized by
the debris inventory. No suffix changes the meaning of the container itself.

A writer takes the exclusive operating-system lock on the container file. Unix and Windows release
that lock when the handle closes or the process exits. WASI has no equivalent; there the embedder
must enforce at most one writer for a store across every process and instance. Readers take no lock.

The data plane has two kinds of immutable evidence:

- the fold stores content pieces, addressed by BLAKE3 identity;
- parts store record versions, content programs, piece references, and attribute columns.

After the first publication, the manifest revision selected by the current container state is the
authority that connects them. Before then, the canonical sequence-zero birth state represents the
empty store authority without a manifest revision. The container superblock flip is the publication
point.

## Fold

### Container member namespace

Fold generation zero has the exact prefix `fold`; generations 1 through 9999 have the exact prefix
`fold-NNNN`, with four zero-padded decimal digits. `fold-0000`, wider numbers, signs, and alternate
spellings do not denote a generation.

Under that prefix, the fold namespace contains only these exact member forms:

- `seg-NNNNNNNN.fold` — a segment, with eight zero-padded decimal digits;
- `seg-NNNNNNNN.dir` — that segment's optional advisory directory sidecar;
- `zdict-HHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHH.zd` — a
  dictionary named by exactly 64 lowercase hexadecimal BLAKE3 digits.

Segment and sidecar numbers must agree. A sidecar without its segment is malformed. A segment that
declares a nonzero dictionary id requires the exactly named dictionary member. Any other name inside
the selected generation namespace is refused.

### Segment header

A fold segment is an append-only member named `seg-%08u.fold`; within a container it is under a
fold-generation prefix. Segment numbers are parsed numerically and must be dense.

```text
offset  size  field
     0     8  magic = "TDBFLD01"
     8     4  segment number
    12     4  flags, must be zero
    16    32  dictionary id: BLAKE3 of the zstd dictionary, or all zero
    48        first block frame
```

Flag bit 0 is reserved for encryption and is refused with a specific unsupported-encryption error.
Every other non-zero bit is unknown and refused.

### Block frame

```text
offset        size  field
     0           1  tag = 0xA5
     1           1  codec: 0 stored, 1 zstd, 2 zstd with segment dictionary
     2           4  raw bytes
     6           4  stored payload bytes
    10           2  first two bytes of BLAKE3 over the raw bytes
    12           4  block id
    16      stored  payload
16+stored        4  first four bytes of BLAKE3 over header and stored payload
```

Block ids are logical and need not match physical order. A piece location is three u32 values:
`block_id`, `in_block_offset`, and `raw_length`. Every piece read is checked against the BLAKE3
piece identity stored in a part. A block's raw length is nonzero; an empty physical block is not a
current-format frame.

### Segment directory sidecar

`seg-NNNNNNNN.dir` is an advisory index for the corresponding segment:

```text
offset  size  field
     0     8  magic = "TDBSDR01"
     8     4  segment number
    12     4  segment tail described by this index
    16     4  entry count
    20   n*8  repeated block-id u32, offset u32 pairs
20+n*8     4  crc32 over every preceding byte
```

A missing, damaged, mismatched, or structurally impossible sidecar is ignored and the authoritative
segment is scanned. A sidecar or described block that exceeds the caller's explicit runtime
`ReadLimits` is refused before allocation; fallback never weakens that policy. Partial sidecar
information is never trusted.

## Parts

A part is immutable, self-contained, and sorted by record id:

```text
[ section ][ section ] ... [ compressed TOC ][ 56-byte footer ]
```

Every offset inside a part is relative to the part's first byte. The same bytes therefore open as a
standalone test artifact, a container extent, or a remote range.

### Footer

```text
offset  size  field
     0     8  magic = "TDBPRT01"
     8     8  TOC offset
    16     4  stored TOC bytes
    20     4  raw TOC bytes
    24     4  record count
    28     8  inclusive sequence low
    36     8  inclusive sequence high
    44     1  TOC codec: 0 stored, 1 zstd
    45     1  draft epoch = 1
    46     4  crc32 of the stored TOC payload
    50     2  reserved, must be zero
    52     4  first four bytes of BLAKE3 over footer[0..52]
```

The footer is the completeness marker. Its magic and epoch must match exactly. Both sequence bounds
are inclusive and `low <= high`.

### Table of contents

The TOC is compressed according to the footer and decodes as:

```text
varint  section count
repeated section count times:
  varint  UTF-8 name length
  bytes   name
  varint  payload offset
  varint  stored bytes
  varint  raw bytes
  u8      codec: 0 stored, 1 zstd
  u32     crc32 of the stored payload
```

Section names are unique. Every section ends at or before the TOC offset and sections do not
overlap. The TOC must have no trailing bytes. Section checksums are verified by explicit
verification; bounded query reads need not hash an entire section they touch only partially.

### Required and conditional sections

These sections are required, including when their logical contents are empty:

| name | contents |
|---|---|
| `ids` | front-coded, strictly increasing record ids |
| `ids.restart` | little-endian u32 offsets, one every 16 ids |
| `cmeta` | named content-column metadata |
| `pdict.loc` | 12-byte piece locations, sorted in fold order |
| `pdict.hash` | 32-byte piece hashes parallel to `pdict.loc` |

Conditional sections are exact:

| name | condition |
|---|---|
| `con.prog.N`, `con.off.N`, `con.id.N` | content column N exists |
| `con.rid.N` | content column N is sparse; absent when dense |
| `layout`, `layout.off`, `colmeta` | any record has an attribute |
| `col.val.N` | attribute column N exists |
| `col.rid.N` | column N uses delta row ids; absent when dense |
| `col.dict.N` | column N is string or binary |

Advisory sections are `pdict.hsort`, `pdict.bloom`, and `zone`; their absence can cost work but
cannot change a logical answer. `tomb` is optional because absence means no row is a tombstone.
Every other section name is unknown and refused. Adding a section or changing a known section's
meaning requires a new physical identity.

The current writer places stored section payloads contiguously from part offset zero in this exact order:
`ids`, `ids.restart`, `cmeta`; each content column's `con.prog.N`, `con.off.N`, `con.id.N`, then
conditional `con.rid.N`; `pdict.loc`, `pdict.hash`, `pdict.hsort`, `pdict.bloom`; conditional
`tomb`; then, when attributes exist, `layout`, `layout.off`, `colmeta`, `zone`, followed for each
attribute column by `col.val.N`, conditional `col.rid.N`, and conditional `col.dict.N`. The TOC
preserves that order. A reader accepts any TOC order and gaps between non-overlapping sections;
those are placement differences, not new section semantics.

`pdict.loc` contains one 12-byte little-endian `(block_id, in_block_offset, raw_length)` tuple per
piece in strict fold-location order; `raw_length` is nonzero. `pdict.hash` contains the corresponding
32-byte piece identities, and every identity in this base dictionary is distinct whether or not the
advisory indexes are present. `pdict.hsort`, when present, is a parallel array of little-endian u32 piece ordinals,
each ordinal appearing exactly once, ordered by the corresponding piece identity bytes.

`pdict.hsort` and `pdict.bloom` are either both present or both absent. `pdict.bloom`, when present,
starts with the little-endian u64 bit count `m`, followed by exactly
`ceil(m / 8)` bytes. The current writer chooses `m = max(max(piece_count, 1) * 10, 64)`. For each
piece identity, let `a` and `b0` be the little-endian u64 values in identity bytes 0..8 and 8..16,
and let `b = b0 | 1`; bits `(a + i*b mod 2^64) mod m` are set for `i` from 0 through 6. A reader
may treat an absent Bloom/index pair as unavailable advisory data, never as proof that referenced
content is absent. Every declared piece identity must probe as a possible hit. A present Bloom or
hash-order section outside its exact grammar is refused; advisory means the pair may be omitted, not
that a half-pair, malformed bytes, or a false-negative filter is accepted.

### Record ids and tombstones

Every 16th id is a restart. Between restarts each id is front-coded against its predecessor:

```text
varint  shared prefix bytes; zero at a restart
varint  tail bytes
bytes   tail
```

`tomb`, when present, is an ascending sequence of row ordinals:

```text
varint  tombstone count
repeated count times:
  varint  delta from the previous ordinal; the first is absolute
```

Parts are resolved newest to oldest by inclusive sequence interval. The newest part containing an
id decides its value; a tombstone makes it absent. A tombstone can be discarded only by a merge
covering every part referenced by the current manifest revision, because otherwise an older value
could reappear.

### Named content

`cmeta` assigns content columns in UTF-8 byte order:

```text
varint  content column count
repeated count times:
  varint  name bytes
  bytes   UTF-8 name
  varint  occurrence count
  u8      row-id kind: 0 dense, 1 ascending delta ids
```

Every declared content column has at least one occurrence. Dense means exactly one occurrence on
every row, in row order, and `con.rid.N` is absent. Otherwise `con.rid.N` contains exactly one
canonical varint per occurrence: the first row ordinal is absolute and every later value is its
strictly positive delta from the preceding ordinal. The reconstructed ordinals are strictly
increasing, are less than the part record count, and consume the section exactly.

`con.off.N` contains `occurrences + 1` little-endian u64 program offsets. They begin at zero, are
monotonic, and end exactly at the byte length of `con.prog.N`. `con.id.N` contains exactly 32 bytes
per occurrence: BLAKE3 of the fully reconstructed named value. There is no unavailable
identity representation. A present occurrence without an identity is malformed.

A content program is:

```text
varint  operation count
repeated count times:
  varint  tagged = (payload << 1) | operation
  operation 0: payload is literal length, followed by literal bytes
  operation 1: payload is piece-dictionary ordinal, followed by a nonzero varint piece length
```

`tagged == 0` is reserved and refused. An empty operation list is a present empty value; no row id
means absence. Content names are non-empty and unique within a record.

### Attributes

`colmeta` assigns columns in sorted `(key, type-tag)` order:

```text
varint  attribute column count
repeated count times:
  varint  key bytes
  bytes   UTF-8 key
  u8      type tag
  varint  occurrence count
  u8      row-id kind: 0 dense, 1 ascending delta ids
```

`layout` records the exact sequence of column ordinals for each row, preserving duplicate keys and
attribute order. `layout.off` carries `record_count + 1` little-endian u64 offsets: zero first,
monotonic, and ending exactly at the layout byte length. Every row slice contains one varint count,
exactly that many in-range column ordinals, and no trailing bytes. Counts across all row slices equal
the occurrence counts in `colmeta`. Attribute keys are non-empty UTF-8.

Every declared attribute column has at least one occurrence. Dense means exactly one occurrence on
every row, in row order, and `col.rid.N` is absent. Otherwise `col.rid.N` contains exactly one
canonical varint per occurrence: the first row ordinal is absolute and later values are deltas from
the preceding ordinal. Deltas may be zero only because duplicate keys on one row are distinct
occurrences. Reconstructed ordinals are nondecreasing, are less than the record count, and consume
the section exactly.

| tag | type | physical value |
|---:|---|---|
| 0 | string | u32 ordinal into sorted UTF-8 `col.dict.N` |
| 1 | i64 | eight bytes |
| 2 | f64 | eight raw IEEE-754 bits |
| 3 | bool | one byte |
| 4 | u64 | eight bytes |
| 5 | binary | u32 ordinal into byte-sorted `col.dict.N` |
| 6 | UTC Unix nanoseconds | signed eight-byte i64 |
| 7 | explicit null | no value bytes |

`col.val.N` is exactly `occurrences * width` bytes using the widths above. Integers and timestamp
values are little-endian; floats are their raw little-endian IEEE-754 bits; booleans are exactly
`0` or `1`; string and binary ordinals are little-endian u32 values within the corresponding
dictionary. A null value section is empty. NaN payloads and negative zero are preserved bit-for-bit.

Every string or binary `col.dict.N` is encoded as a canonical-varint entry count followed by that
many canonical-varint byte lengths and byte strings. Entries are strictly increasing and distinct
by their bytes and the section has no trailing bytes. String entries are UTF-8; binary entries are
arbitrary bytes. Non-string/binary columns have no dictionary section.

`zone`, when present, begins with a canonical-varint entry count and has one entry per attribute
column. Entry byte `0` means no usable bound. Entry byte `1` is followed by 16 bytes: little-endian
minimum then maximum, each encoded like that column's eight-byte scalar (booleans use eight-byte
zero or one). The current writer emits no bound for string, binary, null, or any float column
containing NaN; otherwise it emits the exact minimum and maximum of the column's occurrences.
Malformed, missing, or unusable advisory zone data means no pruning, never permission to exclude a
candidate row.

## Write-ahead log

The WAL is `<store>-wal`. It carries ordered replay input for accepted mutations until publication
makes that input redundant and truncation removes it. Its presence alone proves neither that every
frame is durable nor that every frame is still pending: a complete prefix may lie before the
durability frontier, and input from the most recent publication may remain after the container
state became current but before WAL truncation. Clean close settles and removes it.

```text
offset  size  field
     0     1  tag
     1     8  part-sequence target
     9     4  payload bytes
    13   len  payload
13+len     4  crc32 over header and payload
```

Current tags are exhaustive:

| tag | meaning |
|---:|---|
| `0xD1` | standalone tombstone |
| `0xD2` | batch completion marker |
| `0xD3` | tombstone inside a batch |
| `0xD4` | standalone record |
| `0xD5` | record inside a batch |

A checksumming unknown tag is refused. A torn final frame ends replay. Batch members remain pending
until a batch completion marker whose payload is exactly one canonical, nonzero varint naming the
immediately preceding member count; zero-member markers and trailing payload bytes are refused.
Uncommitted batch members are discarded together. The payload of either tombstone tag (`D1` or
`D3`) is the complete record ID: one or more raw UTF-8 bytes with no length prefix. Once a frame
checksum passes, malformed payload structure is corruption and refuses the WAL; it is never
reclassified as a torn suffix.

A record payload is:

```text
varint  id bytes
bytes   UTF-8 id
varint  named content count
repeated content count times, in UTF-8 name order:
  varint  name bytes
  bytes   UTF-8 name
  bytes   32-byte reconstructed-value BLAKE3
  varint  operation count
  repeated operation count times:
    u8      operation: 0 literal, 1 piece
    op 0:   varint length, then literal bytes
    op 1:   32-byte piece hash, then nonzero varint piece length
varint  attribute count
repeated attribute count times:
  varint  key bytes
  bytes   UTF-8 key
  u8      type tag, using the part tag table
  value   tag-specific bytes
varint  novel piece count
repeated novel piece count times:
  bytes   32-byte piece hash
  varint  nonzero piece bytes
  bytes   piece content
```

Novel bytes let WAL replay after writer open use the fold tail selected by the current store authority
and recreate everything the durable pending change set needs. At the canonical origin that tail is
empty. A WAL frame may omit duplicate piece bytes only when the piece is reachable before the
selected tail or when an earlier frame in the same ordered WAL input carries those novel bytes.

## Manifest revisions

`MANIFEST` is compact JSON followed by one newline and a checksum trailer:

```text
<compact JSON bytes>\ncrc32=XXXXXXXX
```

The checksum is crc32 over the JSON bytes only; `XXXXXXXX` is exactly eight lowercase hexadecimal digits. Bare
JSON, a missing or malformed trailer, unknown fields, and a checksum mismatch are refused.

The JSON bytes have one canonical encoding. There is no whitespace. Object fields appear in the
table order below; each part-reference object uses the listed part-reference field order. Arrays
retain their listed order, and each punched pair is a two-element array. Unsigned integers use the
shortest base-10 spelling and `prev` uses either a JSON string or the literal `null`. Strings carry
UTF-8 directly except that `"` and `\\` are escaped, backspace/form-feed/newline/carriage-return/tab
use `\\b`, `\\f`, `\\n`, `\\r`, and `\\t`, and every other U+0000 through U+001F byte uses
lowercase `\\u00xx`. Alternate but JSON-equivalent spellings or field orders are refused.

The JSON object has exactly these fields:

| field | meaning |
|---|---|
| `draft_epoch` | required integer `1` |
| `parts` | ordered references to the current parts |
| `fold_gen` | fold generation referenced by this manifest revision |
| `fold_seg`, `fold_off` | published fold tail |
| `next_seq` | highest `seq_hi` among referenced parts; every pending WAL frame for the next publication uses the following value |
| `commit` | manifest revision number |
| `punched` | ascending disjoint inclusive block-id intervals |
| `prev` | BLAKE3 hex of the previous manifest revision's exact bytes, or null on the first manifest revision |

Each part reference has exactly `member`, `seq_lo`, `seq_hi`, `records`, and `b3`. For a singleton
interval, `member` is exactly `part-%08u.part` with `seq_lo`; for a wider interval it is exactly
`part-%08u-%08u.part` with `seq_lo` and `seq_hi`. The decimal fields use eight zero-padded digits
as a minimum width and grow without truncation. A part rebuilt by refold may instead be named
`part-r%04u-%08u-%08u.part`, using the nonzero manifest `fold_gen`, `seq_lo`, and `seq_hi`; its
generation field is exactly four digits in `0001..9999`.
`b3` is required 64-digit lowercase BLAKE3 hex. Names are unique. When parts are present, their
sequence intervals form one contiguous history: the first `seq_lo` is 1 and each later `seq_lo`
is exactly one greater than the preceding `seq_hi`. Sequence zero, a first interval above one,
overlap, and gaps are not current-format states. The exact unsigned integer domains are:

| value | domain |
|---|---|
| `draft_epoch` | u8, and exactly `1` |
| `fold_gen`, `fold_seg`, `fold_off` | u32; generation and segment also obey their namespaces, and a persisted `fold_off` includes a complete segment header |
| `next_seq`, `commit`, `seq_lo`, `seq_hi` | u64 |
| `records`, punched range endpoints | u32 |

Every persisted manifest has `commit > 0`. Revision 1 has `prev: null`; every later revision has a
64-digit lowercase BLAKE3 `prev`. Counts, referenced part metadata, and fold tails must be
semantically consistent. When the manifest revision references parts, `next_seq` equals their
highest `seq_hi`; a manifest revision with no parts has `next_seq == 0`.
If a total merge or refold removes every row after sequences have been used, it emits one canonical
zero-row part spanning the used interval; the empty part preserves physical evidence for the
nonzero cursor rather than reverting to the no-part origin shape. A refold that removes every row
of some parts but not all folds each eliminated part's interval into the next surviving part, and
into the last surviving part when no later part survives, so the published intervals stay
contiguous and the last `seq_hi` remains the cursor.
Across consecutive retained manifest revisions, `next_seq` never decreases; a publication may
preserve the cursor or advance it, but cannot reuse an earlier record-version sequence.
Within one fold generation, the `(fold_seg, fold_off)` tail likewise never decreases. Ordinary
publication appends or preserves Fold evidence; refold is the only tail reset and atomically purges
every predecessor from the earlier generation.
New WAL frames beside that manifest carry exactly `next_seq + 1`. Frames at `next_seq` may remain
as a redundant prefix only when publication completed before WAL truncation; they precede every
frame at the successor. Any other sequence or a return to the published sequence is semantic
corruption, and no new mutation can be accepted when the successor is unrepresentable.

Every writer manifest-revision publication restages the exact current manifest bytes as `MANIFEST` and retains
the same revision as `MANIFEST.%08u`. The implementation retains four manifest revisions. Retained
revisions pin every part and fold generation they name. A refold intentionally reduces retention to
its own revision so erased bytes cannot remain reachable through time travel. A backup copies the
current manifest as `MANIFEST` into a distinct destination container and intentionally omits the
source's retained-manifest members; committing the backup container is not a writer publication of
the source manifest revision.

The `prev` links cover the retained window. Part pins cover exact immutable part bytes; part piece
hashes then cover fold content transitively. Checksums detect drift, while semantic validation
refuses authentic but impossible claims.

`punched` belongs to the manifest revision selected as current and describes block payloads deliberately
deallocated after the declaration was published. Readers use the declaration in the current
manifest revision even when they
open a retained revision. Without it, zeroed payload bytes are corruption rather than erasure.

## Container

The container begins with two 4096-byte superblock slots. Member data starts at byte 8192.

```text
[ slot 0 ][ slot 1 ][ aligned member extents and directories ... ]
```

Members may have multiple extents in logical order. Fresh extents begin at 4096-byte boundaries;
adjacent extents coalesce. Alignment padding is structural and is not recorded as free space.

### Superblock

Only the first 56 bytes of each slot are defined; every remaining byte is zero.

```text
offset  size  field
     0     8  magic = "TDBDRFT1"
     8     8  container sequence; highest valid slot is current
    16     8  directory payload offset
    24     4  stored directory bytes
    28     4  raw directory bytes
    32     4  member count
    36     4  crc32 of stored directory payload
    40     8  first byte beyond this published container state
    48     1  directory codec: 0 stored, 1 zstd
    49     1  draft epoch = 1
    50     2  reserved, must be zero
    52     4  first four bytes of BLAKE3 over slot[0..52]
```

A torn or never-written slot has no valid checksum and contributes no claim. A checksum-valid slot
with any wrong identity, epoch, reserved byte, codec, range, or semantic assertion refuses the
container; the reader does not fall back to an older state.

Container birth writes the complete 8192-byte two-slot image under an exact
`<final>.creating-<pid>-<serial>` staging name and synchronizes it before a no-replace directory-entry
installation exposes the final name: slot 0 is the empty state and slot 1 is zero. Linux and macOS
use a no-replace rename; WASI uses atomic hard-link creation followed by staging-name removal. A
crash can therefore leave an exact staging artifact or a complete final-name container, never a
partial final-name birth. Every short or malformed final-name file is unknown and refused without mutation; no reader or writer completes
it in place.

### Member directory

The directory is compressed according to the superblock and decodes as:

```text
varint  member count
repeated member count times, in name order:
  varint  name bytes
  bytes   `/`-joined normal UTF-8 components
  varint  extent count; zero denotes an empty member
  repeated extent count times:
    varint  absolute container offset
    varint  non-zero extent bytes
  u32     crc32 over the member's logical bytes in extent order
varint  free extent count
repeated free extent count times:
  varint  absolute container offset
  varint  non-zero extent bytes
  varint  container sequence that first made it free
```

A member name is non-empty UTF-8 of at most 4096 bytes, split on `/` into one or more non-empty
components. No component is `.` or `..`, and a name contains no `\`. Every other UTF-8 byte
sequence within those bounds is an ordinary component; acceptance does not depend on host path
syntax.

Member names are unique and sorted. Every member extent, free extent, and the directory extent lies
within the published tail, begins at a 4096-byte boundary, and all are pairwise disjoint. The
directory ends exactly at the published tail; no trailing published bytes exist. `freed_seq` cannot
exceed the publishing superblock sequence. The decoded directory has no trailing bytes. The only zero-byte directory
declaration is the sequence-zero birth state: offset and tail 8192, every length/count/checksum zero,
and stored codec zero. Member checksums are verified by explicit container verification. Content
punch is the sole exception to interpreting the stored directory CRC as authoritative for a
member's present logical bytes. For a current-generation fold segment containing a block declared
in the current manifest's `punched` ranges, that CRC may predate deallocation or may describe a
later physical copy of the post-punch representation. Verification therefore uses the manifest
declaration, every surviving frame header, and every unpunched payload checksum instead. The same
mismatch without a current punched declaration is corruption.

### Publication

Publication order is:

1. append or restage member extents beyond the previous tail;
2. append the new directory;
3. fsync the container;
4. write the other superblock slot with sequence plus one;
5. fsync the container again.

The pre-superblock fsync makes every named byte durable before the pointer to it. A crash before the
slot write leaves the predecessor current. A torn slot loses to the intact slot. Bytes beyond the
selected tail are unpublished and ignored.

A complete successor superblock that wins slot resolution is the publication point; success of the
second fsync is its publication acknowledgement. If an error is reported after the slot write has
fully landed, or that final barrier reports failure, reopening the same live file may already select
the successor, but the caller has no acknowledgement that the selection survives a crash. The
operation reports failure and follows its documented reconciliation path; a mutation publication
retains its redundant WAL input. A later crash may therefore resolve either predecessor or successor
according to what reached durable storage.

Free extents are never reused because an already-open reader may still address the predecessor
state. Reclaim copies current members into a fresh container and atomically replaces the store-path
artifact with the whole verified file;
hole punching may deallocate sufficiently old free extents but never gives their offsets a new
meaning.

## Backup and restore

A backup is an ordinary current-format container holding the current store authority. For a
manifest revision, it holds that revision, every part it names, and the fold generation it
references. For the canonical origin, it is the canonical sequence-zero birth container. It omits
retained manifest revisions and writer state.
It is copied to a temporary destination, fully verified as a store, then atomically installed without replacing an existing
destination. Restore performs the same current-format verification before installing a byte copy as
a writable store. Neither operation recognizes or converts another physical format.

## Limits and refusal

Representational bounds are enforced before allocation or traversal:

- stored and raw lengths are checked against their field widths and configured read limits;
- record and member counts are admitted before vectors are reserved;
- member names, ids, attribute names, and content names are length-bounded and validated as UTF-8;
- all offset arithmetic is checked for overflow and containment;
- decompression must produce exactly the declared raw length;
- decoders consume their full declared payload unless a section is explicitly advisory.

Runtime admission settings may be stricter than the physical maxima. Those settings govern future
work by one open handle; they are not persisted as format promises.

## Draft identity rule

The accepted physical identities are exactly:

| structure | identity |
|---|---|
| container | magic `TDBDRFT1`, draft epoch `1` |
| part | magic `TDBPRT01`, draft epoch `1` |
| fold segment | magic `TDBFLD01` |
| fold sidecar | magic `TDBSDR01` |
| fold dictionary | raw dictionary bytes whose full BLAKE3 digest is the 64 lowercase hexadecimal digits in `zdict-<digest>.zd` |
| WAL | tags `D1` through `D5` as assigned above |
| manifest | required `draft_epoch: 1` plus checksum trailer |

If a byte structure is not on this list, it is not a TurnDB format. There is no physical
compatibility promise until this document explicitly declares the format frozen.
