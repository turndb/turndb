# Backup and restore

TurnDB backups are ordinary version-1 pack files: immutable, directly readable snapshots containing
the live `MANIFEST`, every part it names, and the live fold generation. They do not contain the WAL,
writer lock, or retained-manifest history. The pack remains a storage format rather than a
consumer-specific export, and content addresses survive the crossing unchanged.

## Consistent backup

The Rust writer API is:

```rust
let stats = store.backup(Path::new("snapshot.turndb"))?;
```

`Store::backup_with_control` accepts the same absolute deadline/shared cancellation token used by
other lifecycle work. Directory-level callers can use `pack::write_with_control`; pack validation
and explicit scrubs have `Pack::open_with_control` and `Pack::verify_with_control`.

`Store::backup` syncs and flushes every earlier accepted operation while holding the sole writer
role. It then copies the exact committed files while the mutable store is borrowed and no later
operation can advance the manifest. The Node equivalent, `await store.backup(path)`, is serialized
through the store actor, so its cut includes every command accepted before it and excludes commands
submitted after it.

The directory-level `pack::write(dir, out)` and `turndb pack DIR OUT` commands acquire the writer
role themselves. They recover and settle a durable WAL before packing, and refuse with writer
contention if another process is currently writing the store. This avoids an external packer racing
compaction, re-fold, or unreachable-file cleanup.

A backup is written to a uniquely named sibling staging file, synced, reopened, and fully verified.
Copy and verification check interruption between files and bounded 1 MiB chunks. Only then is it
hard-linked under the requested output name. Hard-link publication is atomic and
refuses an existing file, directory, or symlink; TurnDB never replaces a backup destination. The
parent directory is synced before success is returned. A process or machine crash can leave a hidden
staging file, but never a partial artifact under the requested name.

Cancellation has one final checkpoint immediately before the hard link. Before that point the staging
guard removes the private file best-effort and the destination remains absent. Once the link exists,
TurnDB completes cleanup/directory sync and reports that publication outcome instead of returning a
false cancellation after making an artifact visible. Writer backup may already have synced/flushed
the same logical source cut before a later cancellation; it never rolls durable source state back.

`BackupStats` reports the number of packed files, artifact bytes, and manifest commit. The legacy
`pack::write` return omits the commit but has the same safety behavior.

## Validated restore

The Rust and Node restore APIs are:

```rust
let stats = turndb::pack::restore(Path::new("snapshot.turndb"), Path::new("restored"))?;
```

```js
const stats = await restoreBackup('snapshot.turndb', 'restored', { timeoutMs: 30_000 });
```

Restore follows a deliberately conservative sequence:

1. Refuse if any filesystem object already occupies the destination.
2. Validate the footer, TOC, every member checksum, manifest, and member paths before extraction.
3. Extract through a bounded 1 MiB buffer into a unique sibling staging directory and sync every
   file and directory.
4. Open the staged directory through the ordinary read path to validate that it is a usable store.
5. Atomically rename it into place with an OS no-replace primitive and sync the parent directory.

`pack::restore_with_control` checks interruption while validating and between bounded extraction
chunks. Cancellation before the final no-replace rename removes the private staging directory
best-effort and leaves the destination absent. The rename is the last cancellation point: after it,
the operation completes publication bookkeeping and reports success or a real filesystem failure.

The final rename uses Linux `renameat2(RENAME_NOREPLACE)` or Apple `renamex_np(RENAME_EXCL)`. On a
platform without an equivalent primitive, safe restore reports `UNSUPPORTED`; it does not weaken the
no-overwrite promise with an `exists()`/`rename()` race. A failed validation or extraction removes
its staging directory best-effort and never publishes the destination. A successful restore is an
ordinary writable store starting from the backup's live commit, without its old retained history.

Node reports existing destinations as `INVALID_ARGUMENT`, invalid artifacts as `CORRUPTION`, missing
inputs as `NOT_FOUND`, unsupported safe publication as `UNSUPPORTED`, and filesystem failures as
`IO`. `capabilities().backupRestore` states whether atomic restore is available on the current
native target.

## Operational boundaries

- Backups are external copies. Record erasure or re-folding in the source store cannot erase an
  already-created pack; consumers must apply their retention and deletion policy to backup media.
- Native Node `store.backup(path, options)` and `restoreBackup(pack, dir, options)` accept
  `timeoutMs`/`AbortSignal`. Writer queue time and restore worker-scheduling time are included because
  the absolute deadline is created before submission. Dropping a Promise does not cancel work.
- The format is a full snapshot, not an incremental or remote-object protocol. Incremental backup,
  object-store transfer, encryption, scheduling, and retention policy remain higher-layer concerns.
- Callers choose and secure filesystem paths. TurnDB guarantees integrity and publication behavior,
  not access control for the surrounding directory.
