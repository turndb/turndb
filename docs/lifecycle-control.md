# Lifecycle cancellation and deadlines

TurnDB lifecycle work uses cooperative interruption at storage-safe checkpoints. It never kills the
writer thread asynchronously: cancellation between writing an authority and completing the state
that authority describes could turn a responsiveness feature into corruption.

The Rust core exposes a reusable `control::OperationControl` with an absolute deadline and a
shareable `CancellationToken`. `OperationInterrupted` reports `Cancelled` or `DeadlineExceeded` as a
typed reason. Existing no-options methods remain and delegate to controlled variants with no limit.

Controlled operations currently include:

- Durability entry through `Store::sync_with_control` and memtable publication through
  `Store::flush_with_control`.
- Part compaction through `merge_range_with_control` and `auto_compact_with_control`.
- Manifest-chain, part-section, and fold-frame verification.
- In-place punching through `punch_unreferenced_with_control`.
- Generational content rewriting through `refold_with_control`.
- The read-only planning phase of strong record erasure through `erase_ids_with_control`.
- Backup packing/verification through `Store::backup_with_control` and
  `pack::write_with_control`.
- Validated extraction/publication through `pack::restore_with_control`.
- Offline candidate validation/publication through `store::recover_manifest_with_control`.
- Store inventory and Node maintenance-space preflight while traversing or settling the
  actor-ordered store cut.

Checkpoints occur between records, dictionary entries, sections, fold frames, copied pieces, rebuilt
parts, and independently punchable blocks. An individual unit is not split, so cancellation latency
is bounded by the largest unit currently being read, compressed, written, or checksummed rather than
by a real-time scheduler guarantee.

## Atomicity and restart behavior

Each operation deliberately interprets interruption according to its publication protocol:

- **Sync** checks once before WAL fsync. Once the durability boundary begins, TurnDB reports its real
  outcome and does not claim cancellation after accepted writes may have become durable.
- **Flush** checks during memtable/locator planning, after part construction, during bounded digest
  reads, and immediately before manifest publication. Cancellation may leave fold bytes durably
  sealed but unreachable; it removes the unpublished part and preserves the live memtable/manifest.
  Part encoding remains one uninterruptible work unit. Once manifest commit begins, ordinary crash
  recovery owns the result.
- **Compaction** writes an unreachable output part. Cancellation before manifest publication removes
  that part and leaves the live part set unchanged. Once manifest commit begins, cancellation is no
  longer observed because the crash-safe commit protocol owns the outcome.
- **Verification** is read-only after the embedding actor settles earlier writes. It returns no
  partial success report.
- **Refold** builds an unpublished fold generation and replacement parts. Cancellation removes all
  staged artifacts and preserves the live generation byte-exact. Once generation commit begins,
  TurnDB finishes handle, retention, and orphan cleanup without another cancellation checkpoint.
- **Punching** publishes the complete erased-block declaration before deallocating any bytes.
  Cancellation may therefore leave safe durable progress: some declared unreachable blocks have not
  yet been punched. A later call retries every declared block still present. This retry behavior also
  closes the same window after a process crash.
- **Strong erasure** accepts cancellation while determining which requested ids are present. Once its
  atomic tombstone batch is applied, cancellation is deferred until total merge and refold complete.
  Returning `cancelled` after logical deletion but before physical removal would make a retry mistake
  those ids for previously absent records and falsely report success.
- **Backup** copies and verifies into a private sibling file. Cancellation removes unpublished
  staging and leaves the requested artifact absent. The hard link is the final checkpoint; once it
  exists, TurnDB reports the publication outcome rather than cancellation.
- **Restore** validates and extracts into a private sibling directory. Cancellation removes staging
  and leaves the destination absent. The atomic no-replace rename is its final checkpoint.
- **Manifest recovery** holds exclusive writer locks while it discovers and completely validates
  retained candidates. Cancellation leaves the damaged live manifest and retained history
  unchanged. Promotion is its final checkpoint; after it begins, TurnDB reports the actual outcome.

An actor operation may have settled earlier accepted writes before a later checkpoint stops its main
work. That publication is ordered prerequisite work, not a partially published compaction or refold.
An already-expired deadline is checked before settling and therefore has no lifecycle side effect.

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
await store.punch({ signal: abort.signal });
await store.refold({ timeoutMs: 120_000 });
await store.erase(ids, { signal: abort.signal });
await store.backup('snapshot.turndb', { signal: abort.signal });
await restoreBackup('snapshot.turndb', 'restored', { timeoutMs: 120_000 });
await recoverManifest('damaged-store', {
  maxRollbackCommits: 0n,
  timeoutMs: 120_000,
  signal: abort.signal,
});
```

`timeoutMs` is converted to an absolute deadline before submission, so writer-actor queue time and
restore/recovery worker-scheduling time count. Zero is a deterministic pre-mutation refusal. A signal
aborted before submission is rejected at the JavaScript boundary; later aborts set the Rust token directly.
Both conditions reject with
`TurnDbError.code === "CANCELLED"`; the message distinguishes cancellation from deadline expiry.
Dropping or ignoring the Promise does not cancel the operation—pass a signal when cancellation is
required.

The native capability profile reports `lifecycleCancellation: true`. SQL planning/pulls expose their
own interruption controls. Sync intentionally has only a pre-fsync checkpoint, and flush's part
encoder remains an indivisible work unit; neither is represented as asynchronously killable work.
