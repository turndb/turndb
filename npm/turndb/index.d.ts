/**
 * turndb — a content-addressed columnar store for AI traces.
 *
 * `put` is not durable; `sync()` is the ACK point. `flush()` is separate again: it seals writes
 * into the columnar plane for OTHER readers. This handle sees its own unflushed writes without
 * either.
 */

/** An attribute value. Wrappers preserve distinctions JavaScript primitives cannot express. */
export type AttrValue =
  | string
  | number
  | boolean
  | bigint
  | Uint8Array
  | null
  | { i: number | bigint }
  | { f: number }
  | { fBits: string }
  | { u: number | bigint }
  | { timestampNs: number | bigint };

/**
 * Attributes as an object, or as pairs.
 *
 * **Prefer the array form when order or duplicate keys matter** — turndb preserves both because
 * byte-exact reconstruction depends on them, and a JS object can represent neither.
 */
export type Attrs = Record<string, AttrValue> | Array<[string, AttrValue]>;

export interface OpenOptions {
  /** Bytes gathered before a block seals. Default 4 MiB. Bigger compresses harder, costs more per read. */
  blockTarget?: number;
  /**
   * zstd level. **This package defaults to 3; the engine's own default is 19.**
   *
   * The divergence is deliberate: this build is single-threaded, so the block seal compresses on
   * the calling thread inside whichever `putBody` crosses the `blockTarget` boundary. Measured on
   * 4 MiB blocks (wasm, Node 22, a single workstation, synthetic bodies): ~1.7s per seal at level 19,
   * ~80ms at level 3.
   * Level 3 costs more disk; the delta varies materially with workload ordering and
   * configuration, so measure your own workload rather than trusting a figure.
   * An explicit `level: 19` buys that ratio back at that per-seal price — stated here and in the
   * README; nothing warns at runtime. `0` selects the engine default (currently 19). Write-side
   * only — a reader never needs to know it, so the choice is per-open, never a format commitment.
   */
  level?: number;
  /** Worst-case complete WAL frame bytes admitted for one record; defaults to 64 MiB. */
  maxRecordBytes?: number;
  /** Member frames plus commit marker admitted for one atomic batch; defaults to 256 MiB. */
  maxBatchBytes?: number;
  /** Ordered members admitted in one atomic batch; defaults to 4,096. */
  maxBatchRecords?: number;
  /** UTF-8 bytes admitted in an id, attribute name, or content name; defaults to 4 KiB. */
  maxIdentifierBytes?: number;
  /** Stored input admitted for one WAL, part-TOC/section, or fold-block frame; defaults to 512 MiB. */
  maxStoredFrameBytes?: number;
  /** Decoded output admitted for one part-TOC/section or fold-block frame; defaults to 512 MiB. */
  maxDecodedFrameBytes?: number;
  /** Entries visited in one filesystem directory enumeration; defaults to 100,000. */
  maxDirectoryEntries?: number;
  /** Physical frames admitted in one unflushed WAL; defaults to 100,000. */
  maxWalFrames?: number;
  /** Content blocks admitted in one fold generation; defaults to 1,000,000. */
  maxFoldBlocks?: number;
}

export interface ScanOptions {
  /** Inclusive lower bound. */
  from?: string;
  /** Exclusive upper bound. */
  to?: string;
  /**
   * Shorthand for the half-open range covering exactly this prefix. Overrides `from`/`to`.
   *
   * `''` means every id — the range is unbounded, not empty. A prefix of all-U+10FFFF is likewise
   * unbounded above, because no valid string sorts past it.
   */
  prefix?: string;
  /** Default 100. */
  limit?: number;
  /** Walk the same range backwards — what a newest-first view wants. */
  reverse?: boolean;
}

export type Compare = 'eq' | 'ne' | 'lt' | 'lte' | 'gt' | 'gte';

/**
 * Float comparisons: `eq`/`ne` are BIT equality — they match the exact stored NaN payload and
 * distinguish -0.0 from 0.0, honoring the store's byte-exactness promise. Ordering ops use IEEE
 * partial order, so no NaN satisfies any inequality and -0.0 orders equal to 0.0; `eq` therefore
 * does not imply `lte`. IEEE equality is expressible as `lte` && `gte`.
 *
 * `value` is tagged by exactly the rules {@link Attrs} uses on the write side — pass a BigInt for
 * an exact i64, `{ u }` for unsigned, `{ f }` to force a float, `{ timestampNs }` for a timestamp.
 */
export type Predicate =
  | { kind: 'id'; op: Compare; value: string }
  | { kind: 'attr'; name: string; op: Compare; value: AttrValue }
  | { kind: 'attr_exists'; name: string; present: boolean }
  | { kind: 'content_exists'; name: string; present: boolean };

export interface ContentSelect {
  name: string;
  /** `metadata` describes the value without reconstructing it and opens no fold block. */
  mode: 'metadata' | 'bytes';
}

export interface ScanRequest {
  /** Cooperative relative deadline. Zero refuses before returning a partial page. */
  timeoutMs?: number;
  /** Inclusive lower bound. */
  from?: string;
  /** Exclusive upper bound. */
  to?: string;
  /** Shorthand for the half-open range covering exactly this prefix. Overrides `from`/`to`. */
  prefix?: string;
  direction?: 'forward' | 'reverse';
  /** Opaque checked continuation from a previous page's `next`. */
  cursor?: string;
  /** Default 100. */
  limit?: number;
  /** Hard bound on candidate records examined. A partial page carries a cursor. */
  maxExamined?: number;
  /** Pre-predicate immutable-row plus memtable-entry ceiling; one id group may exceed it. */
  maxResolutionEntries?: number;
  /** Whole-page content byte ceiling; one oversized row is admitted to guarantee progress. */
  maxReconstructedBytes?: number | bigint;
  /** Attribute keys to return. Order and duplicate keys are preserved. */
  attrs?: string[];
  /** Named content values to describe or reconstruct. */
  contents?: ContentSelect[];
  predicates?: Predicate[];
}

export interface ProjectedContent {
  name: string;
  present: boolean;
  len?: bigint;
  pieces?: number;
  /** BLAKE3 of the exact reconstructed bytes; unavailable for values written by legacy formats. */
  identity?: string;
  /** Present only for a value selected with `mode: 'bytes'`. */
  bytes?: Uint8Array;
}

export interface ScanStats {
  durationNs: bigint;
  examined: number;
  returned: number;
  /** Resolved rows rejected from part metadata without projecting column values. */
  predicatePrunedRows: bigint;
  /** Repeat `(name, type)` occurrences beyond each first. Every occurrence is still returned. */
  duplicateAttrOccurrences: number;
  contentValuesReconstructed: number;
  reconstructedBytes: bigint;
  /** A matching row was left for the next page rather than crossing the reconstruction ceiling. */
  reconstructionBudgetExhausted: boolean;
  /** Exact operation-local storage reads. Zero fold reads on a metadata-only page. */
  io: {
    partSectionsTouched: bigint;
    partSectionCacheHits: bigint;
    partSectionCacheMisses: bigint;
    partStoredBytesRead: bigint;
    partRawBytesDecoded: bigint;
    foldBlocksTouched: bigint;
    foldBlockCacheHits: bigint;
    foldBlockCacheMisses: bigint;
    foldStoredBytesRead: bigint;
    foldRawBytesDecoded: bigint;
  };
  resolution: {
    physicalRows: bigint;
    supersededRows: bigint;
    tombstones: bigint;
    memtableEntries: bigint;
    budgetExhausted: boolean;
  };
}

export interface ScanRow {
  id: string;
  /** Order and duplicate keys preserved, exactly as stored. */
  attrs: Array<[string, AttrValue]>;
  contents: ProjectedContent[];
}

export interface ScanPage {
  rows: ScanRow[];
  /** Absent when the range is exhausted. */
  next?: string;
  stats: ScanStats;
}

export interface BatchRecord {
  id: string;
  body?: Uint8Array | string;
  attrs?: Attrs;
  /** Write a tombstone instead of a value. */
  delete?: boolean;
}

export interface NamedContent {
  name: string;
  bytes: Uint8Array | string;
}

export type WriteOperation =
  | { kind: 'put'; id: string; contents: NamedContent[]; attrs?: Attrs }
  | { kind: 'delete'; id: string };

export interface WriteResult {
  applied: number;
  /** True only when the engine completed the durability sync before returning. */
  durable: boolean;
}

export interface StoreRecord {
  id: string;
  body: Uint8Array;
  /** Order and duplicate keys preserved, exactly as stored. */
  attrs: Array<[string, AttrValue]>;
}

export interface Stats {
  records: number;
  parts: number;
}

export type BindingOperation =
  | 'capabilities' | 'readLimits' | 'putBody' | 'applyBatch' | 'write' | 'delete'
  | 'sync' | 'flush' | 'autoCompact' | 'maybeCompact' | 'get' | 'getText' | 'getRecord'
  | 'scanIds' | 'scan' | 'stats' | 'verify' | 'health' | 'metrics' | 'lifecycleEvents'
  | 'contentLiveness' | 'spaceUsage' | 'estimateRefoldSpace' | 'refold' | 'eraseIds' | 'close';

export type ContractOperation =
  | 'openWriter' | 'openSnapshot' | 'compiledCapabilities' | 'write' | 'sync' | 'flush'
  | 'scan' | 'explainScan' | 'schema' | 'readContent' | 'snapshot' | 'querySql' | 'seal'
  | 'verify' | 'spaceUsage' | 'compactBounded' | 'refold' | 'erase' | 'close';

/** What is actually callable through this npm/WASI binding. */
export interface Capabilities {
  contractVersion: 1;
  profile: 'wasi';
  binding: 'wasi';
  /** Stable Tier-1 contract operations. */
  operations: ContractOperation[];
  /** Package-specific convenience methods retained outside the cross-binding contract. */
  bindingOperations: BindingOperation[];
  partFormat: { write: number; readMax: number };
  writerExclusion: 'embedder_enforced';
  positionedIo: true;
  threads: false;
  columnar: boolean;
  sql: false;
  arrowIpc: false;
  reclamation: 'refold_only';
  cancellation: { scan: true; lifecycle: true };
  limits: { lifecycleEvents: number };
  controls: {
    /** Methods accepting cooperative `timeoutMs`; every name resolves on `Store`. */
    deadlineOperations: Array<
      'scan' | 'sync' | 'flush' | 'autoCompact' | 'maybeCompact' | 'verify'
      | 'contentLiveness' | 'spaceUsage' | 'estimateRefoldSpace' | 'refold' | 'eraseIds'
    >;
  };
  unavailable: {
    allocatedBytes: 'absent';
    cancellationToken: 'absent';
    atomicNoReplacePublication: 'absent';
  };
}

/** Mechanisms and format facts compiled into the guest, independent of binding reachability. */
export interface CompiledCapabilities {
  part_format_write: number;
  part_format_read_max: number;
  writer_exclusion: 'os_enforced' | 'embedder_enforced';
  positioned_io: boolean;
  threads: boolean;
  columnar: boolean;
  sql: boolean;
  portable_wasm: boolean;
  write_admission_limits: true;
  read_admission_limits: true;
  object_count_admission: true;
  store_space_usage: true;
  allocated_space_usage: boolean;
  format_migration: true;
  operation_metrics: true;
  part_distribution: true;
  content_liveness: true;
  lifecycle_event_journal: true;
  query_timings: true;
  sql_explain: false;
  max_record_bytes_default: number;
  max_batch_bytes_default: number;
  max_batch_records_default: number;
  max_identifier_bytes_default: number;
  max_stored_frame_bytes_default: number;
  max_decoded_frame_bytes_default: number;
  max_directory_entries_default: number;
  max_wal_frames_default: number;
  max_fold_blocks_default: number;
}

export declare function capabilities(): Promise<Capabilities>;
export declare function compiledCapabilities(): Promise<CompiledCapabilities>;

export interface OperationMetrics {
  attempts: bigint;
  succeeded: bigint;
  failed: bigint;
  cancelled: bigint;
  totalDurationNs: bigint;
  lastDurationNs: bigint;
  maxDurationNs: bigint;
}

/**
 * Cooperative relative deadline, measured by the guest clock. Zero refuses at the first safe
 * checkpoint. AbortSignal is deliberately absent: this single-threaded guest cannot observe a
 * token changing during a synchronous call.
 */
export interface DeadlineOptions { timeoutMs?: number }

export interface Metrics {
  /** Per-handle counters. They start fresh each time the store is opened. */
  openRecovery: OperationMetrics;
  recoveredWalFrames: bigint;
  sync: OperationMetrics;
  flush: OperationMetrics;
  compaction: OperationMetrics;
  backup: OperationMetrics;
  verification: OperationMetrics;
  verificationCorruptionFailures: bigint;
  punch: OperationMetrics;
  refold: OperationMetrics;
  erase: OperationMetrics;
  formatMigration: OperationMetrics;
  foldedContent: { pieces: bigint; dedupHits: bigint; logicalBytes: bigint; novelBytes: bigint };
}

export interface LifecycleEvent {
  sequence: bigint;
  operation: string;
  outcome: 'succeeded' | 'failed' | 'cancelled';
  errorCode: ErrorCode | null;
  durationNs: bigint;
}

/**
 * Lifecycle-operation history for this handle. Ordinary reads are not lifecycle operations: a
 * failed read throws a typed error and affects metrics, but does not append an event here.
 */
export interface LifecycleEventBatch {
  events: LifecycleEvent[];
  oldestAvailableSequence: bigint | null;
  latestSequence: bigint;
  droppedEvents: bigint;
  gap: boolean;
  capacity: number;
}

export interface FoldBlockSpace {
  blocks: bigint;
  rawBytes: bigint;
  storedBytes: bigint;
}

export interface ContentLiveness {
  livePieces: bigint;
  liveLogicalBytes: bigint;
  deadLogicalBytes: bigint;
  strandedDeadLogicalBytes: bigint;
  liveBlocks: FoldBlockSpace;
  reclaimableBlocks: FoldBlockSpace;
}

export type MeasuredBytes = { state: 'measured'; bytes: bigint } | { state: 'absent' };
export interface SpaceAmount {
  files: number;
  logicalBytes: bigint;
  allocatedBytes: MeasuredBytes;
}
export interface SpaceUsage {
  live: SpaceAmount;
  retainedOnly: SpaceAmount;
  unclassified: SpaceAmount;
  total: SpaceAmount;
  filesystemAvailableBytes: MeasuredBytes;
}

export type ReclamationOutcome =
  | { state: 'not_applicable' }
  | { state: 'not_reclaimed'; reason: 'stale_generation_left' }
  | {
      state: 'measured';
      /** Exact portable file-length delta, not allocated filesystem blocks. */
      logicalBytes: bigint;
      pieces?: number;
      /** WASI cannot measure allocated blocks; absence is not zero and not an operation failure. */
      allocatedBytes: { state: 'absent' };
    };
export interface ErasureResult {
  requested: number;
  erased: number;
  absent: number;
  remaining: number;
  reclamation: ReclamationOutcome;
}

export interface RefoldSpaceEstimate {
  sourceFoldLogicalBytes: bigint;
  sourcePartBytes: bigint;
  sourcePartSections: number;
  sourcePartRawSectionBytes: bigint;
  retainedOnlyLogicalBytesBefore: bigint;
  /** Advisory duplicate-generation estimate, explicitly not an admission bound. */
  estimatedStageBytes: bigint;
  estimateIsHardBound: false;
  filesystemAvailableBytes: MeasuredBytes;
}

export interface RefoldResult {
  partsIn: number;
  partsOut: number;
  recordsKept: number;
  recordsDropped: number;
  tombstonesDropped: number;
  piecesKept: number;
  piecesDropped: number;
  foldLogicalBytesBefore: bigint;
  foldLogicalBytesAfter: bigint;
  reclamation: Exclude<ReclamationOutcome, { state: 'not_applicable' }>;
}

export type ErrorCode =
  | 'INVALID_ARGUMENT'
  | 'CANCELLED'
  | 'RESOURCE_EXHAUSTED'
  | 'UNSUPPORTED'
  | 'CONTENTION'
  | 'NOT_FOUND'
  | 'CORRUPTION'
  | 'IO'
  | 'INTERNAL';

/** Every engine-reported failure, carrying a stable class and the engine's full message. */
export declare class TurndbError extends Error {
  readonly name: 'TurndbError';
  readonly code: ErrorCode;
}

export interface VerificationReport {
  /** Verification covers committed state; sync and flush staged writes before calling to include them. */
  scope: 'committed_snapshot';
  /** `incomplete` means verification succeeded but a legacy identity/digest was unavailable. */
  state: 'valid' | 'incomplete';
  retainedManifests: { state: 'verified' | 'not_applicable'; count: number };
  chain: { links: number; partDigests: number; undigestedParts: number };
  parts: number;
  partSections: number;
  fold: {
    segments: number;
    blocks: number;
    bytes: bigint;
    /** Crash residue outside the committed snapshot, reported rather than absorbed into `valid`. */
    trailingUncommittedBytes: bigint;
  };
  records: number;
  contentValues: number;
  contentBytes: bigint;
  contentIdentities: number;
  unidentifiedContentValues: number;
}

export interface StoreHealth {
  /** The handle answered. This is deliberately not an integrity verdict. */
  state: 'available';
  commit: bigint;
  foldGeneration: number;
  parts: number;
  partRows: bigint;
  memtableEntries: number;
  memtableBytes: number;
  walBytes: bigint;
  walFrames: bigint;
  foldDiskBytes: bigint;
  foldSegments: number;
  foldCacheHits: bigint;
  foldCacheMisses: bigint;
  foldCacheBytes: number;
  foldCacheBudget: number;
  foldBlockTargetBytes: number;
  foldSegmentMaxBytes: number;
  foldCompressionLevel: number;
  foldCompressionThreads: number;
  partCacheBytes: number;
  partCacheBudget: number;
  maxStoredFrameBytes: bigint;
  maxDecodedFrameBytes: bigint;
  maxDirectoryEntries: bigint;
  maxWalFrames: bigint;
  maxFoldBlocks: bigint;
  dedupWindowEntries: number;
  retainedCommits: number;
  punchedBlocks: bigint;
}

/**
 * Ids must be strings, and are refused if they are not — `String(value)` would encode `{}` as
 * `"[object Object]"`, silently aliasing it onto the literal string of that name. (`applyBatch` is
 * the exception: the engine rejects a non-string id there with the offending item's index, which is
 * a better message than the binding could give.)
 *
 * Ids, range bounds and attribute strings are refused if they contain an unpaired surrogate.
 *
 * JS strings are UTF-16 and can hold them; UTF-8 cannot represent them, and `TextEncoder` maps them
 * to U+FFFD silently — so two ids the caller believes are distinct would land on one record and the
 * second would overwrite the first. A `TurndbError` is thrown instead. U+FFFD itself is a valid
 * character and is accepted normally.
 */
export declare class Store {
  /** True once {@link Store.close} has run. */
  readonly closed: boolean;
  /** Guarantees of the compiled portable core. */
  capabilities(): Capabilities;
  /** Exact frame-byte and persistent object-count admission configured for this handle. */
  readLimits(): {
    maxStoredFrameBytes: number;
    maxDecodedFrameBytes: number;
    maxDirectoryEntries: number;
    maxWalFrames: number;
    maxFoldBlocks: number;
  };
  /** Write one record. Not durable until {@link Store.sync}. */
  putBody(id: string, body: Uint8Array | string, attrs?: Attrs): void;
  /** Apply many records atomically — all-or-nothing, so a crash cannot commit half an export. */
  applyBatch(records: BatchRecord[]): number;
  /**
   * Apply generic named-content records and deletions atomically.
   *
   * With `durable: true`, a successful return is the acknowledgement: the exact batch is durable.
   * A thrown error is not an acknowledgement and the caller must retain its source copy.
   */
  write(operations: WriteOperation[], options?: { durable?: boolean }): WriteResult;
  /** Tombstone a record. Not durable until {@link Store.sync}. */
  delete(id: string): void;
  /** Make everything written so far durable. **The ACK point.** */
  /** Last cancellable checkpoint: immediately before WAL fsync. */
  sync(options?: DeadlineOptions): void;
  /** Seal the memtable into an immutable part, making writes visible to other readers. */
  /** Last cancellable checkpoint: immediately before manifest publication. */
  flush(options?: DeadlineOptions): void;
  /**
   * Total merge when the live part list reaches the engine's threshold. Returns whether a merge ran.
   *
   * The stall is the caller's: wall time is linear in the store's on-disk content (~5s/GB at
   * level 19, wasm — synthetic stores up to 1.9 GB, a single workstation; level 3 unmeasured), on the
   * calling thread. Never fires on its own — schedule it when a
   * multi-second pause is acceptable, or bound the pause with {@link Store.maybeCompact}. Only
   * total merges settle deletes.
   */
  autoCompact(options?: DeadlineOptions): boolean;
  /**
   * Bounded compaction: if at least `trigger` parts are live (default 8), merge the oldest `run`
   * of them (default 4). Returns whether a merge ran.
   *
   * The latency-budget dial: the stall is capped by the merged run instead of the whole store.
   * Bounded merges never settle deletes — run {@link Store.autoCompact} occasionally if the store
   * sees deletions.
   */
  maybeCompact(opts?: { trigger?: number; run?: number; timeoutMs?: number }): boolean;
  /** The body, byte-exact, or `null` if absent or deleted. */
  get(id: string): Uint8Array | null;
  /**
   * The body decoded as UTF-8, or `null`.
   *
   * **Lossy on bodies that are not valid UTF-8**: invalid sequences decode to U+FFFD, so this is a
   * convenience for text bodies and not a round-trip. `get` returns the bytes exactly and is the
   * one to use when the body may be binary — re-encoding this string would not reproduce them.
   */
  getText(id: string): string | null;
  /** Body plus attributes, or `null`. */
  getRecord(id: string): StoreRecord | null;
  /** Live ids in range, in id order. The paging primitive. */
  scanIds(opts?: ScanOptions): string[];
  /**
   * One structured page: engine-side predicates, attribute and named-content projection, and a
   * checked continuation cursor.
   *
   * Unlike {@link Store.scanIds} the filtering and projection happen in Rust against exact stored
   * values. A page selecting no content opens no fold block.
   *
   * `timeoutMs` is cooperative and checked by the guest. `AbortSignal` is deliberately absent:
   * this single-threaded guest cannot observe a token changing during a synchronous call.
   */
  scan(request?: ScanRequest): ScanPage;
  stats(): Stats;
  /** Verify every integrity leg in the committed snapshot, returning exact counts. */
  verify(options?: DeadlineOptions): VerificationReport;
  /** Cheap operational facts; not a substitute for {@link Store.verify}. */
  health(): StoreHealth;
  metrics(): Metrics;
  lifecycleEvents(options?: { after?: bigint; limit?: number }): LifecycleEventBatch;
  contentLiveness(options?: DeadlineOptions): ContentLiveness;
  spaceUsage(options?: DeadlineOptions): SpaceUsage;
  /** Advisory duplicate-generation preflight. Writes nothing and requires a flushed memtable. */
  estimateRefoldSpace(options?: DeadlineOptions): RefoldSpaceEstimate | null;
  /**
   * Rewrite content from the live-reference set and report exact logical output facts.
   *
   * This does NOT promise media-byte non-recoverability: not on arbitrary or copy-on-write
   * filesystems, not through WASI, and not for copies already made.
   */
  refold(options?: DeadlineOptions): RefoldResult;
  /**
   * Atomically erase named ids and report query absence separately from reclamation.
   * This carries the same explicit refusal of media-byte non-recoverability as {@link Store.refold}.
   */
  /** Last cancellable checkpoint: before the atomic tombstone batch; after it, erasure completes. */
  eraseIds(ids: string[], options?: DeadlineOptions): ErasureResult;
  /**
   * Close the store and release its handle. Does NOT sync — call {@link Store.sync} first.
   *
   * A later `open` reuses this handle's WASI instance. Closing is therefore required before opening
   * any other store in this process. An abandoned handle is reclaimed when JavaScript collects it,
   * but collection has no timing guarantee and is not a substitute for `close`.
   *
   * Not "release the writer lock": this build holds no advisory lock. See {@link open}.
   */
  close(): void;
}

/**
 * Open (or create) a store at a `.turndb` file path.
 *
 * **At most one open writer per store file across every process.** This package is always the
 * `wasm32-wasip1` build and WASI has no advisory locking, so the engine cannot enforce
 * cross-process exclusion. Measured concurrent writers both received successful durability
 * acknowledgements while one writer's entire record set was silently discarded; the surviving
 * store remained internally consistent. The embedder must enforce this precondition before it can
 * treat an acknowledgement as durable fact.
 *
 * The parent directory is created if it does not exist, matching the Rust `Store::open_file` it wraps.
 *
 * Within one process, sequential opens — including opens of different directories — reuse one WASI
 * instance. The directory capability is switched between handles without widening the sandbox to a
 * common ancestor. Consequently only one `Store` may be open in a process at a time; use separate
 * processes when multiple stores must be held open concurrently.
 */
export declare function open(file: string, opts?: OpenOptions): Promise<Store>;

/** Read admission for a single-file open; the write-side limits do not apply to a reader. */
export type OpenFileOptions = Pick<
  OpenOptions,
  | 'maxStoredFrameBytes'
  | 'maxDecodedFrameBytes'
  | 'maxDirectoryEntries'
  | 'maxWalFrames'
  | 'maxFoldBlocks'
>;

/**
 * Open a store held in ONE FILE — a sealed pack or a growable container — READ-ONLY.
 *
 * Which form it is comes from the file's magic, not its extension, and both answer reads
 * identically. Neither has a writer role to take, so this open cannot contend with anything and
 * needs no WAL replay; in exchange the returned handle **refuses every mutating method**
 * (`putBody`, `applyBatch`, `write`, `delete`, `sync`, `flush`, compaction, erasure) with a
 * `TurndbError` naming the handle as read-only.
 *
 * Reads available: {@link Store.get}, {@link Store.getText}, {@link Store.getRecord},
 * {@link Store.scan}, {@link Store.scanIds}, {@link Store.stats}.
 *
 * WASI preopens directories rather than files, so the file's parent is the capability the guest is
 * granted and the file is addressed by name inside it.
 */
export declare function openFile(file: string, opts?: OpenFileOptions): Promise<Store>;

/**
 * Which single-file form a path holds, or `null` for a directory or a file carrying neither magic.
 *
 * Reading does not need this — {@link openFile} dispatches on its own — but tooling that must know
 * whether a file can still be appended to before it plans to does.
 */
export declare function singleFileKind(file: string): Promise<'pack' | 'container' | null>;

/**
 * The first id that cannot start with `prefix`, or `null` when the range is unbounded above.
 *
 * Exported because its boundary behaviour is part of the contract: it carries by code POINT, so a
 * prefix ending at U+FFFF or inside an astral pair yields a valid bound rather than an inverted
 * range or an unpaired surrogate. Compare bounds as UTF-8 bytes — ids sort by bytes, and JS `<`
 * compares UTF-16 code units, which disagree for astral characters.
 */
export declare function prefixUpperBound(prefix: string): string | null;

declare const _default: {
  open: typeof open;
  capabilities: typeof capabilities;
  Store: typeof Store;
  TurndbError: typeof TurndbError;
};
export default _default;
