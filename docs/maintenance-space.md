# Maintenance space accounting and preflight

TurnDB exposes storage facts and estimates without choosing an operator's disk-pressure policy.
There are two deliberately different contracts.

## Exact store inventory

`Store::space_usage()` and Node `store.spaceUsage(options)` traverse regular files beneath the store
and classify each file exactly once:

- `live`: required by the current manifest, WAL, or current fold generation;
- `retainedOnly`: not current, but pinned by a retained time-travel manifest;
- `unclassified`: not proven reachable by either authority.

`unclassified` does **not** mean reclaimable. It can include interrupted staging that a later open
will sweep, but it can also be an operator-owned file. Reporting it is not deletion authority.
Counts and logical lengths are portable. On Unix, `allocatedBytes` uses filesystem block counts, so
punched sparse fold regions are not misreported as occupied. `filesystemAvailableBytes` uses the
bytes available to the current user. Both fields are absent on a platform that cannot prove them;
the capability profile reports `allocatedSpaceUsage`.

The inventory parses the retained manifest window and can therefore fail on damaged retention
metadata. It is intentionally separate from constant-work `health()`.

## Operation preflight

`Store::estimate_compaction_space(budget)` reports the exact selected plan, compressed input bytes,
section count, uncompressed section bytes, bytes the immediately preceding retained manifest will
continue to pin, and current filesystem availability. Node
`estimateCompactionSpace(budget, options)` first settles the actor cut, then returns the same facts.

`Store::estimate_refold_space()` reports exact current fold length, part file bytes, section/raw
bytes, retention-only bytes before refold, and filesystem availability. Node
`estimateRefoldSpace(options)` likewise settles earlier actor work first.

Both return `estimatedStageBytes`. The estimate uses uncompressed section bytes plus explicit row,
section, and format allowance; refold also includes a complete logical copy of the current fold.
It is intentionally marked `estimateIsHardBound: false`. Compression and rebuilt index layout are
not known until execution, so presenting the number as guaranteed admission would be false. An
embedder may apply a safety factor, refuse under its own reserve threshold, or compare it with a
separate quota. TurnDB does not reserve filesystem space, and another process can consume free bytes
after preflight.

These methods, including inventory, accept ordinary lifecycle deadlines/cancellation in Node.
Preflight is evidence, not a combined plan-and-execute transaction: later writes can change the next
compaction cut. The executing `compactBounded` call still reports the exact plan and actual output it
published.
