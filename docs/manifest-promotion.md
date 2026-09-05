# Manifest promotion

TurnDB refuses to open a store whose current `MANIFEST` is missing or damaged. It does not silently
select an older retained manifest because doing so can abandon durability-acknowledged mutations.
Manifest promotion is an
explicit, exclusive operator action with a reported data-loss bound.

This procedure repairs the store's manifest authority from the bounded retained-manifest log the
store file carries as members. It is not a repair system for damaged content. If no retained manifest revision is
fully readable, restore a verified backup or healthy replica.

## Safety contract

`store::promote_manifest_file` and the equivalent Node and CLI operations:

1. Acquire the writer lock — `flock` on Unix or `LockFileEx` on Windows, on the store container
   handle. **On native builds** another open writer causes a typed contention failure, so manifest
   promotion cannot race ingest or maintenance. **On `wasm32-wasip1` that
   lock gates nothing** — see [the store shape](../FORMAT.md#store-shape) — so another open writer
   is not detected and the procedure can promote a manifest underneath one. There the exclusion is
   the embedder's, for manifest promotion exactly as for ordinary writes.
2. Refuse an intact current `MANIFEST`. Manifest promotion cannot be used as an accidental rollback mechanism.
3. Before treating the bytes as recoverable current-format history, refuse any retained member
   whose canonical name disagrees with its internal revision, and refuse parse-valid retained
   revisions that cross fold generations. Those are global physical-identity contradictions, not
   damage that rollback is authorized to normalize.
4. Examine retained manifest revisions from newest to oldest and find the nearest *fully usable*
   candidate whose complete surviving retained ancestry is valid, adjacent, linked, and reopenable.
   A damaged older revision makes every newer candidate that would retain it unusable; the search
   may continue to an older candidate that atomically abandons the damage within the authorized
   rollback bound.
5. Open only the exact fold prefixes authorized by that candidate and its surviving ancestry. Valid or damaged bytes from later
   appends and later segments are outside the candidate and cannot affect its result.
6. Verify the fold prefix, manifest-pinned part digests, every part section, every visible content
   program, every referenced piece hash and length, and every persisted whole-content identity.
7. Refuse if reaching the nearest valid candidate exceeds the caller's rollback allowance.
8. Publish with one container-state flip that installs the retained bytes as `MANIFEST`, drops the
   abandoned newer timeline's retained members, and removes part/fold members no surviving
   authority names, atomically.

Validation reconstructs each content value incrementally into a BLAKE3 hasher; it does not allocate
the complete value. Part digests and section checksums are also read in bounded chunks. The
controlled Rust and Node APIs accept cooperative cancellation/deadlines throughout discovery and
validation. The final checkpoint is immediately before manifest promotion: after publication begins,
TurnDB completes the crash-safe protocol rather than reporting cancellation. If the promoted
authority becomes selected but the operation obtains no publication acknowledgement, the error says
so explicitly and the caller must reopen to determine what survives a crash.
The report includes the selected manifest revision, rollback distance, records and content values validated,
part/section counts, and fold segment/block/byte counts.

The default allowance is zero. This permits replacement by the newest retained manifest revision—for example,
repairing bit rot in the current `MANIFEST` from its byte-identical retained copy—but refuses abandoning
any newer retained manifest revision. Rollback distance is the numeric difference between the newest retained
manifest revision and the nearest fully validated candidate.

## CLI procedure

Stop the application and keep an immutable copy of the damaged store file before manifest promotion. First
try the lossless default:

```sh
turndb recover STORE.turndb
```

If the command reports that rollback is required, investigate the damaged manifest revisions and choose whether
the reported loss is acceptable. Authorize exactly that distance (or another deliberate upper bound):

```sh
turndb recover STORE.turndb --max-rollback 2
```

On success, run a normal deep verification before returning the store to service:

```sh
turndb verify STORE.turndb --deep
```

A failed attempt does not publish the candidate. A successful rollback permanently removes newer
retained manifests from this store, so preserve the pre-promotion copy for diagnosis. Promotion
and removal are the same container-state flip, so no crash can leave both the promoted timeline and
the abandoned one on disk, and re-running an interrupted promotion promotes the same target.

## Rust API

```rust
use turndb::{
    control::OperationControl,
    fold::FoldCfg,
    store::{promote_manifest_file_with_limits_and_control, ManifestPromotionOptions},
};

let report = promote_manifest_file_with_limits_and_control(
    "damaged-store.turndb".as_ref(),
    FoldCfg::default(),
    ManifestPromotionOptions { max_rollback_commits: 0 },
    turndb::read_limits::ReadLimits::default(),
    &OperationControl::default(),
)?;
println!("promoted manifest revision {}", report.commit);
# Ok::<(), anyhow::Error>(())
```

The `ManifestPromotionError::Healthy`, `ManifestPromotionError::RollbackLimit`, and
`ManifestPromotionError::NoUsableCandidate` variants report manifest-promotion outcomes; they distinguish
operator refusal from corruption exhaustion.
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

- `CONTENTION` when a writer or another manifest promotion holds the store file.
- `INVALID_ARGUMENT` when the current `MANIFEST` is intact or the rollback allowance is insufficient.
- `CORRUPTION` when no retained manifest revision is fully usable.
- `NOT_FOUND` or `IO` for classified filesystem failures.
- `CANCELLED` when the signal or deadline stops validation before promotion.

The Node call runs off the event loop. Under WASI, TurnDB cannot enforce advisory writer locks; the
embedder must provide single-writer and manifest-promotion exclusion just as it must for ordinary writes.
