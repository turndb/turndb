# Backup and restore

A TurnDB backup is a **sealed single-file store**: the same container format as the live
database, holding the live `MANIFEST`, every part it names, and the live fold generation — one
aligned extent each — with the SEALED flag set in its superblock. It does not contain the WAL
sidecar or retained-manifest history. Sealed is final: nothing ever opens a backup for writing,
and content addresses survive the crossing unchanged. Any TurnDB reader serves a backup directly;
no restore step is needed just to look inside it.

Version-1 **pack** files, the previous backup format, remain readable and restorable. Nothing
produces new ones.

## Consistent backup

The Rust writer API is:

```rust
let stats = store.backup(Path::new("snapshot.turndb"))?;
```

`Store::backup_with_control` accepts the same absolute deadline/shared cancellation token used by
other lifecycle work; validation and explicit scrubs of old pack artifacts keep
`Pack::open_with_control` and `Pack::verify_with_control`.

`Store::backup` syncs and flushes every earlier accepted operation while holding this process's
writer role — sole across processes only where that role is enforced (`flock` on the store file
on Unix; **not enforced on `wasm32-wasip1`**, see [the writer lock](../FORMAT.md#the-writer-lock)).
It then copies the exact committed members while the mutable store is borrowed and no later
operation can advance the manifest *from this process*. The Node equivalent,
`await store.backup(path)`, is serialized through the store actor, so its cut includes every
command accepted before it and excludes commands submitted after it.

The artifact is built in a sibling staging file, committed sealed, verified member by member, and
only then renamed under the requested output name with an OS no-replace primitive; the parent
directory is synced before success is returned. TurnDB never replaces a backup destination. A
process or machine crash can leave staging litter, but never a partial artifact under the
requested name.

Cancellation has one final checkpoint immediately before publication. Before that point the
staging file is removed best-effort and the destination remains absent. Once the artifact is
published, TurnDB completes its bookkeeping and reports that outcome instead of returning a false
cancellation after making an artifact visible. Writer backup may already have synced/flushed the
same logical source cut before a later cancellation; it never rolls durable source state back.

`BackupStats` reports the member count, member payload bytes, and manifest commit.

## Validated restore

The Rust and Node restore APIs are:

```rust
let stats = turndb::store::restore_file(
    Path::new("snapshot.turndb"),
    Path::new("restored.turndb"),
)?;
```

```js
const stats = await restoreBackup('snapshot.turndb', 'restored.turndb', { timeoutMs: 30_000 });
```

Restore is member-verified copying, in a deliberately conservative sequence:

1. Refuse if any filesystem object already occupies the destination.
2. Verify every member of the backup against its recorded checksums.
3. Copy the artifact byte-for-byte to a sibling staging file.
4. Clear the SEALED flag **on the staging copy** — finality binds the artifact: the backup stays
   sealed forever, and the restored copy is a different file being born writable, which is the
   point of restoring.
5. Atomically rename it into place with an OS no-replace primitive and sync the parent directory.

`restore_file_with_control` checks interruption at entry and immediately before the publishing
rename; a cancelled restore removes its staging and never publishes. The rename is the last
cancellation point: after it, the operation reports success or a real filesystem failure.

The final rename uses Linux `renameat2(RENAME_NOREPLACE)` or Apple `renamex_np(RENAME_EXCL)`. On
a platform without an equivalent primitive, safe restore reports `UNSUPPORTED`; it does not
weaken the no-overwrite promise with an `exists()`/`rename()` race. A failed verification removes
staging best-effort and never publishes the destination. A successful restore is an ordinary
writable single-file store starting from the backup's live commit, without its old retained
history.

Restoring a version-1 **pack** goes through the same door everything retired walks: conversion.
The Node `restoreBackup` dispatches on the artifact's magic — a sealed container restores by
verified copy, a pack converts into a fresh single-file store.

Node reports existing destinations as `INVALID_ARGUMENT`, invalid artifacts as `CORRUPTION`,
missing inputs as `NOT_FOUND`, unsupported safe publication as `UNSUPPORTED`, and filesystem
failures as `IO`. `capabilities().backupRestore` states whether atomic restore is available on
the current native target.

## Operational boundaries

- Backups are external copies. Record erasure or re-folding in the source store cannot erase an
  already-created backup; consumers must apply their retention and deletion policy to backup
  media.
- Native Node `store.backup(path, options)` and `restoreBackup(artifact, path, options)` accept
  `timeoutMs`/`AbortSignal`. Writer queue time and restore worker-scheduling time are included
  because the absolute deadline is created before submission. Dropping a Promise does not cancel
  work.
- The format is a full snapshot, not an incremental or remote-object protocol. Incremental
  backup, object-store transfer, encryption, scheduling, and retention policy remain higher-layer
  concerns.
- Callers choose and secure filesystem paths. TurnDB guarantees integrity and publication
  behavior, not access control for the surrounding directory.
