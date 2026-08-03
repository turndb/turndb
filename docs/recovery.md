# Manifest recovery

TurnDB refuses to open a store whose live `MANIFEST` is missing or damaged. It does not silently
select an older retained manifest because doing so can abandon acknowledged commits. Recovery is an
explicit, exclusive operator action with a reported data-loss bound.

This procedure repairs the store's commit point from its bounded retained-manifest log. It is not a
repair system for damaged content. If no retained commit is fully readable, restore a verified
backup or healthy replica.

## Safety contract

`store::recover_manifest` and the equivalent Node and CLI operations:

1. Acquire the writer lock for every fold generation present in the store. A live writer causes a
   typed contention failure; recovery never races ingest or maintenance.
2. Refuse an intact live manifest. Recovery cannot be used as an accidental rollback mechanism.
3. Examine retained manifests from newest to oldest and find the nearest *fully usable* candidate.
4. Open only the exact fold prefix authorized by that candidate. Valid or damaged bytes from later
   appends and later segments are outside the candidate and cannot affect its result.
5. Verify the fold prefix, manifest-pinned part digests, every part section, every visible content
   program, every referenced piece hash and length, and every persisted whole-content identity.
6. Refuse if reaching the nearest valid candidate exceeds the caller's rollback allowance.
7. Durably publish the retained bytes as `MANIFEST`, then remove retained manifests from the
   abandoned newer timeline.

Validation reconstructs each content value incrementally into a BLAKE3 hasher; it does not allocate
the complete value. Recovery is nevertheless a full offline scan and has no deadline or cancellation
control yet. Its report includes the selected commit, rollback distance, records and content values
validated, part/section counts, and fold segment/block/byte counts.

The default allowance is zero. This permits replacement by the newest retained commit—for example,
repairing bit rot in the live manifest from its byte-identical retained copy—but refuses abandoning
any newer retained commit. Rollback distance is the numeric difference between the newest retained
commit and the nearest fully validated candidate.

## CLI procedure

Stop the application and keep an immutable copy of the damaged directory before recovery. First try
the lossless default:

```sh
turndb recover STORE_DIR
```

If the command reports that rollback is required, investigate the damaged commits and choose whether
the reported loss is acceptable. Authorize exactly that distance (or another deliberate upper bound):

```sh
turndb recover STORE_DIR --max-rollback 2
```

On success, run a normal deep verification before returning the store to service:

```sh
turndb verify STORE_DIR --deep
```

A failed attempt does not publish the candidate. A successful rollback permanently removes newer
retained manifests from this store directory, so preserve the pre-recovery copy for diagnosis.

## Rust API

```rust
use turndb::{
    fold::FoldCfg,
    store::{recover_manifest, RecoveryOptions},
};

let report = recover_manifest(
    "damaged-store".as_ref(),
    FoldCfg::default(),
    RecoveryOptions { max_rollback_commits: 0 },
)?;
println!("recovered commit {}", report.commit);
# Ok::<(), anyhow::Error>(())
```

`RecoveryError::Healthy`, `RecoveryError::RollbackLimit`, and
`RecoveryError::NoUsableCandidate` distinguish operator refusal from corruption exhaustion.
`fold::WriterLocked` remains the typed contention condition.

## Node API

```js
const { recoverManifest } = require('@turndb/native');

const report = await recoverManifest('/data/damaged-store', {
  maxRollbackCommits: 0n,
});
```

Counts and byte values are returned as `bigint`. Failures use stable `TurnDbError` codes:

- `CONTENTION` when a writer or another recovery owns a fold lock.
- `INVALID_ARGUMENT` for a healthy store or insufficient rollback allowance.
- `CORRUPTION` when no retained manifest is fully usable.
- `NOT_FOUND` or `IO` for classified filesystem failures.

The Node call runs off the event loop but remains an offline, non-cancellable operation. Under WASI,
TurnDB cannot enforce advisory writer locks; the embedder must provide single-writer and recovery
exclusion just as it must for ordinary writes.
