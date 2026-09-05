# Persistent object-count admission

Byte ceilings are not enough to bound a parser or WAL-replay path. A container or standalone fold
directory can contain millions of tiny names, a WAL can contain millions of checksum-valid empty frames, and
one checksummed fold frame can carry a sparse block id that asks a reader to resize its index to that
id. TurnDB therefore admits persistent object counts before growing the corresponding collection.

The controls are inclusive fields of the same per-handle `ReadLimits` used for atomic frame bytes:

| Rust field | native/portable Node option | default | unit |
|---|---|---:|---|
| `max_directory_entries` | `maxDirectoryEntries` | 100,000 | container members or entries visited by one standalone fold-directory enumeration |
| `max_wal_frames` | `maxWalFrames` | 100,000 | physical frames in one WAL sidecar |
| `max_fold_blocks` | `maxFoldBlocks` | 1,000,000 | blocks in one fold generation and largest block id plus one |

The values are runtime policy, not persisted format state. Native/Rust use positive u64 values within
the process and format address spaces. The portable package accepts positive u32 values. A zero or
out-of-range policy is `INVALID_ARGUMENT`; a valid ceiling refusing existing or proposed objects is
`RESOURCE_EXHAUSTED` through the typed `ReadAdmissionError::ObjectCountTooLarge` cause.

## Container members and working-directory entries

Container open counts member names before retaining or acting on them. Writer startup, fold staging,
backup, refold accounting, and debris cleanup apply the same ceiling to working-directory entries.
Irrelevant names count too; an attacker cannot evade the ceiling by choosing names TurnDB does not
recognize. Entry I/O errors propagate rather than disappearing through a flattened iterator.

Writers reserve future persistent objects before their first relevant mutation. This includes WAL
frames, fold sidecar/segment rolls, part and retained-manifest members, and the complete refold
staging set. A handle therefore does not deliberately create a shape its own policy refuses to
enumerate.

Retained-manifest-revision discovery is bounded by the same member ceiling. Writer health keeps the
retained-window count as process state and remains a constant-work observation rather than hiding a
container scan in a getter.

## WAL frames

The count is physical, not logical. A standalone put/delete consumes one frame; an atomic batch
consumes one member frame per item plus its completion marker. Complete batch admission checks that count
before any member can alter the fold or WAL. `health().wal_frames`/`health().walFrames` reports the
current physical count.

Replay admits the next count before allocating or decoding its payload. It also returns the exact
byte and physical-frame boundary safe for continued writing. A torn frame and an uncommitted batch tail
are truncated before a recovered writer can append behind them; otherwise the next reopen would stop
at the old tear and silently hide the new suffix. A valid WAL rejected only by a stricter count or
byte profile is not truncated.

## Fold blocks

Tail scan and sidecar parsing admit entry count before growing their vectors. Directory installation
checks both the number of observed blocks and `block_id + 1` before resizing the sparse index, and
duplicate ids are refused instead of overwriting an earlier location. This makes a checksum-valid
sparse id a small typed refusal rather than a caller-selected allocation.

Writers admit the next block id before finalizing or appending. A strict profile can fill its last
allowed block and read it normally; the first operation requiring another block fails before fold
mutation. Advisory sidecar bytes are also checked against stored-frame admission before reading,
while a missing or structurally damaged sidecar still falls back to authoritative segment scanning.

## API surfaces

Rust embedders set all five byte/count fields through `StoreOptions::read_limits`; explicit `Part`,
`Fold`, read-view, restore, and manifest-promotion variants carry the same value. Capabilities and
writer health report defaults/effective values.

Native Node exposes the three bigint options on writer/read-view/manifest-promotion/restore opens,
exact read-view getters, capability defaults, and effective writer health. Writer-created read views inherit the
writer profile. Portable Node exposes positive-number options and returns all five effective values
from `store.readLimits()`. Its one current `tdb_open` ABI carries the complete admission profile.

These are collection-admission controls, not a total RSS, disk-space, or CPU guarantee. Manifest
bytes retain their separate supported-reader limits; query results, reconstruction,
SQL execution, caches, and write payloads retain their own budgets. Cooperative cancellation also
does not preempt one filesystem syscall or one codec call.
