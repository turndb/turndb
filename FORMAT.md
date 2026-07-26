# turndb on-disk format

**Status: format version 1. Not frozen.** See [Compatibility](#compatibility) for what is
promised and what is not.

This is the one document in this repository, and the only place mechanics are written down twice. It
exists because a portable format has to outlive the implementation that happens to write it: code can
be rewritten, bytes on someone's disk cannot. Everything else about how turndb works belongs in the
code.

It is **normative**. Where this document and the code disagree, that is a bug in one of them, and the
first job is to find out which.

---

## The shape of a store

A store is a directory. Reading one requires nothing but the files — no daemon, no lock, no recovery.
A server is a role a process takes when it holds the writer lock, not something the format depends on.

```
mystore/
  MANIFEST                  the only commit point
  MANIFEST.tmp              transient; a crash may leave one behind
  WAL                       uncommitted records
  fold/                     content, generation 0
    seg-00000000.fold       segments, numbered densely from 0
    seg-00000001.fold
  fold-0001/                content, generation 1 (after a re-fold)
  part-00000003.part                 written by a flush, named by its sequence
  part-00000001-00000003.part        written by a merge, named by its sequence RANGE
  part-r0001-00000001-00000003.part  written by a re-fold into generation 1
```

Part filenames are informative only — the manifest names what is live, and any file it does not name
is unreachable and swept. The three forms exist so a merge output can never collide with an input it
is about to replace.

Two planes, and the split is the whole design:

* the **fold** holds content, addressed by identity, written once and never rewritten;
* **parts** hold references and columns, and can be reorganised freely because they hold no content.

A merge rewrites parts and never touches the fold. That is what decouples compaction cost from data
volume, and it is asserted in code (`MergeStats::fold_bytes_touched == 0`) rather than assumed. The one
deliberate exception is a re-fold, which writes a new fold generation; see
[Fold generations](#fold-generations).

All integers are **little-endian**. Varints are LEB128-style unsigned, 7 bits per byte, low byte first.

---

## The fold

### Segment

A segment is an append-only file, `seg-%08u.fold`, numbered densely from 0. A gap is corruption and is
refused at open. Names are parsed **numerically**, not lexicographically, so numbering stays correct
past the eight-digit width.

```
offset  size  field
     0     8  MAGIC = "TURNFOLD"
     8     4  seg          segment number, must match the filename
    12     4  flags        MUST BE ZERO
    16    32  dict_id      BLAKE3 of this segment's trained dictionary, or all-zero for none
    48        first block frame
```

`flags` is the fold's **reject-forward lever**. A reader that finds a bit it does not know refuses to
open the fold. It does not skip the segment, guess at the layout, or read what it recognises: unknown
means stop, not adapt. Any future change that a version-1 reader could misinterpret must set a flag
bit, and that is what the field is reserved for.

`dict_id` names a dictionary file `zdict-<hex>.zd` beside the segments, whose contents must hash to
the id naming it. No writer currently produces one; the field is honoured on read.

### Block frame

A **piece** is the unit of identity and dedup. A **block** is the unit of compression and I/O. Pieces
accumulate in a buffer and are compressed together, which captures the cross-piece redundancy that
dominates trace data.

```
offset  size  field
     0     1  tag = 0xA5
     1     1  codec        0 stored, 1 zstd, 2 zstd with the segment dictionary
     2     4  raw          decompressed size of the whole block
     6     4  stored       on-disk payload size
    10     2  r16          first 2 bytes of BLAKE3 over the block's raw bytes
    12     4  block_id     LOGICAL identity; see below
    16 stored payload
16+stored 4   xsum         first 4 bytes of BLAKE3 over frame[0 .. 16+stored]
```

Frame length is `20 + stored`. Invariants a reader must enforce, because a violation means corruption
rather than an unsupported feature:

* `stored <= raw` — the encoder falls back to codec 0 when compression does not shrink, so this is
  structural;
* `codec == 0` implies `raw == stored`;
* `codec == 2` only in a segment whose `dict_id` is non-zero.

`xsum` exists to distinguish a **torn write** from a good block during tail recovery, before any decode
is attempted. It is not content integrity — BLAKE3 over the piece is. `r16` is a cheap filter that a
decode produced the bytes the block was written for; it never concludes identity.

### Logical block ids

`block_id` is **not** a position. Blocks are compressed in parallel and land in completion order, so
block 5 may physically precede block 3. The directory mapping `block_id -> (segment, offset)` is
**derived**: it is rebuilt at open by scanning the ids the frames carry, and is never stored.

This has a consequence worth stating plainly, because it is not obvious and it is load-bearing:
**content can be relocated on disk without invalidating any reference above the fold.** A segment can
be rewritten to drop dead blocks, and every `Loc` in every part stays valid. The indirection was
introduced so compression could run off the write path; the relocation freedom came with it.

### Loc — 12 bytes

How everything above the fold refers to content.

```
offset  size  field
     0     4  block_id
     4     4  in_off       byte offset within the block's DECOMPRESSED bytes
     8     4  raw          length of this piece
```

### Recovery

Two layers answer two different questions. A self-scan of the frame chain answers *"where do my blocks
stop being valid?"*. The manifest's committed tail answers *"where did the store promise it stopped?"*.

Recovery truncates to the committed tail and replays the log. A committed tail **beyond** the last good
block means the disk broke an fsync promise, and the fold refuses to open rather than serve content
that silently lost durable bytes.

### Fold generations

The manifest names which generation is live. Generation 0 is the plain `fold/` directory; generation
*N* is `fold-NNNN/`. A re-fold writes a new generation, rebuilds the parts against it, and the manifest
commit is the swap.

It has to work this way. A reader holding an older manifest is still reading the old generation, and
rewriting underneath it would hand back **wrong bytes rather than an error**. A generation directory
the manifest does not name is unreachable and is swept at writer open.

---

## Parts

A part is immutable, self-contained, and id-sorted. It holds no content.

```
[ section ][ section ] ... [ TOC ][ FOOTER (56 bytes, at EOF) ]
```

The footer lands last and is the completeness marker: a part whose footer is absent or fails its
checksum was torn mid-write and is discarded, never half-read.

### Footer — 56 bytes, at EOF

```
offset  size  field
     0     8  MAGIC = "TURNPART"
     8     8  toc_off       byte offset of the TOC payload
    16     4  toc_stored    TOC payload size on disk
    20     4  toc_raw       TOC size decompressed
    24     4  n_records
    28     8  seq_lo        inclusive sequence range this part covers
    36     8  seq_hi
    44     1  toc_codec
    45     1  version       format version; 0 predates this field
    46     6  reserved, zero
    52     4  xsum          first 4 bytes of BLAKE3 over footer[0..52]
```

`version` is the part plane's reject-forward lever, and the counterpart to the fold's `flags`. A reader
refuses a part whose version exceeds its own rather than parsing fields at offsets that may no longer
mean what they did — magic and a checksum are no defence here, because a future writer computes a
perfectly valid checksum over a layout this reader would then misparse.

Version 0 means *written before the field existed*. This works only because the padding was already
zero-filled, which is why claiming the byte cost nothing.

### TOC

Compressed with `toc_codec`, located by `toc_off`. Decompressed:

```
varint   n_sections
repeated n_sections times:
  varint  name_len
  bytes   name
  varint  off        absolute file offset of the section payload
  varint  stored
  varint  raw
  u8      codec
  u32     xsum       crc32 of the STORED bytes           (version >= 1 only)
```

The TOC is **not** checksummed as a whole; the footer is. Every entry is range-checked against the file
at open, because a corrupt-but-plausible entry would otherwise direct a reader to allocate `stored`
bytes and read at an arbitrary offset.

Per-section `xsum` covers what content hashes do not. Content carries BLAKE3 per piece and is verified
on **every** read; the columnar metadata — ids, attribute values, offset arrays, dictionaries — has no
such cover, and a flipped bit there is a wrong query answer with no error anywhere. Verification is
**not** performed on the read path: hashing a section costs time proportional to the whole part rather
than to what a query touches. It is exposed as a deliberate call. A reader may ignore `xsum` entirely
and still be correct; it may not write a part without one.

### Sections

Absent means the feature is unused, never that the part is malformed. A reader must tolerate absence.

| name | contents |
|---|---|
| `ids` | front-coded id column, strictly increasing |
| `ids.restart` | u32 stream offsets, one every 16 ids |
| `prog` | body programs, one per row |
| `prog.off` | u64 offsets into `prog`, `n_records + 1` of them |
| `pdict.loc` | piece dictionary `Loc`s, 12 bytes each, sorted in FOLD order |
| `pdict.hash` | piece hashes, 32 bytes each, parallel to `pdict.loc` |
| `pdict.hsort` | u32 permutation of the dictionary in HASH order |
| `pdict.bloom` | filter over the dictionary's hashes |
| `tomb` | tombstoned row ordinals; absent when the part deletes nothing |
| `layout` | per-row attribute column ordinals |
| `layout.off` | u64 offsets into `layout` |
| `colmeta` | column descriptors, in ordinal order |
| `col.val.N` | column N's fixed-width values |
| `col.rid.N` | column N's row indices; absent when dense |
| `col.dict.N` | column N's string dictionary; absent for non-string columns |

**The piece dictionary is sorted in fold order, not hash order**, and `pdict.hsort` carries hash order
separately. Two orders over one dictionary rather than two dictionaries: fold order keeps `pdict.loc`
ascending so it compresses, and hash order makes the dictionary searchable for dedup. A reader
resolving a piece by content binary-searches `hsort`, dereferencing into `pdict.hash`.

`pdict.bloom` answers "definitely not present" from memory. It has no false negatives, so it can cost a
missed dedup and never a wrong answer.

#### Body programs

Per row, in `prog` at `prog.off[row]`:

```
varint  n_ops
repeated n_ops times:
  varint  tagged        (payload << 1) | op
  op 0 (literal):  payload is a byte length, followed by that many bytes inline
  op 1 (piece):    payload is a dictionary ordinal, followed by a varint length
```

Concatenating the ops in order reproduces the record's body **byte for byte**. That is the format's
central promise.

#### colmeta

Column ordinals are assigned in sorted `(key, tag)` order, so the same input always produces the same
ordinals — insertion order would make a part depend on arrival order.

```
varint   n_columns
repeated n_columns times:
  varint  key_len
  bytes   key
  u8      tag           value type; see below
  varint  occurrences   entries in this column's rid/val arrays
  u8      rid_kind      0 dense (rid elided), 1 ascending varint deltas
```

#### Attribute columns

One logical column per `(key, type)`, so a key carrying different types across records yields several
homogeneous columns rather than one that can mis-decode. Type tags and value widths:

| tag | type | width | note |
|---|---|---|---|
| 0 | string | 4 | u32 ordinal into `col.dict.N`, which is sorted and distinct |
| 1 | i64 | 8 | |
| 2 | f64 | 8 | stored as **bits**, so -0.0 and NaN payloads round-trip exactly |
| 3 | bool | 1 | |

A column is a sparse pair of parallel arrays: `rid` (ascending row indices) and `val`. `col.rid.N` is
encoded per `colmeta`: kind 0 (`RID_DENSE`) means the array is exactly `0..n` and is **elided** — it
carried no information and was 39.4% of part metadata on a real corpus. Kind 1 (`RID_DELTA`) is
ascending varint deltas, where a repeated key on one row encodes as a zero.

Columns alone cannot reproduce a row's *interleaving* — `[a, b, a]` and `[a, a, b]` have identical
columns — so `layout` records the exact sequence of column ordinals each row used. Reconstruction walks
the layout and draws the next value from each named column.

### Version resolution

Parts carry inclusive sequence ranges and are ordered oldest to newest. The newest part holding an id
decides, and if its row is in `tomb` the id is **absent**. Older parts still listing that id are
superseded, not consulted.

A tombstone is a row, not an absence, because a deletion must shadow older versions living in older
immutable parts — and an absence cannot shadow anything. A tombstone may only be discarded by a merge
covering **every** live part, because only then is there nothing left for it to shadow. Discarding one
otherwise resurrects deleted data.

---

## The write-ahead log

`WAL`, in the store root. Truncated at every flush, so it never carries history across a version
boundary.

```
offset  size  field
     0     1  tag        0x57 record, 0x58 tombstone
     1     8  seq
     9     4  len        payload size
    13   len  payload
13+len     4  crc32      over header AND payload
```

A record payload carries the id, the body program, the attributes, and the **bytes** of every piece the
record introduced. A tombstone payload is the id alone.

Bytes are carried for genuinely new pieces only. Content that deduplicated is already durable
elsewhere, so replay does not need it — recovery truncates the fold to the committed tail and replays,
and anything written past that tail is regenerated from these bytes.

Replay stops at the first torn or corrupt frame. A partial tail is the end of the log, not an error: a
crash mid-append leaves exactly that.

---

## The manifest

`MANIFEST`, JSON, the **only** commit point. It names the live parts, the fold generation and tail, and
the sequence cursor. Everything else — the block directory, dedup indexes, part contents — is derived.

```json
{
  "parts": [{"file": "part-00000001.part", "seq_lo": 1, "seq_hi": 1, "records": 40}],
  "fold_seg": 0,
  "fold_off": 4144,
  "next_seq": 1,
  "fold_gen": 0
}
```

JSON on purpose: it is small, written once per flush, and self-describing, so a field can be added
without a version lever. `fold_gen` was added exactly that way and absent means 0.

Committed with tmp + fsync + rename + fsync-dir, so a crash sees either the old manifest or the new
one. **An unreadable manifest is an error, not an empty store** — conflating those with a sweep that
unlinks unnamed files turns one bad byte into an empty directory.

### Ordering

```
put    -> fold buffer + WAL append          (no fsync)
sync   -> WAL fsync                          <- the ACK point
flush  -> fold fsync, write part, commit manifest, truncate WAL
```

Data before pointers, always: the fold is durable before a part names any of it, and the part is
durable before the manifest names the part. A crash between any two steps leaves orphans, which are
swept at writer open, and never a pointer to something that is not there.

---

## Limits

Enforced, not assumed. Each refuses rather than truncating, because a store that cannot be written is
recoverable and one that lies is not.

| limit | value | why |
|---|---|---|
| piece length | 4 GiB | `Loc.raw` is u32 |
| block offset within a block | 4 GiB | `Loc.in_off` is u32 |
| blocks per fold | 4 Gi | `block_id` is u32 |
| segment size | 4 GiB | segment offsets are u32 |
| section size | 4 GiB | TOC `stored`/`raw` are u32 |
| records per part | 4 Gi | `n_records` is u32 |
| id restart interval | every 16 ids | `RESTART` |

`block_target` is bounded well below 4 GiB at open, because a block is admitted into a fresh segment
however large it is — so it, not `seg_max`, is what can overflow the segment append point.

---

## Compatibility

**The format is not frozen, and does not need to be.** A re-fold rewrites every part and the fold
wholesale, so a format change is applied by re-folding forward rather than by re-ingesting. That makes
the useful promise much weaker than permanence:

> **A build will read the previous generation and re-fold it forward.**

What that requires of a change:

* a change a version-1 reader could **misparse** must move `PART_VERSION`, or set a `flags` bit in the
  fold — silence is the failure mode this is designed to prevent;
* a change it would merely **not use** — a new section, a new manifest field — needs neither, because
  absent sections and absent JSON fields are already defined as "unused";
* removing or repurposing an existing field always moves the version.

Both planes now have a lever. They did not always: the fold could refuse an unknown future from the
start, and the part could not until version 1, which is why version 0 exists as a name for "before
anyone was watching".
