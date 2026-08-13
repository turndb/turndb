# Manifest recovery

TurnDB refuses to open a store whose live `MANIFEST` is missing or damaged. It does not silently
select an older retained manifest because doing so can abandon acknowledged commits. Recovery is an
explicit, exclusive operator action with a reported data-loss bound.

This procedure repairs the store's commit point from the bounded retained-manifest log the store
file carries as members. It is not a repair system for damaged content. If no retained commit is
fully readable, restore a verified backup or healthy replica.

## Safety contract

`store::recover_manifest_file` and the equivalent Node and CLI operations:

1. Acquire the writer lock — `flock` on the store file. **On Unix** a live writer causes a typed
   contention failure, so recovery cannot race ingest or maintenance. **On `wasm32-wasip1` that
   lock gates nothing** — see [the writer lock](../FORMAT.md#the-writer-lock) — so a live writer
   is not detected and recovery can promote a manifest underneath one. There the exclusion is the
   embedder's, for recovery exactly as for ordinary writes.
2. Refuse an intact live manifest. Recovery cannot be used as an accidental rollback mechanism.
3. Examine retained manifests from newest to oldest and find the nearest *fully usable* candidate.
4. Open only the exact fold prefix authorized by that candidate. Valid or damaged bytes from later
   appends and later segments are outside the candidate and cannot affect its result.
5. Verify the fold prefix, manifest-pinned part digests, every part section, every visible content
   program, every referenced piece hash and length, and every persisted whole-content identity.
6. Refuse if reaching the nearest valid candidate exceeds the caller's rollback allowance.
7. Publish with ONE commit flip that installs the retained bytes as `MANIFEST` and drops the
   abandoned newer timeline's retained members together, atomically.

Validation reconstructs each content value incrementally into a BLAKE3 hasher; it does not allocate
the complete value. Part digests and section checksums are also read in bounded chunks. The
controlled Rust and Node APIs accept cooperative cancellation/deadlines throughout discovery and
validation. The final checkpoint is immediately before manifest promotion: after publication begins,
TurnDB completes the crash-safe protocol and reports its actual outcome rather than cancellation.
The report includes the selected commit, rollback distance, records and content values validated,
part/section counts, and fold segment/block/byte counts.

The default allowance is zero. This permits replacement by the newest retained commit—for example,
repairing bit rot in the live manifest from its byte-identical retained copy—but refuses abandoning
any newer retained commit. Rollback distance is the numeric difference between the newest retained
commit and the nearest fully validated candidate.

## CLI procedure

Stop the application and keep an immutable copy of the damaged store file before recovery. First
try the lossless default:

```sh
turndb recover STORE.turndb
```

If the command reports that rollback is required, investigate the damaged commits and choose whether
the reported loss is acceptable. Authorize exactly that distance (or another deliberate upper bound):

```sh
turndb recover STORE.turndb --max-rollback 2
```

On success, run a normal deep verification before returning the store to service:

```sh
turndb verify STORE.turndb --deep
```

A failed attempt does not publish the candidate. A successful rollback permanently removes newer
retained manifests from this store, so preserve the pre-recovery copy for diagnosis. Promotion
and removal are the SAME superblock flip, so no crash can leave both the promoted timeline and
the abandoned one on disk, and re-running an interrupted recovery promotes the same target. If a
store carries stranded residue anyway (recovered by a build predating this protocol, or restored
from a copy taken mid-recovery), the next writer open durably removes any retained manifest newer
than the live commit before replaying the WAL.

## Rust API

```rust
use turndb::{
    control::OperationControl,
    fold::FoldCfg,
    store::{recover_manifest_file_with_limits_and_control, RecoveryOptions},
};

let report = recover_manifest_file_with_limits_and_control(
    "damaged-store.turndb".as_ref(),
    FoldCfg::default(),
    RecoveryOptions { max_rollback_commits: 0 },
    turndb::read_limits::ReadLimits::default(),
    &OperationControl::default(),
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
const abort = new AbortController();

const report = await recoverManifest('/data/damaged-store.turndb', {
  maxRollbackCommits: 0n,
  timeoutMs: 120_000,
  signal: abort.signal,
});
```

Counts and byte values are returned as `bigint`. Failures use stable `TurnDbError` codes:

- `CONTENTION` when a writer or another recovery holds the store file.
- `INVALID_ARGUMENT` for a healthy store or insufficient rollback allowance.
- `CORRUPTION` when no retained manifest is fully usable.
- `NOT_FOUND` or `IO` for classified filesystem failures.
- `CANCELLED` when the signal or deadline stops validation before promotion.

The Node call runs off the event loop. Under WASI, TurnDB cannot enforce advisory writer locks; the
embedder must provide single-writer and recovery exclusion just as it must for ordinary writes.
