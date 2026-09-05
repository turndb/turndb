# Resource budgets and overload behavior

TurnDB exposes bounded work and memory at the layer that can enforce each limit. These are runtime
policies, not persisted schema or format commitments.

| Resource | Control | Refusal/progress evidence |
|---|---|---|
| Native actor backlog | `commandQueueCapacity` | immediate typed `BUSY`; no hidden JS queue |
| Record/batch admission | `maxRecordBytes`, `maxBatchBytes`, `maxBatchRecords`, `maxIdentifierBytes` | typed `INVALID_ARGUMENT` or `RESOURCE_EXHAUSTED` before WAL mutation |
| Atomic persisted frames | `maxStoredFrameBytes`, `maxDecodedFrameBytes` (`ReadLimits`) | typed refusal before stored/decoded allocation; early fold-block finalization preserves small-record progress |
| Persistent object counts | `maxDirectoryEntries`, `maxWalFrames`, `maxFoldBlocks` (`ReadLimits`) | typed refusal before directory/vector growth and writer mutation; sparse ids cannot size allocations |
| Structured page work | `maxExamined`, `maxResolutionEntries`, `maxReconstructedBytes`, `limit` | continuation cursor plus explicit budget-exhausted flags |
| SQL execution memory | per-query `maxMemoryBytes`, aggregate `maxConcurrentSqlMemoryBytes` | typed `RESOURCE_EXHAUSTED`; reservations visible and released at every terminal path |
| Part-merge work | `maxInputParts`, `maxInputRows`, `maxInputBytes` | exact selected plan or typed too-small refusal |
| Fold decompression cache | `foldCacheBytes` | current bytes and effective budget in `health()` |
| Immutable-part cache | `partCacheBytes` | current bytes and effective shared budget in `health()` |
| Compression/read tradeoff | `blockTargetBytes`, `compressionLevel`, `compressionThreads`, `segmentMaxBytes` | effective values in `health()`; invalid settings refused at open |
| Lifecycle event retention | fixed bounded journal | cumulative evictions and cursor-specific `gap` |
| Manifest and dictionary candidates | supported-reader ceilings; structurally derived sidecar bound | refusal before whole-file allocation; malformed advisory sidecars fall back, but explicit `ReadLimits` never do |

Rust groups storage settings in `StoreOptions`, containing `FoldCfg`, `WriteLimits`, `ReadLimits`, and one cache
budget shared by every immutable part in that handle. `Store::open` and `Store::open_with_limits`
remain source-compatible convenience entry points. Native Node passes the corresponding storage
settings through the same options seam rather than reconstructing storage machinery in JavaScript.

Fold block target, compression level, threads, segment size, cache sizes, and read admission affect future work and
resident memory, not how existing bytes are interpreted. A store may be reopened with different
values. Cache budgets admit one atomic cache entry where necessary, and maintenance operations retain
their existing crash/cancellation boundaries; a budget never weakens durability.

No single process-wide "memory limit" is claimed. Output buffers belong to callers, the OS page cache
is external, and DataFusion documents allocations outside its execution pool. The health, metrics,
query stats, and preflight APIs provide the separate facts needed for an embedder to set policy and
detect pressure honestly.

Part sections and fold blocks remain atomic codec units, but their stored and decoded allocations are
now bounded independently from cache residency. Writer outputs use the same policy and refuse before
publication; fold blocks finalize early for progress. See [atomic frame read admission](read-admission.md)
and the [security review](security-review.md). Directory enumeration, WAL replay, and fold-directory
growth have independent count admission; see
[persistent object-count admission](object-admission.md).
