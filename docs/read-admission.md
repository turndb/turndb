# Atomic frame read admission

TurnDB checks persisted frame lengths before allocating either their stored input or decoded output.
This is separate from cache budgeting: a cache can evict an entry after materialization, but cannot
protect the allocation needed to materialize that one entry.

`ReadLimits` has two inclusive runtime ceilings:

| Rust field | native/portable Node option | default |
|---|---|---:|
| `max_stored_frame_bytes` | `maxStoredFrameBytes` | 512 MiB |
| `max_decoded_frame_bytes` | `maxDecodedFrameBytes` | 512 MiB |

The values are per open handle and are not persisted. Raising or lowering them does not create a
format dialect. A store containing a larger legitimate atomic frame can be opened by deliberately
raising the policy; the ordinary default refuses it with `RESOURCE_EXHAUSTED` rather than attempting
the allocation.

## Covered boundaries

The stored limit is checked before reading a complete WAL payload, part TOC, selected part section,
or fold-block frame. The decoded limit is checked before decoding a part TOC, selected part section,
or fold block. Stored-codec frames are subject to both.

Fold directory rebuilding applies the same checks before its reusable scan buffer grows. An
over-budget frame is a typed policy refusal, not a crash-tail boundary: writer recovery will not
truncate valid committed bytes merely because the process was reopened under a stricter profile.
Part sections remain lazy, so opening and metadata-only access can succeed while a later request for
one oversized selected column fails. Unselected sibling sections remain untouched and uncharged.

Writers use the policy as well. Fold gathering treats the smaller of the stored and decoded ceilings
as an additional seal target. If the next piece would cross it, the current non-empty block seals
first; small records therefore continue making progress under a strict profile. One indivisible
piece larger than either ceiling is refused before fold mutation. Flush, compaction, refold, and
format migration check every output part section and TOC before landing the completeness footer, so
the handle never publishes an immutable frame it cannot reopen.

Record and atomic-batch preflight includes every proposed folded piece. A late oversized batch member
therefore fails before an earlier member can alter the fold window or WAL. The deterministic complete
WAL-frame charge is also admitted, so many small pieces cannot aggregate into an unreopenable frame.

Part sections are the remaining indivisible output unit. If a complete column built from the current
memtable or compaction run exceeds policy, that operation returns `RESOURCE_EXHAUSTED` and leaves no
published output. The caller can use a larger read profile, flush more frequently, or select smaller
bounded compaction inputs. TurnDB does not silently split one logical column into an undocumented
format extension.

## API surfaces

Rust embedders set `StoreOptions::read_limits`. Explicit variants are also available for `Part`,
`Fold`, directory `ReadStore` snapshots, retained snapshots, and packed readers. Convenience opens
retain the defaults. `Store::read_limits`, `ReadStore::read_limits`, and writer `health()` expose the
effective policy.

Native Node writer and independently opened snapshot options accept the two bigint fields. Defaults
are present in `capabilities()`, and writer `health()` reports effective values. Writer-created
snapshots inherit the writer's policy. Offline manifest recovery and staged backup restore accept the
same ceilings for their complete validation passes.

The portable package accepts positive u32 values, reports defaults in its compiled capability
profile, and returns effective values from `store.readLimits()`. Its original `tdb_open` ABI remains
available with defaults; the bundled wrapper uses `tdb_open_v2` to carry explicit values.

Invalid zero or out-of-address-space policies are `INVALID_ARGUMENT`. A valid policy rejecting
persisted or newly built bytes is `RESOURCE_EXHAUSTED`. The Rust causes are typed
`ReadAdmissionError` variants, so bindings do not classify rendered prose.

## Related limits

These controls do not replace write admission, scan reconstruction/work ceilings, SQL memory pools,
or cache budgets. Pack TOC counts/names and metadata bytes use `PackLimits`; manifests and candidate
dictionaries have supported-reader ceilings; advisory fold sidecars derive their maximum from the
segment they describe. Together those boundaries distinguish metadata admission, atomic data-plane
admission, retained cache residency, and caller-owned result buffers instead of claiming one
misleading total-process memory limit.
