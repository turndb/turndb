# Backup and restore

A TurnDB backup is a **self-contained single-file store** in the same mutable container format as
the source. After any publication it holds the current `MANIFEST`, every part it names, and the fold
generation that manifest names — one aligned extent each. A brand-new empty source instead produces the canonical
sequence-zero birth container, with no manifest or members. A backup never contains the WAL sidecar
or retained-manifest history. Content addresses survive the crossing unchanged. Any TurnDB reader
serves a backup directly, and any writer can continue from it as an independent store; no restore
step is required for either use.

## Consistent backup

The Rust writer API is:

```rust
let stats = store.backup(Path::new("backup.turndb"))?;
```

`Store::backup_with_control` accepts the same absolute deadline/shared cancellation token used by
other lifecycle work.

`Store::backup` synchronizes durability and publishes every earlier accepted operation while holding this process's
writer role — sole across processes only where that role is enforced (`flock` on Unix and
`LockFileEx` on Windows, on the container handle; **not enforced on `wasm32-wasip1`**, see
[the store shape](../FORMAT.md#store-shape)).
It then copies the exact manifest-selected members while the mutable store is borrowed and no later
operation can advance the manifest *from this process*. The Node equivalent,
`await store.backup(path)`, is serialized through the store actor, so its source state includes every
command accepted before it and excludes commands submitted after it.

The artifact is built in a unique sibling staging file
`<output>.backing-up-<pid>-<n>`, published as a complete container state, and fully verified before it is renamed
under the requested output name with an OS no-replace primitive. Verification covers member
checksums, manifest references and pins, part sections, fold frames, and reconstruction of every
named content value resolved through the current manifest revision. The parent directory is durability-synchronized before success is returned. TurnDB never
replaces a backup destination. A process or machine crash can leave staging litter, but never a
partial artifact under the requested name.

The process id and monotonic process-local serial make concurrent operations use disjoint staging
names. Each name is claimed with exclusive creation; a collision after process-id reuse refuses
without truncating or removing the existing file. Cleanup is armed only after that operation owns
the name, so one operation cannot remove or install another operation's bytes. Store and artifact
paths that themselves use a current protocol-state name are invalid arguments.

Cancellation has one final checkpoint immediately before artifact installation. Before that point the
staging file is removed best-effort and the destination remains absent. Once the artifact is
installed, TurnDB completes its bookkeeping and reports that outcome instead of returning a false
cancellation after making an artifact visible. Writer backup may already have synchronized and published the
same logical source state before a later cancellation; it never rolls durable source state back.

`BackupStats` reports the member count, member payload bytes, and the copied store authority through
the public numeric `commit` encoding: `0` means the canonical origin and a positive value means that
numbered manifest revision. `RestoreStats` uses the same encoding for the installed authority. Zero
never denotes a manifest revision.

## Validated restore

The Rust and Node restore APIs are:

```rust
let stats = turndb::store::restore_file(
    Path::new("backup.turndb"),
    Path::new("restored.turndb"),
)?;
```

```js
const stats = await restoreBackup('backup.turndb', 'restored.turndb', { timeoutMs: 30_000 });
```

Restore is staged, fully verified copying, in a deliberately conservative sequence:

1. Refuse if any filesystem object already occupies the destination.
2. Copy the artifact byte-for-byte to a unique sibling
   `<destination>.restoring-<pid>-<n>` staging file.
3. Fully verify that exact staged store, including its manifest references, part sections, fold
   frames, and reconstructed content identities.
4. Atomically rename it into place with an OS no-replace primitive and durability-synchronize the parent directory.

`restore_file_with_control` checks interruption at entry and immediately before the installing
rename. Any pre-installation failure removes staging best-effort and never installs. The rename is
the last cancellation point: after it, the operation reports success or a real filesystem failure.

The final rename uses Linux `renameat2(RENAME_NOREPLACE)` or Apple `renamex_np(RENAME_EXCL)`. On
a platform without an equivalent primitive, safe restore reports `UNSUPPORTED`; it does not
weaken the no-overwrite promise with an `exists()`/`rename()` race. A failed verification removes
staging best-effort and never installs the destination. A successful restore is an ordinary
writable single-file store starting from the backup's current store authority, without retained
history.

Node reports existing destinations as `INVALID_ARGUMENT`, invalid artifacts as `CORRUPTION`,
missing inputs as `NOT_FOUND`, unsupported safe installation as `UNSUPPORTED`, and filesystem
failures as `IO`. `capabilities().backupRestore` states whether atomic restore is available on
the current native target.

## Operational boundaries

- Backups are external copies. Record erasure or refold in the source store cannot erase an
  already-created backup; consumers must apply their retention and deletion policy to backup
  media.
- Native Node `store.backup(path, options)` and `restoreBackup(artifact, path, options)` accept
  `timeoutMs`/`AbortSignal`. Writer queue time and restore worker-scheduling time are included
  because the absolute deadline is created before submission. Dropping a Promise does not cancel
  work.
- The artifact contains the complete state described by one current store authority, not an incremental or remote-object protocol. Incremental
  backup, object-store transfer, encryption, scheduling, and retention policy remain higher-layer
  concerns.
- Callers choose and secure filesystem paths. TurnDB guarantees integrity and artifact-installation
  behavior, not access control for the surrounding directory.
