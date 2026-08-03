# turndb on-disk format

**Status: format version 4. Not frozen.** See [Compatibility](#compatibility) for what is
promised and what is not.

This is the one document in this repository, and the only place mechanics are written down twice. It
exists because a portable format has to outlive the implementation that happens to write it: code can
be rewritten, bytes on someone's disk cannot. Everything else about how turndb works belongs in the
code.

It is **normative**. Where this document and the code disagree, that is a bug in one of them, and the
first job is to find out which.

---

## The shape of a store

A store is a directory. **Reading** one requires nothing but the files — no daemon, no lock, no
recovery. **Writing** one requires the writer lock described [below](#the-writer-lock). A server is a
role a process takes when it holds that lock, not something the format depends on.

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

Part filenames are informative only: the manifests name what is reachable. **The sweep is narrower
than "everything unnamed", and deliberately so.** It removes exactly two classes — a `part-*.part`
that no manifest (live or retained — see [The manifest](#the-manifest)) names, and a whole
fold-generation directory whose generation no manifest names. Everything else in the directory is
named by no manifest and is *supposed* to survive: the WAL holds records that are not committed yet
and so cannot be named; the retained `MANIFEST.NNNNNNNN` files *are* the naming authority; sidecars
and dictionaries belong to a fold generation rather than to a commit; and `WRITER.lock` belongs to
the process, not to any snapshot. A sweep that took "unnamed is unreachable" literally would delete
acknowledged data that has not yet been flushed.

The three part-name forms exist so a merge output can never collide with an input it is about to
replace.

### The writer lock

`<fold-generation>/WRITER.lock` is an empty file held under an exclusive advisory lock for as long
as a writer holds the fold open. It carries no content and is never read — the lock is the file's
whole purpose, and a second writer is refused at open rather than allowed to interleave.

It is **not** part of a snapshot. A pack excludes it, because a pack has no writer, ever; packing
works from an allowlist of what belongs in a snapshot rather than a denylist of what does not, so it
cannot be swept into one by accident.

**Where the invariant is enforced, and where it is not.** On Unix this is `flock`, which the kernel
releases when the descriptor closes — including on a crash. That is what makes it a *safe* gate
rather than a convention: a stale lock cannot outlive its owner, so there is never a lock nobody can
distinguish from a live one.

**On `wasm32-wasip1` there is no advisory locking, and this document must not imply otherwise.**
WASI provides no equivalent, so the lock call succeeds unconditionally and the file is created but
gates nothing. On that build the single-writer invariant is **the embedder's to keep**, and the
obligation is precise: **at most one open writer per store directory, across all processes and all
WASM instances.** One process is not sufficient isolation — a single process can open the same
directory through two instances or two handles, and the file will not stop it.

Two writers on one store will interleave WAL frames and corrupt it. Some of that damage may later
trip a WAL or frame check, but **detection is not guaranteed, and the absence of an error does not
establish that the store is intact.**

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
refuse, so claiming it early costs four bytes of documentation and buys that guarantee. The
refusal names encryption, because "this is encrypted and this build cannot read it" sends an
operator somewhere very different from "unknown flags".

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
| `cmeta` | named content-column metadata; required even when it declares zero columns (version ≥ 2) |
| `pdict.loc` | piece dictionary `Loc`s, 12 bytes each, sorted in FOLD order |
| `pdict.hash` | piece hashes, 32 bytes each, parallel to `pdict.loc` |

`pdict.loc` and `pdict.hash` are required even when empty, because their length is what defines the
dictionary's size.

**Conditionally required.** Required exactly when the condition holds; absent otherwise.

| name | required when |
|---|---|
| `con.prog.N`, `con.off.N` | content column *N* exists in `cmeta` |
| `con.id.N` | content column *N* exists in `cmeta` (version ≥ 3) |
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
a dense content column named `body`. Version 2 introduced named content without whole-value
identities. Version 3 adds `con.id.N`. Version 4 adds the extended attribute tags 4 through 7; it
never writes the legacy sections.

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
`occurrences + 1` little-endian u64 offsets. Version 3 also has `con.id.N`, exactly 33 bytes per
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
| 0x57 | legacy version-1 body record | legacy payload below |
| 0x58 | tombstone | the id alone, UTF-8, no framing |
| 0x5A | legacy version-1 body record, **inside a batch** | as 0x57 |
| 0x5B | tombstone, inside a batch | as 0x58 |
| 0x59 | **batch commit** | varint member count |
| 0x5C | version-2 record with named content | current payload below, without identity fields |
| 0x5D | version-2 record, **inside a batch** | as 0x5C |
| 0x5E | version-3 record with whole-content identities | current payload below, attribute tags 0 through 3 only |
| 0x5F | version-3 record, **inside a batch** | as 0x5E |
| 0x60 | version-4 record with the complete scalar attribute tags | current payload below |
| 0x61 | version-4 record, **inside a batch** | as 0x60 |

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

A build predating named content refuses 0x5C and later tags by the unknown-tag rule below, which is
the safe direction. A current reader accepts the old record tags, presents version-1 body programs as
content named `body`, and reports whole-value identity unavailable for version-1 and version-2
records.

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

The version-3 0x5E/0x5F payload has the same content layout but accepts only attribute tags 0 through
3. Version 2 omits `identity_present` and `identity` and likewise accepts only those original tags.
The legacy 0x57/0x5A payload places one `n_ops` program directly after the id, followed by the
original attribute and novel-piece encodings. It has no content count or name.

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

Two consequences worth stating because both have been got wrong here. The ranges are **per fold
generation**: block ids restart at 0 in a new generation, so a re-fold — which rewrites the world
without the erased content and therefore has no holes to declare — must reset the list rather than
carry it forward, or it names live blocks as erased. And a **retained** manifest predates every punch
that followed it, so a reader opening a retained snapshot must take `punched` from the **live**
manifest, where it is cumulative, rather than from the snapshot's own.

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
consistent, and only the chain notices. That is an integrity property, and this document claims
nothing beyond it.

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

2. **A partially-erased record does not become wholly unreadable.** An audit record you are legally
   required to keep is not improved by refusing to serve the part of it that survives.

**Condition 2 is NOT met by the current implementation, and this is the honest state rather than a
plan.** A record whose pieces span several blocks, only some of them punched, is refused whole.
Serving the surviving part means returning a reconstruction that is *not* byte-exact, and the
byte-exact promise is the one this format is built to keep — so the resolution is a new return shape
that declares its gaps rather than a relaxation of `reconstruct`, and that is an open decision, not
an implementation detail. Recorded here so a reader knows which way the gap runs.

**Scope, stated because it is easy to overstate.** These conditions bite on *retained* reads after an
erasure. Live reads are unaffected by construction: punching decides what is dead from live
visibility, so no live record's blocks are punchable.

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

### Unpacking

Extraction is byte copying — every inner file lands exactly as it was, and the directory opens as
an ordinary store, writer role available again. The safe restore API verifies all member checksums,
extracts and opens a staged store, then atomically publishes it with a no-replace rename. Both
crossings are mechanical; nothing is reinterpreted in either direction.

---

## Limits

Enforced, not assumed. Each refuses rather than truncating, because a store that cannot be written is
recoverable and one that lies is not. The first table contains representational format bounds.

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

The atomic-frame defaults are `ReadLimits`, not narrower format fields. A caller can raise them to
open an older legitimate large-frame store or lower them for a stricter deployment. Tail scanning
surfaces an over-budget valid frame as resource exhaustion rather than treating it as torn residue.
Writers seal fold blocks early under the effective ceiling and check part sections/TOCs before the
footer completeness marker, so they do not publish frames the same handle refuses to read.
The object-count fields share the same runtime-only `ReadLimits`: they do not narrow any on-disk
integer. Writer output reserves directory names, WAL frames, and fold block ids before mutation so a
handle does not deliberately create structures it cannot later admit.

Backup additionally resolves every source against the canonical store root and accepts only the
ordinary file at that exact path. A symlinked part, fold directory, segment, sidecar, dictionary, or
manifest is refused rather than followed into the backup artifact.

---

## Non-goals

Things a reader might reasonably expect to find here, and the reason each is absent. A format
document that only lists what exists leaves the next person to rediscover these arguments.

**Parity / erasure coding for repair.** The format detects corruption at every level — frame
checksums, section checksums, the TOC and footer chains, per-piece BLAKE3 on every content read,
and manifest-pinned part digests — and repairs none of it. Reed-Solomon companions would solve bit
rot *on a single copy*, which is not the failure this system is deployed into: cold tiers live on
object storage with its own durability, sealed packs are copied, and the honest recovery for a
damaged member is to restore it. Adding an erasure-coding dependency to duplicate what the storage
layer already provides would read as thorough and be surface. Where belt-and-braces is wanted,
external PAR2 over a sealed pack is an operations recipe and needs nothing from this format.

**A second identity algorithm.** BLAKE3 identifies both individual fold pieces and complete named
values; the scopes differ, the identity function does not. Where a cheaper check is wanted for a hot
path, `r16` and the frame checksums already provide it; neither ever concludes identity.

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

* a change a version-4 reader could **misparse** must move `PART_VERSION`, or set a `flags` bit in the
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
