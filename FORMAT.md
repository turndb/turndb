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
  MANIFEST.00000041         the commit log: the last few commits, retained verbatim
  MANIFEST.00000042
  WAL                       uncommitted records
  fold/                     content, generation 0
    seg-00000000.fold       segments, numbered densely from 0
    seg-00000001.fold
  fold-0001/                content, generation 1 (after a re-fold)
  part-00000003.part                 written by a flush, named by its sequence
  part-00000001-00000003.part        written by a merge, named by its sequence RANGE
  part-r0001-00000001-00000003.part  written by a re-fold into generation 1
```

Part filenames are informative only — the manifests name what is reachable, and any file that no
manifest (live or retained — see [The manifest](#the-manifest)) names is unreachable and swept. The
three part-name forms exist so a merge output can never collide with an input it is about to
replace.

Two planes, and the split is the whole design:

* the **fold** holds content, addressed by identity, written once and never rewritten;
* **parts** hold references and columns, and can be reorganised freely because they hold no *piece*
  content — which is what makes a merge O(references) rather than O(bytes).

> **The two-plane split is a performance boundary, not a security boundary.** A part carries record
> ids, attribute values and their dictionaries, inline literals, piece lengths, and unkeyed BLAKE3 of
> every piece — all in plaintext. Anyone who can read a part can confirm whether a guessed string was
> in the store, without the fold and without the manifest. Do not ship a part to a tier that must not
> see content.

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

### The directory sidecar (advisory)

The mapping `block_id -> (segment, offset)` is derived, and deriving it by scan costs a read of the
whole segment. A **sealed** segment may therefore carry `seg-NNNNNNNN.dir` beside it:

```
offset  size  field
     0     8  MAGIC = "TURNSDIR"
     8     4  seg          must match the filename and the segment beside it
    12     4  tail         scan end; for a sealed segment this IS the file length
    16     4  n_entries
    20   n*8  (block_id u32, offset u32) per block
 20+n*8    4  crc32 over everything before it
```

Strictly **advisory and derived**, in the same sense as `pdict.hsort`: a reader may ignore it and
scan, must fall back to the scan whenever it is absent, fails its checksum, names the wrong
segment, or — the staleness gate — its `tail` is not the segment's exact file length. That last
rule is load-bearing: recovery can truncate a once-sealed segment back into being the active one,
and the leftover sidecar then describes blocks past the committed tail; a sealed segment ends
exactly at its last block, so any length mismatch means the sidecar and the segment parted ways.
A trusted-but-wrong sidecar can only misdirect a read into the frame checks (`block_id` match,
`xsum`, per-piece BLAKE3), which refuse — an error, never wrong bytes.

A writer seals the sidecar at roll, regenerates a missing or refused one after the rescan, and
never treats a sidecar failure as a fold failure. The active segment has no sidecar; it is scanned
at every open, which is also what tail recovery requires anyway.

### Recovery

Two layers answer two different questions. A self-scan of the frame chain answers *"where do my blocks
stop being valid?"*. The manifest's committed tail answers *"where did the store promise it stopped?"*.

Recovery truncates to the committed tail and replays the log. A committed tail **beyond** the last good
block means the disk broke an fsync promise, and the fold refuses to open rather than serve content
that silently lost durable bytes.

### Punched blocks

A block whose every piece is unreachable may have its **payload deallocated in place** — the
extents are freed, the bytes read back as zeros, and the file's length is unchanged, so every
offset and every `Loc` in every part still means exactly what it meant. This is erasure without
rewriting: a re-fold reclaims the same space by rebuilding the world, and this reclaims it where
it lies.

Two rules make it safe, and both are load-bearing:

* **Only the payload is punched, never the frame header.** The chain is walked by reading a
  16-byte header and stepping over `stored` bytes; a punched header would end the chain and
  silently orphan every block after it in the segment. The surviving header carries no content —
  it carries the length that keeps the chain walkable and the `block_id` that names the erasure.
* **The manifest's `punched` list is written BEFORE the bytes go**, as ascending disjoint
  inclusive `[lo, hi]` block-id ranges. A crash between the two leaves blocks marked punched that
  are still readable, which the next pass simply re-punches; the opposite order would leave zeros
  that nothing accounts for — indistinguishable from corruption.

A scan therefore recognises a frame whose header parses, whose checksum fails, and whose payload
is entirely zero as **punched**: it steps over it and leaves the block OUT of the directory, so no
`Loc` can resolve into erased bytes. Anything else that fails there is a torn write and ends the
segment's valid span, exactly as before. Reading an erased block reports erasure by name rather
than corruption — the difference between "this disk is failing" and "this content was destroyed
on purpose", and only one of them is true.

Hole punching is a filesystem feature (ext4, xfs, btrfs, tmpfs…) and frees whole filesystem
blocks; where it is unavailable, a re-fold reclaims the same space by rewriting. Metadata residue
— the erased pieces' lengths in `pdict.loc`, their hashes in `pdict.hash` — survives in the parts
until a re-fold or re-seal rebuilds them, and any erasure record must say so.

### Fold generations

The manifest names which generation is live. Generation 0 is the plain `fold/` directory; generation
*N* is `fold-NNNN/`. A re-fold writes a new generation, rebuilds the parts against it, and the manifest
commit is the swap.

It has to work this way. A reader holding an older manifest is still reading the old generation, and
rewriting underneath it would hand back **wrong bytes rather than an error**. A generation directory
no manifest names is unreachable and is swept at writer open.

---

## Parts

A part is immutable, self-contained, and id-sorted. It holds no *piece* content — but see the warning
above about what it does hold in plaintext.

```
[ section ][ section ] ... [ TOC ][ FOOTER (56 bytes, at EOF) ]
```

The footer lands last and is the completeness marker: a part whose footer is absent or fails its
checksum was torn mid-write and is **refused**, never half-read. Refused is not the same as removed —
a part a manifest names is a hard error, and only files no manifest names are swept as unreachable.

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
    46     4  toc_xsum      crc32 of the STORED TOC payload      (version >= 1)
    50     2  reserved, zero
    52     4  xsum          first 4 bytes of BLAKE3 over footer[0..52]
```

Reserved bytes must be zero and a reader **must refuse** otherwise. Reserving a byte that the reader
ignores reserves nothing — a future writer would use it and every shipped build would accept the part
and misread it.

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
  u32     xsum       crc32 of the STORED bytes           (version >= 1; presence by VERSION, not value)
```

Integrity is a **chain**, and every link is needed: the footer checksums itself, `toc_xsum` checksums
the TOC, and the TOC carries a checksum for each section. Leaving any one out makes the ones below it
worthless — section checksums stored in an unverified TOC are only as trustworthy as the bytes carrying
them.

Every entry is additionally range-checked at open, against `toc_off` rather than against the file:
sections live *before* the TOC, so that is the tighter bound and it also rules out a section claiming
to overlap the TOC or footer. A duplicate section name is refused rather than silently overwriting, and
trailing bytes after the last entry are refused rather than ignored.

Presence of `xsum` is decided by `version`, **never by its value**: crc32 can legitimately be zero, so
treating zero as "absent" would silently skip a real checksum about once in four billion.

Per-section `xsum` covers what content hashes do not. Content carries BLAKE3 per piece and is verified
on **every** read; the columnar metadata — ids, attribute values, offset arrays, dictionaries — has no
such cover, and a flipped bit there is a wrong query answer with no error anywhere. Verification is
**not** performed on the read path: hashing a section costs time proportional to the whole part rather
than to what a query touches. It is exposed as a deliberate call. A reader may ignore `xsum` entirely and remain
**format-compatible** — it cannot thereby remain *correct* under corruption, which is the whole point
of the field. A writer may not omit one.

### Sections

Absence is meaningful, but it does **not** mean "anything may be missing". Three classes:

**Required.** A part without these is malformed, and a reader must refuse rather than improvise.

| name | contents |
|---|---|
| `ids` | front-coded id column, strictly increasing |
| `ids.restart` | u32 stream offsets, one every `RESTART` = 16 ids |
| `prog` | body programs, one per row |
| `prog.off` | u64 offsets into `prog`, `n_records + 1` of them |
| `pdict.loc` | piece dictionary `Loc`s, 12 bytes each, sorted in FOLD order |
| `pdict.hash` | piece hashes, 32 bytes each, parallel to `pdict.loc` |

`pdict.loc` and `pdict.hash` are required even when empty, because their length is what defines the
dictionary's size.

**Conditionally required.** Required exactly when the condition holds; absent otherwise.

| name | required when |
|---|---|
| `layout`, `layout.off`, `colmeta` | any record carries an attribute |
| `col.val.N` | column *N* exists in `colmeta` |
| `col.rid.N` | column *N*'s `rid_kind` is 1 (delta); absent and **elided** when dense |
| `col.dict.N` | column *N*'s tag is 0 (string) |

**Optional / advisory.** A reader may ignore these entirely and remain correct, only slower or less
strict. A writer at this version always emits all of them except `tomb`.

| name | contents |
|---|---|
| `pdict.hsort` | u32 permutation of the dictionary in HASH order — an index, derivable by sorting |
| `pdict.bloom` | filter over the dictionary's hashes — an accelerator with no false negatives |
| `tomb` | tombstoned row ordinals; absent means the part deletes nothing |
| `zone` | per-column min/max — a pruning accelerator, derivable by scanning |

Unknown section names must be ignored, not rejected: that is what lets a later version add one without
moving `version`.

**The piece dictionary is sorted in fold order, not hash order**, and `pdict.hsort` carries hash order
separately. Two orders over one dictionary rather than two dictionaries: fold order keeps `pdict.loc`
ascending so it compresses, and hash order makes the dictionary searchable for dedup. A reader
resolving a piece by content binary-searches `hsort`, dereferencing into `pdict.hash`.

`pdict.bloom` answers "definitely not present" from memory. It has no false negatives, so it can cost a
missed dedup and never a wrong answer.

```
offset  size  field
     0     8  m            bit count
     8  m/8   bits         rounded up to a byte
```

Probe positions come from the piece hash itself rather than a second hash function — BLAKE3 output is
already uniform. With `a` = hash[0..8] and `b` = hash[8..16] read little-endian and `b |= 1`, probe *i*
of `k` = 7 is `(a + i*b) mod m`. Sized at 10 bits per entry.

#### ids

Front-coded: each id stores how many leading bytes it shares with its predecessor, then its own tail.
Every 16th id (`RESTART`) starts fresh, and `ids.restart` records that id's byte offset into the
stream, which is what makes a binary search possible without decoding everything before it.

```
repeated n_records times:
  varint  shared      leading bytes in common with the previous id; 0 at a restart
  varint  tail_len
  bytes   tail
```

#### tomb

Absent when the part deletes nothing. Ordinals are **row indices**, ascending, after id sorting.

```
varint   n_tombstones
repeated n_tombstones times:
  varint  delta       from the previous ordinal; the first is absolute
```

#### col.dict.N

Sorted and distinct, which is what lets a reader binary-search it for a value and compare ordinals
instead of strings.

```
varint   n_entries
repeated n_entries times:
  varint  len
  bytes   utf8
```

#### layout

Per row, at `layout.off[row]`. Records the exact sequence of column ordinals the row used, which is
what columns alone cannot reproduce.

```
varint   n_attrs
repeated n_attrs times:
  varint  column_ordinal
```

Reconstruction walks this sequence and draws the next unconsumed value from each named column.

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
central promise, and it has exactly one anticipated exception: content erased for privacy or retention
reasons cannot be reproduced, by definition. A future revision that adds erasure must say what a
reader gets instead, and must not make a partially-erased record unreadable — an audit record you are
legally required to keep is not improved by refusing to serve the part of it that survives.

`tagged == 0` is **RESERVED**. It would encode a zero-length literal, which contributes nothing; a
writer must not emit one, and a reader must refuse it. A future revision may define it as an escape
followed by a varint op number, which is what buys an unbounded op space out of a one-bit tag. Both
halves are load-bearing: a reader that accepted it as an empty literal would parse a future escape's
payload as ops.

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

#### zone

Advisory min/max per column, in colmeta ordinal order — what lets a reader skip a part whose
ranges cannot satisfy a predicate.

```
varint   n_columns        must equal colmeta's count
repeated n_columns times:
  u8     present          0 = no pruning possible for this column
  if 1:  8 bytes min, 8 bytes max
```

Min and max encode in the column's own width rules: i64 little-endian, f64 as **bits** (compared
as floats by the reader), bool widened to 8 bytes as 0 or 1. Three deliberate absences: a string
column never carries a zone, because its sorted-distinct dictionary already bounds it and bytes
repeating that would say nothing; a float column that ever saw a **NaN** declares itself
unprunable, because NaN is unordered and any range claiming to cover it would prune wrongly; and a
column with no occurrences has nothing to bound. A reader resolves **every** doubt — absent
section, damaged entry, out-of-range ordinal — to "no pruning": a zone map may only ever widen
what gets scanned, never narrow it wrongly.

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
     0     1  tag        see below
     1     8  seq        informative only — see below
     9     4  len        payload size
    13   len  payload
13+len     4  crc32      over header AND payload
```

`seq` carries the store's sequence cursor as of the frame's flush interval, which means every frame
between two flushes carries the SAME value. Replay order is file order, and nothing may be inferred
from `seq` — it exists for a human reading a hex dump to correlate a frame with a manifest, not for
a reader to act on.

| tag | meaning | payload |
|---|---|---|
| 0x57 | record | see below |
| 0x58 | tombstone | the id alone, UTF-8, no framing |
| 0x5A | record, **inside a batch** | as 0x57 |
| 0x5B | tombstone, inside a batch | as 0x58 |
| 0x59 | **batch commit** | varint member count |

A batch is a group of writes that replays **all or none** — the unit an ingest source actually
sent, kept whole across a crash. Its members are ordinary record and tombstone payloads under the
in-batch tags, followed by one commit marker. Replay holds in-batch frames in a pen; a marker seals
**exactly the `count` members immediately before it** and applies them in order. Everything else in
the pen is a batch whose marker never landed, and is discarded: members before the sealed run (an
append that errored partway), members under a later standalone frame, and an unsealed run at the
log's end. A marker claiming more members than precede it is corruption that checksums —
the frame chain is unbroken back to the last commit point, so the log is not what a writer put
down — and the reader must refuse. The marker's count is one byte of redundancy that keeps a batch
from being quietly shrunk.

A build predating batches refuses these tags by the unknown-tag rule below, which is the safe
direction; a log without them replays exactly as before.

A record payload is:

```
varint   id_len
bytes    id
varint   n_ops
repeated n_ops times:
  u8      op               0 literal, 1 piece
  op 0:   varint len, then len bytes
  op 1:   32 bytes piece hash, then varint len
varint   n_attrs
repeated n_attrs times:
  varint  key_len
  bytes   key
  u8      tag              0 string, 1 i64, 2 f64 bits, 3 bool
  value   tag 0: varint len + utf8;  1: 8 bytes i64;  2: 8 bytes f64 BITS;  3: 1 byte
varint   n_novel
repeated n_novel times:
  32 bytes hash
  varint   len
  bytes    piece content
```

Two differences from a part's `prog`, both deliberate and neither incidental:

* the op tag is a **plain u8**, not the `(payload << 1) | op` varint packing a part uses — the packing
  buys density in a section read millions of times, and the log is written once and discarded;
* a piece is referenced by **hash**, not by dictionary ordinal, because the log predates every part and
  there is no dictionary to index into yet.

Floats are stored as **bits** here for the same reason they are in a column: `-0.0` and NaN payloads
must replay exactly.

`novel` carries bytes for genuinely new pieces only. Content that deduplicated is already durable, so
replay does not need it.

Bytes are carried for genuinely new pieces only. Content that deduplicated is already durable
elsewhere, so replay does not need it — recovery truncates the fold to the committed tail and replays,
and anything written past that tail is regenerated from these bytes.

Replay stops at the first torn or corrupt frame. A partial tail is the end of the log, not an error: a
crash mid-append leaves exactly that.

An **unknown tag is different, and the two readings are opposite**: garbage from a crash means the log
ends, while a frame type a newer build wrote means refusing is the only safe response — skipping it
would apply a suffix of the log without its prefix, silently discarding committed records. The
checksum disambiguates them: a torn tail does not verify, a deliberately written future frame does. So
a well-formed frame with an unrecognised tag is **refused**, and only a failed checksum ends the log.

---

## The manifest

`MANIFEST`, JSON, the **only** commit point. It names the live parts, the fold generation and tail, and
the sequence cursor. Everything else — the block directory, dedup indexes, part contents — is derived.

```
{"parts":[{"file":"part-00000001.part","seq_lo":1,"seq_hi":1,"records":40}],"fold_gen":0,"fold_seg":0,"fold_off":4144,"next_seq":1,"commit":7}
crc32=9a3fc217
```

Two lines: compact JSON, then a trailer `crc32=XXXXXXXX` — eight hex digits, crc32 over exactly the
JSON bytes (not the newline). The trailer exists because the manifest was the one structure whose
corruption could **destroy data while parsing cleanly**: every field is load-bearing, and a flipped
bit that still reads as JSON — a shortened `fold_off`, a wrong generation — was *believed*, after
which recovery truncated durable fold bytes to match it. A reader must verify the trailer and refuse
on mismatch.

The trailer is recognised by **shape**: a manifest written before it existed is bare compact JSON,
which cannot end with that final line, and is accepted unverified — that is what "before anyone was
watching" costs, exactly as part version 0 does. Corruption cannot demote a checksummed manifest to
a legacy one: damage to the payload fails the checksum, and damage to the trailer leaves trailing
bytes that JSON parsing refuses. A build predating the trailer refuses a manifest carrying one (as a
parse error), which is the safe direction — refusal, never misreading.

JSON on purpose: it is small, written once per flush, and self-describing, so a field can be added
without a version lever — **provided the new field has a documented default**, since older writers will
keep omitting it. `fold_gen` was added exactly that way and absent means 0, and `commit` likewise. A
field without a default is a breaking change that JSON merely fails to announce.

Committed with tmp + fsync + rename + fsync-dir, so a crash sees either the old manifest or the new
one. **An unreadable manifest is an error, not an empty store** — conflating those with a sweep that
unlinks unnamed files turns one bad byte into an empty directory.

### The commit log

`commit` is a monotonic counter, advanced by every commit — flush, merge, or re-fold. (`next_seq`
cannot serve here: it advances only at flush.) Each commit also writes its exact bytes to
`MANIFEST.<commit>`, eight digits zero-padded, parsed **numerically** like segment names. The copy
lands, fsynced, *before* the rename that publishes `MANIFEST`; one directory fsync covers both. The
newest few commits are retained — this implementation keeps 4 — and older copies are pruned.

The log buys three things, and changes one rule:

* **Snapshots.** A reader may open any retained commit and see the store exactly as that commit
  left it. This works because of the rule change: the sweep unlinks only files that **no** manifest
  — live or retained — names, so a retained manifest's parts and fold generation stay on disk until
  the window prunes past it. A part replaced by a merge is therefore *deferred* to the sweep, not
  unlinked at commit.
* **Recovery.** A damaged `MANIFEST` beside an intact retained copy is recoverable by promoting the
  copy — verbatim bytes, checksum and all. In the common case (bit rot in `MANIFEST` itself) the
  newest copy carries the very same commit and nothing is lost. Promotion of an older copy is a
  **rollback** that discards acknowledged commits, so promotion is an explicit operator action;
  an implementation must not fall back silently on open.
* **A missing-manifest tripwire.** A store with retained commits and no `MANIFEST` is damage, not a
  new store, and must refuse to open — otherwise the sweep of an "empty" store unlinks everything
  the log still pins.

A retained copy that fails its checksum pins nothing and cannot be promoted. A **re-fold purges the
log** down to its own commit: erasure semantics trump snapshots, and a retained manifest would
otherwise keep the superseded generation — deleted content included — readable and on disk. Time
travel does not cross a re-fold; that is the point of running one.

A store written before the log existed has no retained copies and `commit` 0, and reads fine; the
log begins at its next commit.

### The hash chain

Two more defaulted fields turn the commit log into tamper-evidence:

* `prev` — BLAKE3 of the **previous manifest's exact bytes**, hex. Every commit chains onto what
  it replaced, at zero marginal cost. Absent on a store's first commit and in pre-chain manifests.
* each part entry's `b3` — BLAKE3 of that part file's bytes, hex, computed when the part is
  committed. Absent in pre-chain entries.

Content is pinned **transitively**: `b3` covers the part, the part's `pdict.hash` carries
per-piece BLAKE3, and every content read verifies against those — so tampering with the fold is
detectable through the parts, and no segment-level digest is needed. The chain's honest span:
pruned manifests take their bytes with them, so links are verifiable across the retained window
plus whatever manifests an operator archived. The chain is tamper-*evidence* for what is present,
never a claim about what is not — and it supports the ordinary business-records foundation; it
does not replace it.

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

## The pack

A **pack** is a store in one file: the committed snapshot's files laid end to end, a table of
contents, and a footer at EOF. It exists so a sealed store can be shipped, archived, produced in
discovery, tiered to object storage, or dropped into a browser — anywhere "a directory" is the
wrong shape and "one file readable by ranged requests" is the right one. A pack is **immutable and
writer-less by definition**: the writer role is what directories are for, and a pack never has one.

```
[ file bytes ][ file bytes ] ... [ TOC ][ FOOTER (40 bytes, at EOF) ]
```

Footer-addressed like a part, and for the same reason: the footer lands last and is the
completeness marker, and an EOF read plus one TOC read is all a reader — local or remote — needs
before it can address any inner file.

### Footer — 40 bytes, at EOF

```
offset  size  field
     0     8  MAGIC = "TURNPACK"
     8     8  toc_off      byte offset of the TOC payload
    16     4  toc_stored   TOC payload size on disk
    20     4  toc_raw      TOC size decompressed
    24     4  n_files
    28     1  toc_codec    0 stored, 1 zstd
    29     1  version      the pack plane's reject-forward lever; this revision writes 1
    30     2  reserved, MUST BE ZERO — and a reader must refuse otherwise
    32     4  toc_xsum     crc32 of the STORED TOC payload
    36     4  xsum         first 4 bytes of BLAKE3 over footer[0..36]
```

The same rules as the part footer, because they are the same rules: `version` above the reader's
own refuses rather than misparses; reserved bytes are enforced, not decorative; the integrity
chain is footer → TOC → per-file checksums, each link covering the one below.

### TOC

Compressed with `toc_codec`, located by `toc_off`. Decompressed:

```
varint   n_files
repeated n_files times:
  varint  name_len
  bytes   name         the file's store-relative path, e.g. "MANIFEST", "fold/seg-00000000.fold"
  varint  off          absolute offset of the file's first byte in the pack
  varint  len
  u32     xsum         crc32 of the file's bytes
```

Entries are sorted by name — determinism, exactly as everywhere else — and every entry is
range-checked against `toc_off` at open. A duplicate name is refused. Per-file `xsum` follows the
part sections' policy: not verified on the read path (the inner formats carry their own integrity,
and content carries BLAKE3), verified by a deliberate scrub call; a reader may ignore it and remain
format-compatible, a writer may not omit it.

### What a pack holds

The committed snapshot, exactly as a reader sees one: `MANIFEST` (verbatim, checksum trailer and
all), every part it names, and the live fold generation's segments — plus their advisory sidecars
and any dictionary files, so a pack opens as fast as the directory did. Deliberately absent:
the WAL (a pack holds committed state; a packer must refuse a store with uncommitted records
rather than silently drop them), the retained commit log (snapshots of an immutable artifact are
meaningless), and the writer lock (no writer, ever).

Names are paths, which is the multi-store door: a future pack may carry several stores under
name prefixes with no format change — the TOC neither knows nor cares. This revision writes and
reads single-store packs.

### Unpacking

Extraction is byte copying — every inner file lands exactly as it was, and the directory opens as
an ordinary store, writer role available again. Both crossings are mechanical; nothing is
reinterpreted in either direction.

---

## Limits

Enforced, not assumed. Each refuses rather than truncating, because a store that cannot be written is
recoverable and one that lies is not.

All are checked at the point of writing, and each refuses rather than truncating.

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

> **A build will read the immediately preceding on-disk format revision, and re-fold it forward.**

"Revision" here means a format version, not a *fold generation* — the two are unrelated, and a store
may sit at fold generation 40 while never having changed format revision at all.

What that requires of a change:

* a change a version-1 reader could **misparse** must move `PART_VERSION`, or set a `flags` bit in the
  fold — silence is the failure mode this is designed to prevent;
* note what these levers do **not** cover: they guard against misparsing, not against a conformant
  writer violating a privacy or retention invariant. A part that parses perfectly can still carry
  content that should have been erased. That is a policy problem and no version byte solves it;
* a change it would merely **not use** — a new optional section, a new manifest field — needs neither,
  because unknown sections must be ignored and an absent JSON field must have a documented default.
  A new manifest field without a default is a breaking change even though JSON tolerates it;
* removing or repurposing an existing field always moves the version.

Both planes now have a lever. They did not always: the fold could refuse an unknown future from the
start, and the part could not until version 1, which is why version 0 exists as a name for "before
anyone was watching".
