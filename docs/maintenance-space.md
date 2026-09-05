# Maintenance space accounting and preflight

TurnDB exposes storage facts and estimates without choosing an operator's disk-pressure policy.
There are two deliberately different contracts.

## Exact store inventory

`Store::space_usage()` and Node `store.spaceUsage(options)` traverse the container member directory
and classify each named member exactly once; the hot WAL sidecar is counted in the literal `live` report field:

- `live`: required by the current store authority or WAL, including a manifest revision's referenced fold generation;
- `retainedOnly`: not current, but pinned by a retained time-travel manifest;
- `unclassified`: not proven reachable by either authority.

`unclassified` does **not** mean reclaimable. It includes free extents and named members not proven
reachable by either manifest authority. Reporting it is not deletion authority. Counts and logical
lengths are portable. The additive `total` covers member payloads, free extents, and the WAL; it does
not pretend that container superblocks, member-directory bytes, or alignment padding belong to a
reachability class. `allocatedBytes` is currently absent on every platform:
the single container interleaves `live`, `retainedOnly`, and free extents, and TurnDB does not mislabel a
structural zero as measured allocation. `filesystemAvailableBytes` independently reports bytes
available to the current user where the platform exposes them. The compiled capability reports
`allocatedSpaceUsage: false`; `inPlaceDeallocation` separately reports whether physical deallocation exists.

The inventory parses the retained manifest-revision window and can therefore fail on damaged retention
metadata. It is intentionally separate from constant-work `health()`.

## Operation preflight

`Store::estimate_compaction_space(budget)` reports the exact selected plan, compressed input bytes,
section count, uncompressed section bytes, bytes the immediately preceding retained manifest revision will
continue to pin, and current filesystem availability. Node
`estimateCompactionSpace(budget, options)` first synchronizes, publishes, and settles earlier
accepted mutations, then returns the same facts.

`Store::estimate_refold_space()` reports the exact length of the fold generation referenced by the
current manifest revision, or zero at the canonical origin, plus part member bytes, section/raw
bytes, retention-only bytes before refold, and filesystem availability. Node
`estimateRefoldSpace(options)` likewise synchronizes, publishes, and settles earlier accepted mutations first.

Both return `estimatedStageBytes`. The estimate uses uncompressed section bytes plus explicit row,
section, and format allowance; refold also includes a complete logical copy of that fold generation.
It is intentionally marked `estimateIsHardBound: false`. Compression and rebuilt index layout are
not known until execution, so presenting the number as guaranteed admission would be false. An
embedder may apply a safety factor, refuse under its own reserve threshold, or compare it with a
separate quota. TurnDB does not reserve filesystem space, and another process can consume free bytes
after preflight.

These methods, including inventory, accept ordinary lifecycle deadlines/cancellation in Node.
Preflight is evidence, not a combined plan-and-execute transaction: later writes can change the next
part set referenced by the current manifest revision. The executing `compactBounded` call still reports the exact plan and actual output it
published.
