# Persistent object-count admission

Byte ceilings are not enough to bound a parser or recovery path. A directory can contain millions
of tiny names, a WAL can contain millions of checksum-valid empty frames, and one checksummed fold
frame can carry a sparse block id that asks a reader to resize its directory to that id. TurnDB
therefore admits persistent object counts before growing the corresponding collection.

The controls are inclusive fields of the same per-handle `ReadLimits` used for atomic frame bytes:

| Rust field | native/portable Node option | default | unit |
|---|---|---:|---|
| `max_directory_entries` | `maxDirectoryEntries` | 100,000 | entries visited by one directory enumeration |
| `max_wal_frames` | `maxWalFrames` | 100,000 | physical frames in one unflushed WAL |
| `max_fold_blocks` | `maxFoldBlocks` | 1,000,000 | blocks in one fold generation and largest block id plus one |

The values are runtime policy, not persisted format state. Native/Rust use positive u64 values within
the process and format address spaces. The portable package accepts positive u32 values. A zero or
out-of-range policy is `INVALID_ARGUMENT`; a valid ceiling refusing existing or proposed objects is
`RESOURCE_EXHAUSTED` through the typed `ReadAdmissionError::ObjectCountTooLarge` cause.

## Filesystem entries

Writer open, fold open, retained-manifest listing, recovery locking, verification, backup, refold
accounting, orphan cleanup, and store-space inventory count entries before retaining or acting on
them. Irrelevant/junk names count too; an attacker cannot evade the ceiling by choosing names TurnDB
does not recognize. Entry I/O errors are propagated instead of silently disappearing through a
flattened iterator.

Store-space inventory uses the same value as an aggregate traversal ceiling and walks directories
iteratively, so an adversarially deep tree cannot consume the call stack. Other paths enumerate only
the store root or one fold directory and apply the ceiling to that complete enumeration.

Writers reserve future names before their first relevant mutation. This includes the initial fold
and WAL, fold sidecar/segment rolls, part output plus retained/temporary manifest names, format
migration, and the complete refold staging set. Manifest commit and checked recovery promotion repeat
their exact checks before incrementing a commit, creating a retained copy, or staging `MANIFEST.tmp`.
A handle therefore does not deliberately create a directory shape its own policy refuses to
enumerate.

`retained_commits` now returns a `Result`, because a bounded listing must be able to report typed
resource exhaustion. `retained_commits_with_limits` accepts an explicit profile. Writer health keeps
the retained-window count as process state and remains a constant-work snapshot rather than hiding a
directory scan in a getter.

## WAL frames

The count is physical, not logical. A standalone put/delete consumes one frame; an atomic batch
consumes one member frame per item plus its commit marker. Complete batch admission checks that count
before any member can alter the fold or WAL. `health().wal_frames`/`health().walFrames` reports the
current physical count.

Replay admits the next count before allocating or decoding its payload. It also returns the exact
byte and physical-frame boundary safe for continued writing. A torn frame and an unsealed batch tail
are truncated before a recovered writer can append behind them; otherwise the next reopen would stop
at the old tear and silently hide the new suffix. A valid WAL rejected only by a stricter count or
byte profile is not truncated.

## Fold blocks

Tail scan and sidecar parsing admit entry count before growing their vectors. Directory installation
checks both the number of observed blocks and `block_id + 1` before resizing the sparse index, and
duplicate ids are refused instead of overwriting an earlier location. This makes a checksum-valid
sparse id a small typed refusal rather than a caller-selected allocation.

Writers admit the next block id before sealing or appending. A strict profile can fill its last
allowed block and read it normally; the first operation requiring another block fails before fold
mutation. Advisory sidecar bytes are also checked against stored-frame admission before reading,
while a missing or structurally damaged sidecar still falls back to authoritative segment scanning.

## API surfaces

Rust embedders set all five byte/count fields through `StoreOptions::read_limits`; explicit `Part`,
`Fold`, snapshot, packed-reader, restore, and recovery variants carry the same value. Capabilities and
writer health report defaults/effective values.

Native Node exposes the three bigint options on writer/snapshot/recovery/restore opens, exact snapshot
getters, capability defaults, and effective writer health. Writer-created snapshots inherit the
writer profile. Portable Node exposes positive-number options and returns all five effective values
from `store.readLimits()`. The portable wrapper calls `tdb_open_v3`; the original `tdb_open` and
frame-only `tdb_open_v2` ABIs remain available with compiled defaults for the new dimensions.

These are collection-admission controls, not a total RSS, disk-space, or CPU guarantee. Manifest
bytes and pack metadata retain their separate supported-reader limits; query results, reconstruction,
SQL execution, caches, and write payloads retain their own budgets. Cooperative cancellation also
does not preempt one filesystem syscall or one codec call.
