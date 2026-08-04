# Exact named-content identity: part of format version 2

The named-content generalization made independently named content a first-class columnar namespace,
but at first a reference could
only report presence, reconstructed length, and its piece count. Those facts described the reference
program; none was the identity of the complete byte value. In particular, one piece hash is not the
identity of a multi-piece value, and hashing an encoded reconstruction program would change when
carving boundaries changed even if the bytes did not.

Version 2 persists one exact whole-value identity per content occurrence:

```text
identity = BLAKE3(literal bytes || piece bytes || ... in reconstruction order)
```

The writer feeds each ingest span into the hasher while it is already carving and folding the value.
It neither concatenates a second copy nor reads the fold. Consequently two values with identical
bytes have the same identity even when one is a literal, one is a single piece, and another is a
different sequence of literals and pieces.

This does not replace piece addressing. Piece hashes remain the fold's storage and deduplication
addresses; the whole-value identity is the logical reference identity exposed to consumers. Both use
BLAKE3 but cover different byte scopes.

## WAL

Standalone and in-batch version-2 records use tags `0x5C` and `0x5D`. After each content name the
payload stores an availability byte and, when available, the 32-byte digest before the reconstruction
program. The availability marker keeps explicitly unidentified records representable and gives
future migrations an honest way to carry version-1 values without reconstructing every payload.

The version-1 tags retain their exact layout and remain readable. Their values return no
whole-value identity.

## Parts

For every named content column `N`, `con.id.N` contains one fixed-width entry per occurrence in the
same order as `con.prog.N`:

```text
u8     available       0 or 1
bytes  digest[32]      BLAKE3 when available; all zero when unavailable
```

The section is therefore exactly `occurrences * 33` bytes. Its presence and size are required by a
version-2 reader; unknown markers and a nonzero unavailable digest are corruption. Fixed width lets
an identity lookup select one digest by occurrence ordinal without decompressing its reconstruction
program or opening a fold block. A value read additionally checks the reconstructed bytes against the
whole-value identity after the ordinary per-piece verification.

Streaming merge copies the semantic identity while rewriting rows. A legacy version-1 input remains
unavailable in version-2 output unless an explicit migration chooses to pay the I/O to reconstruct
and hash it. Ordinary compaction does not quietly turn a missing guarantee into an expensive content
read.

## API meaning

Rust returns `Option<ContentHash>` and Node returns an optional lowercase 64-character hex
`identity`. For a projected content field:

- `present = false, identity = None` means the record has no such named value;
- `present = true, identity = None` means the value exists but its legacy/source representation did
  not carry a whole-value identity; and
- `present = true, identity = Some(...)` means the digest is BLAKE3 of the exact bytes returned by a
  value projection.

Consumers that need to index, correlate, or cache complete values can use this identity without
resolving bytes. It remains generic storage vocabulary: no trace family, semantic convention, or
consumer schema participates in its computation.
