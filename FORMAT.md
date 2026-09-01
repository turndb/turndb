# turndb on-disk format

**Status: format version 2. Not frozen.** See [Compatibility](#compatibility) for what is
promised and what is not.

This document deliberately restates what the code implements, so a disagreement between the two is
detectable. It exists because a portable format has to outlive the implementation that happens to
write it: code can be rewritten, bytes on someone's disk cannot. Everything else about how turndb
works belongs in the code.

It is **normative**. Where this document and the code disagree, that is a bug in one of them, and the
first job is to find out which.

---

## The shape of a store

A store is **one file**: a [container](#the-container), superblock-addressed, holding every
committed artifact as a member. **Reading** one requires nothing but that file — no daemon, no
lock, no recovery. **Writing** one requires the writer lock described [below](#the-writer-lock),
and keeps the arrangement SQLite settled on — one file at rest, flat working state beside it while
a writer holds it:

```
mystore.turndb          the store: manifests, parts, fold segments, as members
mystore.turndb-wal      while hot: acknowledged records not yet flushed; removed on clean close
mystore.turndb-tmp/     while a maintenance operation runs: merge spools; removed whole at open
```

The artifacts inside — the manifest, parts, fold segments — are the byte-identical structures the
rest of this document specifies, addressed by member extents instead of directory entries. The
writer's commit is the container's superblock flip; the ordering argument lives in
[the container's commit protocol](#commit).

### Retired layout: the store directory

The first releases laid the same artifacts out as files in a directory. That layout is **retired**:
`convert` reads it, and nothing else exists for it — no public writer, no reader, no backup. Its
description remains normative for what the converter consumes:

```
mystore/
  MANIFEST                  the only commit point
  MANIFEST.tmp              transient; a crash may leave one behind
  MANIFEST.00000041         the commit log: the last few commits, retained verbatim
  MANIFEST.00000042
  WAL                       uncommitted records
  fold/                     content, generation 0
    WRITER.lock             the single-writer gate; empty, and never read
    seg-00000000.fold       segments, numbered densely from 0
    seg-00000001.fold
    seg-00000000.dir        advisory sidecar beside a SEALED segment
    zdict-<hex>.zd          a trained dictionary, named by its own hash
  fold-0001/                content, generation 1 (after a re-fold)
  part-00000003.part                 written by a flush, named by its sequence
  part-00000001-00000003.part        written by a merge, named by its sequence RANGE
  part-r0001-00000001-00000003.part  written by a re-fold into generation 1
```

Part filenames are informative only: the manifests name what is reachable. **The sweep removes
exactly two classes of file** — a `part-*.part` that no manifest (live or retained — see
[The manifest](#the-manifest)) names, and a whole fold-generation directory whose generation no
manifest names. Everything else in the directory is named by no manifest and is *supposed* to
survive: the WAL holds records that are not committed yet and so cannot be named; the retained
`MANIFEST.NNNNNNNN` files *are* the naming authority; sidecars and dictionaries belong to a fold
generation rather than to a commit; and `WRITER.lock` belongs to the process, not to any snapshot.
A sweep of everything unnamed would delete acknowledged data that has not yet been flushed.

The three part-name forms exist so a merge output can never collide with an input it is about to
replace.

### The writer lock

A single-file store's lock is `flock` **on the store file itself**, exactly where SQLite puts it:
taken exclusively at writer open, released by the kernel when the descriptor closes — including on
a crash — and never observed by readers.

The retired directory layout used `<fold-generation>/WRITER.lock`, an empty file held under the
same exclusive advisory lock for as long as a writer held the fold open. It carries no content and
is never read — the lock is the file's whole purpose, and a second writer is refused at open
rather than allowed to interleave.

It is **not** part of a snapshot. A pack excludes it, because a pack has no writer, ever; packing
works from an allowlist of what belongs in a snapshot rather than a denylist of what does not, so it
cannot be swept into one by accident.

**Where the invariant is enforced, and where it is not.** On Unix this is `flock`, which the kernel
releases when the descriptor closes — including on a crash. That is what makes it a *safe* gate
rather than a convention: a stale lock cannot outlive its owner, so there is never a lock nobody can
distinguish from a live one.

**On Windows this is `LockFileEx`**, which the operating system releases when the handle closes or
the process terminates — the same property. One difference is load-bearing: a Windows byte-range
lock is *mandatory*, and an exclusive lock denies other handles both reads and writes of the range
it covers, while ordinary readers of a live store never take the lock and must keep reading. The
lock therefore covers exactly one byte at offset 2^64 − 2 — past any offset a file can hold, and so
never inside a read (locking beyond end-of-file is permitted). Mapped views ignore byte-range locks
altogether; both reader paths are exercised against the locked byte in `src/sys.rs`'s tests.

**On `wasm32-wasip1` there is no advisory locking.** WASI provides no equivalent, so the lock call
succeeds unconditionally and the file is created but gates nothing. On that build the single-writer
invariant is **the embedder's to keep**, and the
obligation is precise: **at most one open writer per store, across all processes and all
WASM instances.** One process is not sufficient isolation — a single process can open the same
store through two instances or two handles, and the lock call will not stop it.

What two concurrent writers do to a store has been measured in one pattern only. In four
overlapping-writer runs on the `wasm32-wasip1` build, both writers received successful durability
acknowledgements and one writer's complete record set was silently discarded; the surviving store
was internally consistent and every remaining record was readable. Other overlap patterns may fail
differently, including by interleaving WAL frames and
damaging them. **Detection is not guaranteed, and the absence of an error does not establish that
the store holds every acknowledged write** — in the measured pattern the store was intact and the
records were gone, so an integrity check is not the instrument for this.

A lockfile is deliberately not used as a substitute. An `O_EXCL` file survives a hard kill, and a
store wedged closed by a stale lock nobody can tell from a live one is a worse failure than the one
it prevents.

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

| bit | name | meaning |
|---|---|---|
| 0 | `ENCRYPTED` | this segment's block payloads are ciphertext; reading requires keys |
| 1.. | — | unassigned; a reader must refuse any of them |

Bit 0 is **reserved and refused**: nothing writes it and nothing reads it. The bit is claimed so
that if encryption is ever built, every reader shipped before it already refuses rather than
serving ciphertext as content — a reject-forward lever protects only the readers that already
refuse, so reserving the bit now, at no format cost, is what secures that guarantee. The
refusal names encryption, because "this is encrypted and this build cannot read it" sends an
operator somewhere very different from "unknown flags".

### Segment directory sidecar

`seg-NNNNNNNN.dir` is an advisory index for the segment of the same number. It carries the block
ids and offsets a reader would otherwise recover by scanning every frame in the segment:

```
offset  size  field
     0     8  MAGIC = "TURNSDIR"
     8     4  seg          must match the segment name and header
    12     4  tail         logical segment length this index describes
    16     4  n_entries
    20   n*8  block_id u32, offset u32 pairs in physical order
20+n*8     4  crc32 over all preceding bytes
```

The retired directory writer creates a sidecar when a segment seals. A current container writer
also stages one for the active segment whenever it commits that segment's tail; sidecar and segment
therefore become visible in the same superblock state. This is the remote-open locality guarantee,
not a correctness dependency: a missing sidecar, a checksum failure, a tail mismatch, impossible
entries, or an over-budget sidecar all mean **scan the segment**, never refuse an otherwise valid
store and never trust part of the advisory index.

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

**Every offset a part contains is relative to that part's first byte, and the same holds for a fold
segment.** Both are therefore *relocatable*: the identical bytes are a valid artifact whether they
live as their own file, as an extent inside a larger container, or as a remote object, and any
reader that can supply the byte range can read one without translating a single field. The
[pack](#the-pack) already depends on this, laying whole parts and segments end to end at arbitrary
offsets and addressing them through its own table of contents. A field holding an offset into the
enclosing *file* rather than into the artifact would end that property silently — nothing would
break until someone tried to relocate — so this format has none, and adding one is a breaking
change even though no existing reader would notice.

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
    45     1  version       format version; this revision writes 2; 0 predates this field
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
than to what a query touches. It is exposed as an explicit call, `Part::verify_sections`. A reader
may ignore `xsum` entirely and remain **format-compatible** — it cannot thereby remain *correct*
under corruption. A writer may not omit one.

### Sections

Absence is meaningful, but it does **not** mean "anything may be missing". Three classes:

**Required.** A part without these is malformed, and a reader must refuse rather than improvise.

| name | contents |
|---|---|
| `ids` | front-coded id column, strictly increasing |
| `ids.restart` | u32 stream offsets, one every `RESTART` = 16 ids |
| `cmeta` | named content-column metadata; required even when it declares zero columns (version ≥ 2) |
| `pdict.loc` | piece dictionary `Loc`s, 12 bytes each, sorted in FOLD order |
| `pdict.hash` | piece hashes, 32 bytes each, parallel to `pdict.loc` |

`pdict.loc` and `pdict.hash` are required even when empty, because their length is what defines the
dictionary's size.

**Conditionally required.** Required exactly when the condition holds; absent otherwise.

| name | required when |
|---|---|
| `con.prog.N`, `con.off.N` | content column *N* exists in `cmeta` |
| `con.id.N` | content column *N* exists in `cmeta` (version ≥ 2) |
| `con.rid.N` | content column *N* is sparse; absent and **elided** when dense |
| `layout`, `layout.off`, `colmeta` | any record carries an attribute |
| `col.val.N` | column *N* exists in `colmeta` |
| `col.rid.N` | column *N*'s `rid_kind` is 1 (delta); absent and **elided** when dense |
| `col.dict.N` | column *N*'s tag is 0 (string) or 5 (binary) |

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

Version-0 and version-1 parts predate named content. They require `prog` and `prog.off`, holding one
body program per row, instead of `cmeta` and `con.*`; a current reader presents that physical body as
a dense content column named `body`. Version 2 is the single successor of version 1: it introduces
named content columns, whole-value identities (`con.id.N`), and the complete scalar attribute tag
set 4 through 7, and it never writes the legacy sections.

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

Sorted and distinct, which is what lets a reader binary-search it for a value and compare ordinals.
Entries are UTF-8 for tag 0 and arbitrary bytes for tag 5.

```
varint   n_entries
repeated n_entries times:
  varint  len
  bytes   utf8 for tag 0, arbitrary bytes for tag 5
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

#### Named content columns

Content names are unique within a record and sorted by their UTF-8 bytes into physical column
ordinals. `cmeta` is:

```
varint  n_content_columns
repeated n_content_columns times:
  varint  name_len
  bytes   utf8_name
  varint  occurrences
  u8      rid_kind       0 dense, 1 ascending delta row ids
```

Each column *N* has `con.prog.N`, containing programs in occurrence order, and `con.off.N`, containing
`occurrences + 1` little-endian u64 offsets. Version 2 also has `con.id.N`, exactly 33 bytes per
occurrence in the same order: one availability byte followed by a 32-byte digest. Availability `1`
means the digest is BLAKE3 of the exact reconstructed value; availability `0` requires an all-zero
digest and represents a value carried forward from an older or explicitly unidentified source. No
other availability value is valid. A sparse column also has `con.rid.N`, an ascending delta-varint
sequence of row ids; a dense column occurs exactly once on every row and elides that section. Content
names must be non-empty, unique, and strictly sorted. Row ids must be unique and in range. Any
disagreement among `cmeta`, offsets, identities, row ids, and section presence is corruption.

An occurrence's program is:

```
varint  n_ops
repeated n_ops times:
  varint  tagged        (payload << 1) | op
  op 0 (literal):  payload is a byte length, followed by that many bytes inline
  op 1 (piece):    payload is a dictionary ordinal, followed by a varint length
```

Concatenating the ops in order reproduces that named content value **byte for byte**. An empty program
is a present empty value; a missing row id is absence. Every content column uses the same part-wide
piece dictionary, so identical bytes deduplicate across content names and records. The byte-exact
promise has one exception: content erased for privacy or retention reasons cannot be reproduced, by
definition. See [Erasure](#erasure) for what a reader gets instead.

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

Min and max encode in the column's own width rules: i64/timestamp and u64 little-endian, f64 as
**bits** (compared as floats by the reader), bool widened to 8 bytes as 0 or 1. Deliberate absences:
string and binary columns use their sorted-distinct dictionaries as bounds; explicit null is
unordered; a float column that ever saw a **NaN** declares itself
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
| 4 | u64 | 8 | full unsigned range; never rounded through i64 |
| 5 | binary | 4 | u32 ordinal into a byte-sorted `col.dict.N` |
| 6 | timestamp | 8 | signed Unix nanoseconds, UTC |
| 7 | explicit null | 0 | occurrence lives entirely in `rid` and `layout` |

A column is a sparse pair of parallel arrays: `rid` (ascending row indices) and `val`. `col.rid.N` is
encoded per `colmeta`: kind 0 (`RID_DENSE`) means the array is exactly `0..n` and is **elided** — it
carries no information and, in one measured corpus, was 39.4% of part metadata. Kind 1 (`RID_DELTA`) is
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

Beside a single-file store as `<store>-wal`; `WAL` in the root of the retired directory layout.
Truncated at every flush, so it never carries history across a version boundary; removed by a
clean close, so a store at rest is one file and its presence at open means a crash to replay.

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
| 0x57 | legacy version-1 body record | legacy payload below |
| 0x58 | tombstone | the id alone, UTF-8, no framing |
| 0x5A | legacy version-1 body record, **inside a batch** | as 0x57 |
| 0x5B | tombstone, inside a batch | as 0x58 |
| 0x59 | **batch commit** | varint member count |
| 0x5C | version-2 record: named content, whole-value identities, the complete scalar attribute tags | current payload below |
| 0x5D | version-2 record, **inside a batch** | as 0x5C |

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

Writer admission ceilings are deliberately absent from this format. They are per-open runtime policy:
a reader and recovery must accept any frame valid under the format even when the current writer would
decline to create it under lower configured limits. The deterministic charging unit and defaults are
specified in `docs/write-admission.md`.

A build predating named content refuses the 0x5C and 0x5D tags by the unknown-tag rule below, which
is the safe direction. A current reader accepts the old record tags, presents version-1 body programs
as content named `body`, and reports whole-value identity unavailable for version-1 records.

A current record payload is:

```
varint   id_len
bytes    id
varint   n_contents
repeated n_contents times, in UTF-8 name order:
  varint  name_len
  bytes   utf8_name
  u8      identity_present  0 unavailable, 1 followed by 32-byte BLAKE3
  bytes   identity          present only when identity_present is 1
  varint  n_ops
  repeated n_ops times:
    u8      op               0 literal, 1 piece
    op 0:   varint len, then len bytes
    op 1:   32 bytes piece hash, then varint len
varint   n_attrs
repeated n_attrs times:
  varint  key_len
  bytes   key
  u8      tag              0 string, 1 i64, 2 f64 bits, 3 bool, 4 u64,
                           5 binary, 6 UTC Unix nanoseconds, 7 explicit null
  value   tag 0: varint len + utf8; 1: 8 bytes i64; 2: 8 bytes f64 BITS; 3: 1 byte;
          4: 8 bytes u64; 5: varint len + bytes; 6: 8 bytes i64; 7: no bytes
varint   n_novel
repeated n_novel times:
  32 bytes hash
  varint   len
  bytes    piece content
```

The legacy 0x57/0x5A payload places one `n_ops` program directly after the id, followed by the
original attribute encoding — tags 0 through 3 only — and the novel-piece encoding. It has no
content count or name.

Two differences from a part's `con.prog.N`, both deliberate and neither incidental:

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
bit that still reads as JSON — a shortened `fold_off`, a wrong generation — would simply be
believed. During development, before the trailer existed, exactly that happened: recovery believed a
corrupted manifest that parsed cleanly and truncated durable fold bytes to match it. A reader must
verify the trailer and refuse on mismatch.

The trailer is recognised by **shape**: a manifest written before it existed is bare compact JSON,
which cannot end with that final line, and is accepted unverified — a pre-trailer manifest carries
no checksum to verify, exactly as a version-0 part carries no version field. Corruption cannot
demote a checksummed manifest to a legacy one: damage to the payload fails the checksum, and damage
to the trailer leaves trailing bytes that JSON parsing refuses. A build predating the trailer
refuses a manifest carrying one (as a parse error), which is the safe direction — refusal, never
misreading.

JSON on purpose: it is small, written once per flush, and self-describing, so a field can be added
without a version lever — **provided the new field has a documented default**, since older writers will
keep omitting it. `fold_gen` was added exactly that way and absent means 0, and `commit` likewise. A
field without a default is a breaking change that JSON merely fails to announce.

Syntax and checksum are not the end of manifest validation. A part `file` is exactly one non-empty
store-local path component; absolute paths, parent traversal, nested paths, backslash separators,
and duplicate names are refused before filesystem access. Sequence ranges cannot be inverted,
optional BLAKE3 values are exactly 32-byte hex digests, and `punched` ranges must be ascending and
disjoint. A valid checksum authenticates none of these meanings—it only proves the bytes did not
drift—so semantic validation is mandatory.

`punched` is a field of that kind, and it is **normative for erasure**: an array of inclusive
`[lo, hi]` block-id ranges, ascending and disjoint, naming blocks whose payload bytes were
deallocated by [erasure](#erasure). Absent — and it is omitted when empty — means nothing has been
punched. It is written **before** the bytes go, so a crash between the two leaves blocks declared
punched that are still readable, never punched blocks that nothing declares.

**A reader must consult it to tell erasure from corruption, because the bytes cannot.** Punching
zeroes a block's payload and deliberately leaves its 16-byte header intact so the frame chain stays
walkable, so an erased block presents as a valid header over a payload whose checksum fails — which
is byte-for-byte what a torn write looks like. This declaration is the only thing that distinguishes
them.

Two consequences follow. The ranges are **per fold generation**: block ids restart at 0 in a new
generation, so a re-fold — which rewrites the world without the erased content and therefore has no
holes to declare — must reset the list rather than carry it forward, or it names live blocks as
erased. And a **retained** manifest predates every punch that followed it, so a reader opening a
retained snapshot must take `punched` from the **live** manifest, where it is cumulative, rather
than from the snapshot's own.

Committed, in a single-file store, as a restaged `MANIFEST` member published by the container's
superblock flip — one atomic state carrying the manifest, its retained copy, and everything the
commit stages. The retired directory layout committed with tmp + fsync + rename + fsync-dir, so a
crash sees either the old manifest or the new one — or, on Windows, *neither*: a replace-rename there
is not documented to exclude a crash state with the old name removed and the new one not yet
landed, so that layout's commit publishes `MANIFEST.<commit>` before it touches the live name, and
an open that finds the live `MANIFEST` absent beside a commit log promotes the newest retained copy
only after validating it whole — the manifest, its fold at the candidate's tail, every part it
names by digest and section checksums, and every record it serves. A copy that is merely present,
or merely newest, is not promoted; a damaged newest copy is a refusal, never a rollback to an older
one. **An unreadable manifest is an error, not an
empty store** — conflating those with a sweep that unlinks unnamed files turns one bad byte into
an empty store.

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

Two more defaulted fields make the commit log self-checking:

* `prev` — BLAKE3 of the **previous manifest's exact bytes**, hex. Every commit chains onto what
  it replaced, at zero marginal cost. Absent on a store's first commit and in pre-chain manifests.
* each part entry's `b3` — BLAKE3 of that part file's bytes, hex, computed when the part is
  committed. Absent in pre-chain entries.

Content is pinned **transitively**: `b3` covers the part, the part's `pdict.hash` carries
per-piece BLAKE3, and every content read verifies against those — so a fold that has drifted from
what its parts expect is detectable through them, and no segment-level digest is needed. The chain's honest span:
pruned manifests take their bytes with them, so links are verifiable across the retained window
plus whatever manifests an operator archived; it is silent about commits whose bytes have been
pruned.

What the chain is *for*: catching what per-section checksums cannot. A part swapped for another
valid part, a manifest restored out of order, a file replaced wholesale — each is internally
consistent, and only the chain notices. That is an integrity property, nothing more.

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

## Erasure

The one exception to byte-exact reconstruction. Two mechanisms, and they are **not** variants of each
other — they differ in whether the record stays addressable, and that difference decides what a
reader is owed.

**Punching** deallocates the payload bytes of blocks no live record can reach, in place. Offsets do
not move, so no part is rebuilt. The record's id, its columns and its piece lengths all survive; only
the bytes are gone. The blocks are declared in [`punched`](#the-manifest) before they are destroyed.

**Re-folding** rewrites the fold without the dropped content and rebuilds every part, so the id and
the columnar metadata go too, and the retained commit log is purged — a snapshot that could still
serve the erased record is not erasure.

Two conditions bind any erasure, and they follow from the store's general rule that it must refuse
rather than mislead:

1. **A read of erased content reports erasure, not corruption.** Punching leaves the block header
   intact so the frame chain stays walkable, so an erased block is byte-for-byte indistinguishable
   from a torn write. The `punched` declaration is the only thing that separates them, and a reader
   must consult it — from the **live** manifest, since a retained one predates the punch. Telling an
   operator their disk is failing when the truth is that they erased something on purpose is a
   fault, not a cosmetic issue.

2. **A partially-erased record does not become wholly unreadable.** An audit record under a legal
   retention obligation is not improved by refusing to serve the part of it that survives.

**Condition 2 is not met by the current implementation.** A record whose pieces span several
blocks, only some of them punched, is refused whole. Serving the surviving part means returning a
reconstruction that is *not* byte-exact, and the byte-exact promise is the one this format is built
to keep — so the resolution is a new return shape that declares its gaps rather than a relaxation
of `reconstruct`, and that is an open decision, not an implementation detail.

**Scope.** These conditions bite on *retained* reads after an erasure. Live reads are unaffected
by construction: punching decides what is dead from live visibility, so no live record's blocks
are punchable.

**What erasure does not promise:** anything about copies outside this store — packs written earlier,
replicas, backups, or any consumer that already read the data. It removes content from THIS store,
and only that.

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
the WAL (a pack holds committed state; a packer must take the writer role and settle it, or refuse
rather than silently drop records), the retained commit log (snapshots of an immutable artifact are
meaningless), and the writer lock (no writer, ever).

Names are paths, which is the multi-store door: a future pack may carry several stores under
name prefixes with no format change — the TOC neither knows nor cares. This revision writes and
reads single-store packs.

### Leaving the pack

Nothing writes packs any more, and nothing unpacks them into directories. The one crossing left
is **conversion**: every inner file is copied byte-verbatim into a fresh container — built in
staging, committed, verified, published with a no-replace rename — and the result is an ordinary
writable single-file store. The crossing is mechanical; nothing is reinterpreted.

---

## The container

A **container** is a store in one file that can still grow. It holds what a pack holds — the
committed snapshot's files under the same flat `/`-joined names — and differs in exactly one thing:
where it says it is complete.

A pack is footer-addressed, and the footer is at EOF. That is right for a sealed artifact and wrong
for one that grows: appending past the footer leaves a window in which EOF is not a footer, and a
crash there leaves a file nothing can open. A container is **superblock-addressed** instead. Two
fixed slots at the head of the file are written **alternately**, so the slot a reader resolves is
never the slot a writer is touching, and everything else is appended beyond the last committed
tail.

```
[ slot 0 (4 KiB) ][ slot 1 (4 KiB) ][ member ][ member ][ directory ][ member ] ...
                                    ^ region start, byte 8192
```

**A member is a list of extents, not one range.** A member staged whole has exactly one, but a
member may be *extended* across commits — the delta lands at the staging cursor and becomes the
member's next extent, with other members' bytes between. Readers stitch the list into one logical
range, so a part or fold segment opens out of a container byte-identical to its directory and
pack forms whether it lies in one extent or many. Physically adjacent extents coalesce as they are
staged, so a member extended by consecutive commits with nothing between them stays one extent.

Members and every fresh extent start on a 4096-byte boundary. The padding this costs (under 4 KiB
per fresh extent) is deliberate: hole punching deallocates whole filesystem blocks, so an unaligned
extent strands its edges. The padding is structural — a rewrite would recreate it — and is
therefore **not** free-listed: free space means what a rewrite can return.

A container IS the store's writable form: the writer takes `flock` on the file, appends parts and
fold segments as members past the committed tail, and publishes each flush, merge, refold,
migration step, or erasure declaration as one superblock flip. Acknowledged-but-unflushed records
live in the `-wal` sidecar and replay at open; recovery needs no truncate and no unlink, because
the committed extent lists are the truncation.

### Superblock — 4096 bytes, two slots at bytes 0 and 4096

Only the first 56 bytes are defined; the rest is reserved and MUST be zero. A slot is a whole page
so that writing one is a single positioned write that cannot straddle two.

```
offset  size  field
     0     8  MAGIC = "TURNCTNR"
     8     8  seq          commit sequence; the highest valid slot is the live state
    16     8  dir_off      byte offset of the directory payload
    24     4  dir_stored   directory payload size on disk
    28     4  dir_raw      directory size decompressed
    32     4  n_entries    members the directory names
    36     4  dir_xsum     crc32 of the STORED directory payload
    40     8  tail         first byte beyond this commit's data
    48     1  dir_codec    0 stored, 1 zstd
    49     1  version      the container plane's reject-forward lever; this revision writes 2
    50     1  flags        bit 0 SEALED; every other bit MUST be zero and a reader must refuse
    51     1  reserved, MUST BE ZERO — and a reader must refuse otherwise
    52     4  xsum         first 4 bytes of blake3 over slot[0..52]
```

`version` is independent of the record format version: the container plane can evolve without the
parts and fold segments inside it changing, and a version above the reader's own refuses rather
than misparses. **Version 1** — the first published revision: one `(off, len)` pair per member, a
free list of bare pairs, no flags — is read for exactly one purpose, opening what it already
holds; the first commit over it publishes version 2, and nothing writes version 1 again.

**A sealed container is final.** The SEALED flag refuses every further staging and commit, on this
open and every open after it; reads are untouched. Rewriting one under another name is copying,
which sealing cannot and does not prevent — the flag makes the *file* final, not the bytes secret.

**A torn slot and a slot from the future are different failures and must not be confused.** A slot
whose checksum does not cover its bytes was never completed; it carries no claim, and the other
slot wins. A slot whose checksum *passes* under a version the reader does not know is an authentic
statement from a newer writer, and falling back to the older slot would serve a stale state while
reporting success — so the container is refused entire.

### Directory

Compressed with `dir_codec`, located by `dir_off`, checksummed by `dir_xsum`. Decompressed:

```
varint   n_entries
repeated n_entries times:
  varint  name_len
  bytes   name         the member's store-relative path, e.g. "MANIFEST", "fold/seg-00000000.fold"
  varint  n_extents    0 is a legal empty member
  repeated n_extents times:
    varint  off        absolute offset of the extent's first byte in the container
    varint  len        MUST be at least 1 — an empty extent addresses nothing and is refused
  u32     xsum         crc32 of the member's LOGICAL bytes, in extent order
varint   n_free
repeated n_free times:
  varint  off
  varint  len
  varint  freed_seq    the commit sequence that first recorded this extent free
```

Entries are sorted by name. A name is one or more normal path components joined by `/` — the same
namespace a pack TOC uses, and the shape [`safe_part_file_name`](#the-manifest) already guarantees
for manifest entries. A duplicate name is refused, and every extent — member and free alike — must
lie inside the committed region: one pointing past `tail` is corruption and refuses before
anything reads through it. A `freed_seq` above the superblock's own `seq` claims a commit that has
not happened and refuses the same way.

**No byte may be claimed twice.** Member extents, free extents, and the directory's own extent
must be pairwise disjoint. A checksum-valid directory can still lie about this — checksums prove
bytes did not drift, not that they mean anything — so disjointness is validated at open, and an
overlap refuses the container before a read can be served bytes that are simultaneously someone
else's.

Per-member `xsum` follows the pack's policy: not verified on the read path — the inner formats
carry their own integrity, and content carries BLAKE3 — but verified by a deliberate scrub, and a
writer may not omit it. A writer extending a member extends the checksum by CRC combination; it
never rereads what it already wrote.

### Remote-open locality

A **cold open** here means resolving the live container into a `ReadStore`, before a query or
content read. Let `S` be the live fold generation's segment count, `D` its candidate dictionary
member count, and `P` the live manifest's part count. An empty, never-flushed container performs
two reads — the superblock slots — and stops. For a non-empty valid version-2 state published by
the current writer, opening over an uncached positioned source performs exactly

```
4 + 2*S + D + 2*P
```

source reads: two superblock slots, the member directory, the manifest; one sidecar and one header
per segment; each candidate dictionary once; and the footer plus TOC of each part. It reads **zero
fold block payload bytes**. The count is independent of the store's content bytes and record count;
parts and segments are named because those are the metadata objects that actually add open work.

An HTTP block cache may coalesce several of these positioned reads into one fetched block. Its
length-discovery probe, if it needs one, is transport work and is not included in the formula.
Conversely, one logical metadata read spanning several cache blocks may require several HTTP
requests. Those are properties of the cache geometry, so measured HTTP requests and core
positioned reads are reported separately.

The fallback rule above stays load-bearing. A container written before active-sidecar publication,
a retained snapshot whose active tail no longer matches the live sidecar, or a damaged advisory
sidecar still opens by scanning the affected segment. Such a degraded open is correct but is outside
the locality formula; verification reports damaged sidecar/member bytes where applicable rather
than pretending the fast path ran.

### Commit

The order is the crash-safety argument, and it is the whole of it:

1. append members beyond the previous `tail`;
2. append the new directory;
3. **fsync** — everything the next superblock will name must be durable before it names it;
4. write the superblock into the slot the live state was **not** read from, with `seq + 1`;
5. fsync.

A crash before step 4 leaves the previous state entire, because nothing referenced the new bytes. A
torn write in step 4 fails its checksum and loses to the other slot. Recovery is therefore not a
repair: the newest slot that passes its checksum **is** the state, and uncommitted bytes past its
tail are ignored and later overwritten.

One commit legitimately skips steps 1–3: sealing with nothing staged. The committed directory is
already durable and the new superblock re-points at it, adding only the flag — so the flip and its
barrier are the entire commit.

A barrier that reports failure is a failure: no publication path reports success after a failed
sync, and the simulator fails every barrier in the publication sweeps once to hold it to that.

Skipping step 3 is the failure this ordering exists for. Without an intervening fsync the order
dirty pages reach the platter is unspecified, so the superblock can survive while the members it
names do not — a published pointer to bytes that never landed. The
[deterministic simulator](https://github.com/turndb/turndb/blob/main/tests/dst.rs) models that as
`LastPendingOnly` and fails the container sweep at the commit if the fsync is removed.

### Free space

`n_free` records extents that are no longer named — a member restaged under the same name
supersedes its predecessor rather than overwriting it, a removed member's extents join the list
whole, and each commit free-lists the directory it supersedes, because a directory is bytes like
any other and leaving it uncounted is how dead space becomes unaccountable. **Freed extents are
recorded but never reused.** A reader that resolved an older superblock still holds offsets into
them, and handing those bytes to a new member would be silent corruption rather than a detected
fault. Space is therefore reclaimed by rewriting the container, or — where the platform can punch
holes — returned in place without moving an offset, which zeroes freed bytes but never repurposes
them.

`freed_seq` is what makes returning it in place a bounded risk instead of a leap: an extent may be
deallocated only when every superblock a supported reader could still be holding postdates the
commit that freed it. Version-1 free lists carry no stamp and read as `freed_seq` 0 — freed before
anything a reader could still hold.

A container consequently only grows, and every flush restages `MANIFEST` whether or not anything
else changed — so dead space accumulates with sessions, not with writes. **Reclaim** is the
operation that returns it whole: every live member copied to a fresh container as one aligned
extent, committed, verified, and published at the store's name. It is a copy and a rename rather
than an edit, so the container being read is never half-rewritten; a reader holding the old file
keeps reading it, because the inode outlives the name. Reclaim takes the writer lock for the whole
rewrite — a live writer would keep committing to the renamed-away inode — and is refused while an
unsettled `-wal` sidecar sits beside the file (acknowledged records only their writer can settle)
or for a sealed container, whose bytes are final.

The publication is the crash-safety argument. Its shape is the same everywhere — a durable
**anchor** naming verified bytes exists before the store's name is replaced, and survives every
state the replace can leave — and the deterministic simulator proves it under both durability
models it knows. A name is "published durably" below by the platform's namespace barrier: on POSIX
a rename or a link followed by the directory's fsync; on Windows a rename with
`MOVEFILE_WRITE_THROUGH`, which is itself the barrier. The sequence, exactly as the code performs
it:

1. the fresh container is written at `<store>.reclaiming`, committed and verified;
2. **the anchor is obtained.** Where a hard link can be made durable, it is a second directory
   entry for the staged inode: `link(<store>.reclaiming, <store>.reclaimed)` and the directory's
   fsync. The two names are one file, so the anchor costs no bytes. Otherwise — Windows, whose
   namespace publishes only through write-through renames, or any filesystem that refuses the link
   — the staging file is renamed to `<store>.reclaimed` and a **byte copy** of it is fsynced and
   published as `<store>.reclaim-candidate`;
3. the name that will be replaced over the store — the staging file itself when the anchor is a
   link, the candidate when it is a copy — is opened and the writer lock is taken on that handle,
   before it is published at the store's name, and held until reclaim returns, so no second writer
   can enter between the replace and the return;
4. that name is renamed over `<store>` — `rename(2)` on POSIX; on Windows the documented route
   that replaces an open file (`FileRenameInfoEx` with POSIX semantics), which has no
   write-through form and which no later documented operation promotes. The simulator carries
   old / new / neither for this step through every later crash point, including after the cleanup
   below and after the return;
5. the store at its name is reopened and verified, and the anchor is unlinked.

**Which anchor a reclaim uses is not decided by the platform's name.** It attempts the link and
uses the copy if the attempt fails, so a filesystem without hard links — FAT and exFAT, some
network and FUSE mounts — is served by the route that never needed them. The capability is probed
by using it: `link(2)` refuses, before anything has been published, and that refusal is the whole
of the check. Nothing infers from `cfg(unix)` that a filesystem supports links, and no store
depends on an assumption that goes unenforced.

In every one of those states the anchor is intact: a writer open that finds the store's name absent
beside its anchor validates the anchor whole (the manifest-recovery bar), copies it, locks and
verifies the copy, publishes it durably at the store's name, and only then unlinks the anchor; one
recoverer at a time, under the anchor's own lock; a corrupt or incomplete anchor is refused and
nothing is created. A store that is present is always the authority — reclaim material beside it is
removed, never consulted. The anchor's unlink and the candidate's are laggable on Windows, so
`.reclaim*` debris may follow a crash; it never changes which store wins, and a writer open beside
the present store removes it (docs/support-and-compatibility.md, "Transient names"). Cost, recorded because it is a decision and not only a mechanism. A reclaim writes the compacted
container **once** where the anchor is a link — the anchor is one directory entry — and **twice**
where it is a copy: on Windows always, and on a filesystem whose `link(2)` refuses. Windows also
performs two write-through renames more than POSIX. The doubling is of the compacted output rather
than of the store, and reclaim is an explicit administrative operation: nothing runs it on a write,
a checkpoint or a close.

---

## Limits

Enforced, not assumed. Each refuses rather than truncating, because a store that cannot be written is
recoverable and one that lies is not. The first table contains representational format bounds.

All are checked at the point of writing.

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

Readers additionally apply admission policy before allocating parser metadata. These defaults do
not narrow the on-disk integer fields: pack limits are configurable through `PackLimits`, while the
manifest and candidate-dictionary ceilings define this reader's supported profile.

| reader admission | default | behavior |
|---|---:|---|
| manifest bytes | 64 MiB | checked from file metadata and again during a bounded read |
| pack TOC stored bytes | 64 MiB | refused before reading/allocating; configurable |
| pack TOC decoded bytes | 64 MiB | refused before decompression; configurable |
| pack file entries | 100,000 | refused before iterating the TOC; configurable |
| pack entry-name bytes | 16 KiB | refused before allocating the name; configurable |
| candidate zstd dictionary | 64 MiB | refused before whole-file read |
| segment sidecar | derived from segment length | impossible advisory sizes are ignored; the segment is scanned |
| atomic frame stored bytes | 512 MiB | checked before WAL/part/fold input allocation; configurable per open |
| atomic frame decoded bytes | 512 MiB | checked before part/fold codec output allocation; configurable per open |
| filesystem directory entries | 100,000 | checked before enumeration-driven collection growth; configurable per open |
| physical WAL frames | 100,000 | checked during replay and before writer/batch append; configurable per open |
| fold blocks / block-id span | 1,000,000 | checked before sidecar/scan vectors and sparse-directory resize; configurable per open |

The atomic-frame defaults live in `ReadLimits`. A caller can raise them to
open an older legitimate large-frame store or lower them for a stricter deployment. Tail scanning
surfaces an over-budget valid frame as resource exhaustion rather than treating it as torn residue.
Writers seal fold blocks early under the effective ceiling and check part sections/TOCs before the
footer completeness marker, so they do not publish frames the same handle refuses to read.
The object-count fields share the same runtime-only `ReadLimits`. Writer output reserves directory
names, WAL frames, and fold block ids before mutation so a handle does not create structures it
cannot later admit.

Backup additionally resolves every source against the canonical store root and accepts only the
ordinary file at that exact path. A symlinked part, fold directory, segment, sidecar, dictionary, or
manifest is refused rather than followed into the backup artifact.

---

## Non-goals

Things a reader might reasonably expect to find here, and the reason each is absent.

**Parity / erasure coding for repair.** The format detects corruption at every level — frame
checksums, section checksums, the TOC and footer chains, per-piece BLAKE3 on every content read,
and manifest-pinned part digests — and repairs none of it. Reed-Solomon companions would solve bit
rot *on a single copy*, which is not the failure this system is deployed into: cold tiers live on
object storage with its own durability, sealed packs are copied, and the honest recovery for a
damaged member is to restore it. Adding an erasure-coding dependency to duplicate what the storage
layer already provides would add apparent thoroughness without adding protection. Where
belt-and-braces is wanted, external PAR2 over a sealed pack is an operations recipe and needs
nothing from this format.

**A second identity algorithm.** BLAKE3 identifies both individual fold pieces and complete named
values; the scopes differ, the identity function does not. Where a cheaper check is wanted for a hot
path, the truncated BLAKE3 prefix carried in each fold block header and the frame checksums already
provide it; neither ever concludes identity.

## Compatibility

**The format is not frozen, and does not need to be.** A re-fold rewrites every part and the fold
wholesale, so a format change is applied by re-folding forward rather than by re-ingesting. That makes
the useful promise much weaker than permanence:

> **A build will read the immediately preceding on-disk format revision, and re-fold it forward.**

The operational mechanism is documented in [resumable format migration](docs/format-migration.md):
live parts advance one manifest-published unit at a time, while retained history remains visible and
ages out under the ordinary snapshot window.

"Revision" here means a format version, not a *fold generation* — the two are unrelated, and a store
may sit at fold generation 40 while never having changed format revision at all.

What that requires of a change:

* a change a version-2 reader could **misparse** must move `PART_VERSION`, or set a `flags` bit in the
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
