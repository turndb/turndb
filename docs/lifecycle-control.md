# Lifecycle cancellation and deadlines

TurnDB lifecycle work uses cooperative interruption at storage-safe checkpoints. It never kills the
writer thread asynchronously: cancellation between writing an authority and completing the state
that authority describes could turn a responsiveness feature into corruption.

The Rust core exposes a reusable `control::OperationControl` with an absolute deadline and a
shareable `CancellationToken`. `OperationInterrupted` reports `Cancelled` or `DeadlineExceeded` as a
typed reason. Existing no-options methods remain and delegate to controlled variants with no limit.

Controlled operations currently include:

- Durability entry through `Store::sync_with_control` and pending-change-set publication through
  `Store::flush_with_control`.
- Part merge through `merge_range_with_control` and `auto_compact_with_control`.
- Manifest-chain, part-section, and fold-frame verification.
- Content punch through `punch_unreferenced_with_control`.
- Generational content rewriting through `refold_with_control`.
- The read-only planning phase of strong record erasure through `erase_ids_with_control`.
- Backup construction and full staged-store verification through `Store::backup_with_control`.
- Full staged-store verification and restore artifact installation through
  `store::restore_file_with_control`.
- Offline candidate validation/publication through
  `store::promote_manifest_file_with_limits_and_control`.
- Store inventory and Node maintenance-space preflight while traversing the current manifest
  revision or synchronizing and publishing earlier accepted mutations.

Checkpoints occur between records, dictionary entries, sections, fold frames, copied pieces, rebuilt
parts, and independently punchable blocks. An individual unit is not split, so cancellation latency
is bounded by the largest unit currently being read, compressed, written, or checksummed rather than
by a real-time scheduler guarantee.

## Transient protocol names

These names are exhaustive protocol state beside a store:

| shape | owner |
|---|---|
| `<final>.publish-<pid>-<n>` | Windows staging for installation of any newly created protocol file (the suffix is a physical protocol spelling) |
| `<store>.creating-<pid>-<n>` | container birth before final-name installation |
| `<store>.reclaiming` | reclaim staging |
| `<store>.reclaimed` | reclaim anchor |
| `<store>.reclaim-candidate` and `.tmp` | Windows reclaim candidate |
| `<store>-tmp/` | merge/refold spool directory |
| `<artifact>.backing-up-<pid>-<n>` | backup staging |
| `<destination>.restoring-<pid>-<n>` | restore staging |

Recognition follows only the exact forms above; both numeric fields must fit their declared integer
domains. A present store proves these names are dead and writer open removes them. Beside an absent
store, `CreationStaging` cannot contain acknowledged state and does not block a competing complete
birth; a reclaim anchor may reconstruct and reinstate the store. Every other recognized name makes
creation refuse without mutation. No other suffix has TurnDB meaning.

## Atomicity and restart behavior

Each operation deliberately interprets interruption according to its publication protocol:

- **Durability synchronization (`sync`)** checks once before its durability boundary. That boundary
  first completes any delayed publication acknowledgement required by the accepted mutations, then
  fsyncs the WAL. Once it begins, TurnDB reports its real outcome and does not claim cancellation
  after either dependency may have become durable.
- **Flush** checks during pending-change-set/locator planning, after part construction, during bounded digest
  reads, and immediately before manifest publication. Cancellation may leave fold bytes durably
  persisted but unreachable; it removes the unpublished part and preserves the pending change set and current manifest revision.
  Part encoding remains one uninterruptible work unit. Once manifest-revision publication begins,
  the interrupted-publication protocol owns the result.
- **Part merge** writes an unreachable output part. Cancellation before manifest publication removes
  that part and leaves the current manifest revision's part references unchanged. Once manifest-revision publication begins,
  cancellation is no longer observed because the publication protocol owns the outcome.
- **Verification** is read-only after the embedding actor synchronizes, publishes, and settles earlier accepted mutations. It returns no
  partial success report.
- **Refold** is a no-op when the current authority references no parts. Otherwise it builds an
  unpublished fold generation and replacement parts. Cancellation removes all
  staged artifacts and preserves the fold generation referenced by the current manifest revision byte-exact. Once refold publication begins,
  TurnDB finishes handle, retention, and orphan cleanup without another cancellation checkpoint.
- **Content punch** publishes the complete erased-block declaration before deallocating any bytes.
  It begins deallocation only after that publication is acknowledged. Cancellation may therefore
  leave safe durable progress: some declared unreachable blocks have not yet been punched. A later
  call retries every declared block still present. This retry behavior also closes the same window
  after a process crash.
- **Strong erasure** accepts cancellation while determining which requested ids are present. If none
  resolves to a record, it returns without a transition. Once an atomic tombstone batch is applied,
  cancellation is deferred until total merge and any non-no-op refold complete.
  Returning `cancelled` after logical deletion but before physical removal would make a retry mistake
  those ids for record slots that previously resolved to absence and falsely report success.
- **Backup** builds and fully verifies a store in a private sibling file. Cancellation removes uninstalled
  staging and leaves the requested artifact absent. The no-replace artifact installation is the final
  checkpoint; once the artifact exists, TurnDB reports the installation outcome rather than
  cancellation.
- **Restore** copies the backup into a private sibling file, then fully verifies that exact staging
  store. Cancellation removes staging and leaves the destination absent. The atomic no-replace
  rename is its final checkpoint.
- **Manifest promotion** takes the writer lock — `flock` on Unix or `LockFileEx` on Windows, on the
  store container handle — while it discovers and completely validates retained candidates.
  **That lock excludes another open writer on native builds**; on
  `wasm32-wasip1` it always succeeds, so promotion is not protected from a concurrent writer and the
  exclusion is the embedder's, exactly as for ordinary writes — see
  [the store shape](../FORMAT.md#store-shape). Cancellation leaves the damaged current MANIFEST and
  retained history unchanged. Promotion is its final checkpoint; after it begins, TurnDB reports the
  actual outcome.

An actor operation may have synchronized and published earlier accepted mutations before a later checkpoint stops its main
work. That publication is ordered prerequisite work, not a partially published part merge or refold.
An already-expired deadline is checked before that prerequisite work and therefore has no lifecycle side effect.

## Node API

The native lifecycle methods and restore function accept a `LifecycleOptions` object:

```ts
const abort = new AbortController();
const result = await store.compact(true, {
  timeoutMs: 30_000,
  signal: abort.signal,
});

await store.sync({ timeoutMs: 30_000 });
await store.flush({ signal: abort.signal });
await store.verify({ timeoutMs: 30_000 });
await store.contentPunch({ signal: abort.signal });
await store.refold({ timeoutMs: 120_000 });
await store.erase(ids, { signal: abort.signal });
await store.backup('backup.turndb', { signal: abort.signal });
await restoreBackup('backup.turndb', 'restored', { timeoutMs: 120_000 });
await recoverManifest('damaged-store', {
  maxRollbackCommits: 0n,
  timeoutMs: 120_000,
  signal: abort.signal,
});
```

`timeoutMs` is converted to an absolute deadline before submission, so writer-actor queue time and
restore/manifest-promotion worker-scheduling time count. Zero is a deterministic pre-mutation refusal. A signal
aborted before submission is rejected at the JavaScript boundary; later aborts set the Rust token directly.
Both conditions reject with
`TurnDbError.code === "CANCELLED"`; the message distinguishes cancellation from deadline expiry.
Dropping or ignoring the Promise does not cancel the operation—pass a signal when cancellation is
required.

The native capability profile reports `lifecycleCancellation: true`. SQL planning/pulls expose their
own interruption controls. Durability synchronization intentionally has only a pre-boundary checkpoint, and flush's part
encoder remains an indivisible work unit; neither is represented as asynchronously killable work.
