/// <reference types="node" />

export type Attr =
  | { name: string; kind: 'string'; stringValue: string }
  | { name: string; kind: 'int'; intValue: bigint }
  | { name: string; kind: 'float'; floatValue: number }
  | { name: string; kind: 'bool'; boolValue: boolean }
  | { name: string; kind: 'uint'; uintValue: bigint }
  | { name: string; kind: 'binary'; binaryValue: Buffer }
  | { name: string; kind: 'timestamp_ns'; timestampNsValue: bigint }
  | { name: string; kind: 'null' };

export interface Content {
  name: string;
  bytes: Buffer;
}

export type WriteOp =
  | { kind: 'put'; id: string; contents?: Content[]; attrs?: Attr[] }
  | { kind: 'delete'; id: string };

export type Compare = 'eq' | 'ne' | 'lt' | 'lte' | 'gt' | 'gte';

/**
 * Float comparisons: `eq`/`ne` are BIT equality — they match the exact stored NaN payload and
 * distinguish -0.0 from 0.0, honoring the store's byte-exactness promise. Ordering ops use IEEE
 * partial order, so no NaN satisfies any inequality and -0.0 orders equal to 0.0; `eq` therefore
 * does not imply `lte`. IEEE equality is expressible as `lte` && `gte`.
 */
export type Predicate =
  | { kind: 'id'; op: Compare; idValue: string }
  | { kind: 'attr'; op: Compare; value: Attr }
  | { kind: 'attr_exists'; name: string; present: boolean }
  | { kind: 'content_exists'; name: string; present: boolean };

export interface ScanRequest {
  from?: string;
  to?: string;
  direction?: 'forward' | 'reverse';
  cursor?: string;
  limit?: number;
  maxExamined?: number;
  /** Pre-predicate immutable-row plus memtable-entry ceiling; one id group may exceed it. */
  maxResolutionEntries?: number;
  /** Whole-page content byte ceiling; one oversized row is admitted to guarantee progress. */
  maxReconstructedBytes?: bigint;
  /** Milliseconds from submission, including actor-queue wait; zero cancels before scan work. */
  timeoutMs?: number;
  signal?: AbortSignal;
  attrs?: string[];
  contents?: Array<{ name: string; mode: 'metadata' | 'bytes' }>;
  predicates?: Predicate[];
}

export interface ProjectedContent {
  name: string;
  present: boolean;
  len?: bigint;
  pieces?: number;
  /** BLAKE3 of the exact reconstructed bytes; unavailable for values written by legacy formats. */
  identity?: string;
  bytes?: Buffer;
}

export interface ScanPage {
  rows: Array<{ id: string; attrs: Attr[]; contents: ProjectedContent[] }>;
  next?: string;
  stats: {
    /** Complete successful storage-page execution; excludes actor queue wait. */
    durationNs: bigint;
    examined: number;
    returned: number;
    duplicateAttrOccurrences: number;
    contentValuesReconstructed: number;
    reconstructedBytes: bigint;
    reconstructionBudgetExhausted: boolean;
    /** Exact reads attributable to this page; activity by concurrent scans is excluded. */
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
    /** Work used to establish newest-wins candidates before predicates are evaluated. */
    resolution: {
      physicalRows: bigint;
      supersededRows: bigint;
      tombstones: bigint;
      memtableEntries: bigint;
      budgetExhausted: boolean;
    };
  };
}

export interface ScanExplanation {
  direction: 'forward' | 'reverse';
  usesCursor: boolean;
  /** Bounds after applying a checked cursor; lower inclusive, upper exclusive. */
  effectiveFrom?: string;
  effectiveTo?: string;
  emptyRange: boolean;
  projectedAttrs: string[];
  requiredAttrs: string[];
  predicateOnlyAttrs: string[];
  projectedContents: Array<{ name: string; mode: 'metadata' | 'bytes' }>;
  requiredContents: string[];
  predicateOnlyContents: string[];
  reconstructedContents: string[];
  idPredicates: number;
  attrPredicates: number;
  contentPredicates: number;
  limit: number;
  maxExamined: number;
  maxResolutionEntries: number;
  maxReconstructedBytes: bigint;
  /** Exact physical scope before newest-wins resolution; not estimated result cardinality. */
  physical: {
    immutablePartsConsidered: bigint;
    immutablePartsWithRows: bigint;
    immutableRowsInBounds: bigint;
    memtableEntriesInBounds: bigint;
  };
}

export type SqlParam =
  | { kind: 'null' }
  | { kind: 'string'; stringValue: string }
  | { kind: 'int'; intValue: bigint }
  | { kind: 'float'; floatValue: number }
  | { kind: 'bool'; boolValue: boolean }
  | { kind: 'binary'; binaryValue: Buffer }
  | { kind: 'uint'; uintValue: bigint }
  | { kind: 'timestamp_ns'; timestampNsValue: bigint };

export interface SqlQueryOptions extends LifecycleOptions {
  /** DataFusion execution memory; defaults to 256 MiB. IPC output and store caches are separate. */
  maxMemoryBytes?: bigint;
}

export interface SqlStats {
  /** Successful planning and execution-stream startup time; excludes actor queue wait. */
  planningDurationNs: bigint;
  /** Cumulative active pull/IPC encoding time; excludes time idle between consumer pulls. */
  executionDurationNs: bigint;
  rows: bigint;
  batches: bigint;
  columnsDecoded: bigint;
  foldReads: bigint;
  rowsFiltered: bigint;
  rowsHidden: bigint;
  batchesSkipped: bigint;
  shadowedOccurrences: bigint;
}

export interface SqlBatch {
  /** Complete Arrow IPC stream containing the result schema and exactly one record batch. */
  ipc: Buffer;
  rows: number;
  stats: SqlStats;
}

export interface Capabilities {
  partFormatWrite: number;
  partFormatReadMax: number;
  writerExclusion: 'os_enforced' | 'embedder_enforced';
  positionedIo: boolean;
  threads: boolean;
  columnar: boolean;
  sql: boolean;
  portableWasm: boolean;
  nativeNode: true;
  napiVersion: 6;
  commandQueueCapacity: number;
  commandQueueCapacityMax: number;
  writeAdmissionLimits: true;
  readAdmissionLimits: true;
  objectCountAdmission: true;
  storeSpaceUsage: true;
  allocatedSpaceUsage: boolean;
  formatMigration: true;
  operationMetrics: true;
  partDistribution: true;
  contentLiveness: true;
  lifecycleEventJournal: true;
  lifecycleEventCapacity: number;
  queryTimings: true;
  sqlExplain: true;
  storageRuntimeOptions: true;
  maxRecordBytesDefault: bigint;
  maxBatchBytesDefault: bigint;
  maxBatchRecordsDefault: number;
  maxIdentifierBytesDefault: number;
  maxStoredFrameBytesDefault: bigint;
  maxDecodedFrameBytesDefault: bigint;
  maxDirectoryEntriesDefault: bigint;
  maxWalFramesDefault: bigint;
  maxFoldBlocksDefault: bigint;
  immutableSnapshots: true;
  lifecycleOperations: true;
  backupRestore: boolean;
  recoveryControls: true;
  healthSnapshots: true;
  schemaDiscovery: true;
  scanExplanation: true;
  scanCancellation: true;
  lifecycleCancellation: true;
  boundedCompaction: true;
  scanReconstructionBudget: true;
  scanReconstructedBytesDefault: bigint;
  scanResolutionBudget: true;
  scanResolutionEntriesDefault: number;
  scanResolutionEntriesMax: number;
  arrowIpc: boolean;
  parameterizedSql: boolean;
  sqlMemoryBytesDefault?: bigint;
  sqlAggregateMemoryBytesDefault?: bigint;
}

export interface OpenOptions {
  /** Accepted operations waiting behind the one executing; defaults to 64. */
  commandQueueCapacity?: number;
  /** Aggregate reservation ceiling across live SQL queries; defaults to 1 GiB. */
  maxConcurrentSqlMemoryBytes?: bigint;
  /** Worst-case complete WAL frame bytes admitted for one record; defaults to 64 MiB. */
  maxRecordBytes?: bigint;
  /** Member frames plus commit marker admitted for one atomic batch; defaults to 256 MiB. */
  maxBatchBytes?: bigint;
  /** Ordered members admitted in one atomic batch; defaults to 4,096. */
  maxBatchRecords?: number;
  /** UTF-8 bytes admitted in an id, attribute name, or content name; defaults to 4 KiB. */
  maxIdentifierBytes?: number;
  /** Stored input admitted for one WAL, part-TOC/section, or fold-block frame; defaults to 512 MiB. */
  maxStoredFrameBytes?: bigint;
  /** Decoded output admitted for one part-TOC/section or fold-block frame; defaults to 512 MiB. */
  maxDecodedFrameBytes?: bigint;
  /** Entries visited in one filesystem directory enumeration; defaults to 100,000. */
  maxDirectoryEntries?: bigint;
  /** Physical frames admitted in one unflushed WAL; defaults to 100,000. */
  maxWalFrames?: bigint;
  /** Content blocks admitted in one fold generation; defaults to 1,000,000. */
  maxFoldBlocks?: bigint;
  /** Raw bytes gathered per compressed content block; defaults to 4 MiB. */
  blockTargetBytes?: bigint;
  /** Decompressed content-block cache budget; defaults to 64 MiB. */
  foldCacheBytes?: bigint;
  /** One decoded-section cache shared by all immutable parts; defaults to 512 MiB. */
  partCacheBytes?: bigint;
  /** Fold segment roll threshold below 4 GiB; defaults to 1 GiB. */
  segmentMaxBytes?: bigint;
  /** Zstd write level from 1 through 22; defaults to 19. */
  compressionLevel?: number;
  /** Compression workers; zero selects available parallelism. */
  compressionThreads?: number;
}

export interface SnapshotOpenOptions {
  maxConcurrentSqlMemoryBytes?: bigint;
  maxStoredFrameBytes?: bigint;
  maxDecodedFrameBytes?: bigint;
  maxDirectoryEntries?: bigint;
  maxWalFrames?: bigint;
  maxFoldBlocks?: bigint;
}

export interface LifecycleOptions {
  /** Submission-inclusive relative deadline. Zero refuses before lifecycle mutation. */
  timeoutMs?: number;
  signal?: AbortSignal;
}

export type AttributeType =
  | 'string'
  | 'int'
  | 'float'
  | 'bool'
  | 'uint'
  | 'binary'
  | 'timestamp_ns'
  | 'null';

export interface StoreSchema {
  attributes: Array<{ name: string; types: AttributeType[] }>;
  contents: string[];
  mayIncludeShadowedFields: boolean;
}

export interface SpaceAmount {
  files: bigint;
  logicalBytes: bigint;
  /** Filesystem blocks in bytes; absent when the platform cannot report sparse allocation. */
  allocatedBytes?: bigint;
}

export interface StoreSpaceUsage {
  live: SpaceAmount;
  /** Files needed only by retained time-travel manifests, not by the current manifest. */
  retainedOnly: SpaceAmount;
  /** Files TurnDB cannot prove are live or retention-pinned; not authorization to delete them. */
  unclassified: SpaceAmount;
  total: SpaceAmount;
  /** Bytes available to the current user on the containing filesystem, when supported. */
  filesystemAvailableBytes?: bigint;
}

export interface OperationMetrics {
  attempts: bigint;
  succeeded: bigint;
  failed: bigint;
  cancelled: bigint;
  totalDurationNs: bigint;
  lastDurationNs: bigint;
  maxDurationNs: bigint;
}

export interface StoreMetrics {
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
  formatMigration: OperationMetrics;
  foldedContent: {
    pieces: bigint;
    dedupHits: bigint;
    logicalBytes: bigint;
    novelBytes: bigint;
  };
}

export interface PartDistribution {
  parts: bigint;
  totalBytes: bigint;
  minBytes: bigint;
  p50Bytes: bigint;
  p95Bytes: bigint;
  maxBytes: bigint;
  totalRows: bigint;
  minRows: bigint;
  p50Rows: bigint;
  p95Rows: bigint;
  maxRows: bigint;
}

export interface FoldBlockSpace {
  blocks: bigint;
  /** Decompressed content bytes represented by these blocks. */
  rawBytes: bigint;
  /** Compressed payload bytes, excluding block framing and filesystem allocation granularity. */
  storedBytes: bigint;
}

export interface ContentLiveness {
  livePieces: bigint;
  liveLogicalBytes: bigint;
  deadLogicalBytes: bigint;
  /** Dead bytes sharing a compressed block with live content; reclaimable only by refold. */
  strandedDeadLogicalBytes: bigint;
  liveBlocks: FoldBlockSpace;
  /** Whole blocks with no live references, eligible for punching or removal by refold. */
  reclaimableBlocks: FoldBlockSpace;
}

export interface LifecycleEvent {
  sequence: bigint;
  operation:
    | 'open_recovery' | 'sync' | 'flush' | 'compaction' | 'backup'
    | 'verification' | 'punch' | 'refold' | 'format_migration';
  outcome: 'succeeded' | 'failed' | 'cancelled';
  /** Stable TurnDB error code; absent on success. */
  errorClass?: TurnDbErrorCode;
  durationNs: bigint;
}

export interface LifecycleEventBatch {
  events: LifecycleEvent[];
  oldestAvailableSequence?: bigint;
  latestSequence: bigint;
  /** Cumulative events evicted from this handle's bounded journal. */
  droppedEvents: bigint;
  /** The requested next sequence was older than the first retained event. */
  gap: boolean;
}

export interface MergeStats {
  inputs: bigint;
  recordsIn: bigint;
  recordsOut: bigint;
  superseded: bigint;
  tombstonesKept: bigint;
  tombstonesDropped: bigint;
  foldBytesTouched: bigint;
}

export interface CompactionBudget {
  /** Maximum number of contiguous input parts; must be at least two. */
  maxInputParts: number;
  /** Maximum physical rows read from input parts. */
  maxInputRows: bigint;
  /** Maximum exact on-disk bytes read from input part files. */
  maxInputBytes: bigint;
}

export interface CompactionPlan {
  /** Zero-based position in the current live part list. */
  startPart: bigint;
  inputParts: bigint;
  inputRows: bigint;
  inputBytes: bigint;
  /** True only when this run covers the entire live part list. */
  dropsTombstones: boolean;
}

export interface CompactionSpaceEstimate {
  plan: CompactionPlan;
  inputSections: bigint;
  inputRawSectionBytes: bigint;
  /** Conservative planning estimate, explicitly not an admission limit or hard upper bound. */
  estimatedStageBytes: bigint;
  estimateIsHardBound: false;
  retainedInputBytesAfterCommit: bigint;
  filesystemAvailableBytes?: bigint;
}

export interface RefoldResult {
  partsIn: bigint;
  partsOut: bigint;
  recordsKept: bigint;
  recordsDropped: bigint;
  tombstonesDropped: bigint;
  piecesKept: bigint;
  piecesDropped: bigint;
  foldBytesBefore: bigint;
  foldBytesAfter: bigint;
  bytesReclaimed: bigint;
  staleGenerationLeft: boolean;
}

export interface RefoldSpaceEstimate {
  sourceFoldLogicalBytes: bigint;
  sourcePartBytes: bigint;
  sourcePartSections: bigint;
  sourcePartRawSectionBytes: bigint;
  retainedOnlyBytesBefore: bigint;
  /** Conservative duplicate-generation estimate; not an admission limit or hard upper bound. */
  estimatedStageBytes: bigint;
  estimateIsHardBound: false;
  filesystemAvailableBytes?: bigint;
}

export interface FormatMigrationStatus {
  targetPartVersion: number;
  liveParts: bigint;
  currentParts: bigint;
  legacyParts: bigint;
  legacyRows: bigint;
  legacyBytes: bigint;
  retainedLegacyParts: bigint;
  retainedLegacyRows: bigint;
  retainedLegacyBytes: bigint;
}

export interface FormatMigrationPlan {
  partIndex: bigint;
  sourcePartVersion: number;
  seqLo: bigint;
  seqHi: bigint;
  inputRows: bigint;
  inputBytes: bigint;
  inputSections: bigint;
  inputRawSectionBytes: bigint;
  estimatedStageBytes: bigint;
  estimateIsHardBound: false;
  retainedInputBytesAfterCommit: bigint;
  filesystemAvailableBytes?: bigint;
}

export interface FormatMigrationStep {
  plan: FormatMigrationPlan;
  outputBytes: bigint;
  remainingLegacyParts: bigint;
  rewrite: MergeStats;
}

export type TurnDbErrorCode =
  | 'INVALID_ARGUMENT'
  | 'BUSY'
  | 'CLOSED'
  | 'CANCELLED'
  | 'RESOURCE_EXHAUSTED'
  | 'UNSUPPORTED'
  | 'CONTENTION'
  | 'NOT_FOUND'
  | 'CORRUPTION'
  | 'IO'
  | 'INTERNAL';

export declare class TurnDbError extends Error {
  readonly code: TurnDbErrorCode;
  readonly cause?: unknown;
}

export declare function capabilities(): Capabilities;
export declare function retainedCommits(path: string): Promise<bigint[]>;
/**
 * Which single-file form a path holds, or `null` for a directory or a file carrying neither
 * magic. Reading does not need this — {@link NativeSnapshot.openFile} dispatches on its own — but
 * tooling that must know whether a file can still be appended to does.
 */
export declare function singleFileKind(path: string): 'pack' | 'container' | null;
export interface CheckpointResult {
  /** Members the container holds after the checkpoint. */
  members: number;
  /** Bytes written into the container by this call. */
  ingestedBytes: bigint;
  /** Members already present byte-for-byte and therefore not rewritten. */
  skippedMembers: number;
  /** The container's committed sequence after this call. */
  commitSeq: bigint;
  /** Bytes now superseded inside the container, reclaimable only by rewriting it. */
  freeBytes: bigint;
}
/**
 * Checkpoint a store directory into a growable single file, creating it or growing one in place.
 * Incremental: immutable members already present at the same length are skipped. The source must
 * be quiescent — `sync()` then `flush()` first.
 */
export declare function checkpointIntoContainer(
  directoryPath: string,
  containerPath: string,
): Promise<CheckpointResult>;
export interface RecoveryOptions extends LifecycleOptions {
  /** Maximum number of newer retained commits that recovery may abandon; defaults to zero. */
  maxRollbackCommits?: bigint;
  maxStoredFrameBytes?: bigint;
  maxDecodedFrameBytes?: bigint;
  maxDirectoryEntries?: bigint;
  maxWalFrames?: bigint;
  maxFoldBlocks?: bigint;
}
export interface RestoreOptions extends LifecycleOptions {
  maxStoredFrameBytes?: bigint;
  maxDecodedFrameBytes?: bigint;
  maxDirectoryEntries?: bigint;
  maxWalFrames?: bigint;
  maxFoldBlocks?: bigint;
}
export declare function recoverManifest(
  path: string,
  options?: RecoveryOptions,
): Promise<{
  commit: bigint;
  rollbackCommits: bigint;
  records: bigint;
  contentValues: bigint;
  parts: bigint;
  partSections: bigint;
  foldSegments: number;
  foldBlocks: bigint;
  foldBytes: bigint;
}>;
export declare function restoreBackup(
  backupPath: string,
  destinationPath: string,
  options?: RestoreOptions,
): Promise<{ files: bigint; bytes: bigint; commit: bigint }>;

export declare class NativeSqlQuery {
  readonly schemaIpc: Buffer;
  /** Pull one batch; null is stable at EOF. Dropping or closing the query cancels remaining work. */
  next(options?: { timeoutMs?: number; signal?: AbortSignal }): Promise<SqlBatch | null>;
  stats(): Promise<SqlStats>;
  close(): Promise<void>;
}

export declare class NativeSnapshot {
  static open(path: string, options?: SnapshotOpenOptions): Promise<NativeSnapshot>;
  /**
   * Open a store held in ONE FILE — a sealed pack or a growable container, told apart by magic
   * rather than extension. Both answer reads identically; there is no writer role to take and no
   * WAL to replay, so this cannot contend with a writer the way a directory open can.
   */
  static openFile(path: string, options?: SnapshotOpenOptions): Promise<NativeSnapshot>;
  static openAt(path: string, commit: bigint, options?: SnapshotOpenOptions): Promise<NativeSnapshot>;
  readonly commit: bigint;
  readonly maxConcurrentSqlMemoryBytes: bigint;
  readonly maxStoredFrameBytes: bigint;
  readonly maxDecodedFrameBytes: bigint;
  readonly maxDirectoryEntries: bigint;
  readonly maxWalFrames: bigint;
  readonly maxFoldBlocks: bigint;
  readonly reservedSqlMemoryBytes: bigint;
  scan(request?: ScanRequest): Promise<ScanPage>;
  explainScan(request?: ScanRequest): Promise<ScanExplanation>;
  querySql(sql: string, params?: SqlParam[], options?: SqlQueryOptions): Promise<NativeSqlQuery>;
  readContent(id: string, name: string): Promise<Buffer | null>;
  schema(): Promise<StoreSchema>;
  close(): Promise<void>;
}

export declare class NativeStore {
  static open(path: string, options?: OpenOptions): Promise<NativeStore>;
  readonly commandQueueCapacity: number;
  readonly maxConcurrentSqlMemoryBytes: bigint;
  readonly reservedSqlMemoryBytes: bigint;
  write(ops: WriteOp[], durable?: boolean): Promise<void>;
  /** Cancellation is observed before entering the WAL fsync boundary. */
  sync(options?: LifecycleOptions): Promise<void>;
  /** Cancellation is observed before manifest publication; staged parts remain unreachable. */
  flush(options?: LifecycleOptions): Promise<boolean>;
  scan(request?: ScanRequest): Promise<ScanPage>;
  explainScan(request?: ScanRequest): Promise<ScanExplanation>;
  /** Publishes earlier writes as an immutable cut before planning; timeout includes actor queue time. */
  querySql(sql: string, params?: SqlParam[], options?: SqlQueryOptions): Promise<NativeSqlQuery>;
  readContent(id: string, name: string): Promise<Buffer | null>;
  snapshot(): Promise<NativeSnapshot>;
  spaceUsage(options?: LifecycleOptions): Promise<StoreSpaceUsage>;
  schema(): Promise<StoreSchema>;
  compact(full?: boolean, options?: LifecycleOptions): Promise<{
    flushed: boolean;
    partsBefore: bigint;
    partsAfter: bigint;
    merge?: MergeStats;
  }>;
  /** Settles prior writes, then merges one contiguous run within exact physical-input limits. */
  compactBounded(budget: CompactionBudget, options?: LifecycleOptions): Promise<{
    flushed: boolean;
    partsBefore: bigint;
    partsAfter: bigint;
    plan?: CompactionPlan;
    outputBytes?: bigint;
    merge?: MergeStats;
  }>;
  /** Settles the actor cut, then returns exact estimate inputs and an explicitly advisory result. */
  estimateCompactionSpace(
    budget: CompactionBudget,
    options?: LifecycleOptions,
  ): Promise<{ flushed: boolean; estimate?: CompactionSpaceEstimate }>;
  formatMigrationStatus(options?: LifecycleOptions): Promise<FormatMigrationStatus>;
  estimateFormatMigrationSpace(options?: LifecycleOptions): Promise<{
    flushed: boolean;
    status: FormatMigrationStatus;
    estimate?: FormatMigrationPlan;
  }>;
  migrateFormatStep(options?: LifecycleOptions): Promise<{
    flushed: boolean;
    step?: FormatMigrationStep;
  }>;
  verify(options?: LifecycleOptions): Promise<{
    manifestLinks: bigint;
    partDigests: bigint;
    undigestedParts: bigint;
    parts: bigint;
    partSections: bigint;
    foldSegments: number;
    foldBlocks: bigint;
    foldBytes: bigint;
    trailingUncommittedBytes: bigint;
  }>;
  /** Settles prior writes; cancellation never publishes the destination. */
  backup(
    path: string,
    options?: LifecycleOptions,
  ): Promise<{ files: bigint; bytes: bigint; commit: bigint }>;
  erase(ids: string[], options?: LifecycleOptions): Promise<{
    requested: bigint;
    tombstoned: bigint;
    absent: bigint;
    refold?: RefoldResult;
  }>;
  punch(options?: LifecycleOptions): Promise<{ blocksExamined: bigint; blocksPunched: bigint }>;
  refold(options?: LifecycleOptions): Promise<RefoldResult>;
  estimateRefoldSpace(
    options?: LifecycleOptions,
  ): Promise<{ flushed: boolean; estimate?: RefoldSpaceEstimate }>;
  health(): Promise<{
    commit: bigint;
    foldGeneration: number;
    parts: bigint;
    partRows: bigint;
    memtableEntries: bigint;
    memtableBytes: bigint;
    walBytes: bigint;
    walFrames: bigint;
    foldDiskBytes: bigint;
    foldSegments: number;
    foldCacheHits: bigint;
    foldCacheMisses: bigint;
    foldCacheBytes: bigint;
    foldCacheBudget: bigint;
    foldBlockTargetBytes: bigint;
    foldSegmentMaxBytes: bigint;
    foldCompressionLevel: number;
    foldCompressionThreads: bigint;
    partCacheBytes: bigint;
    partCacheBudget: bigint;
    maxStoredFrameBytes: bigint;
    maxDecodedFrameBytes: bigint;
    maxDirectoryEntries: bigint;
    maxWalFrames: bigint;
    maxFoldBlocks: bigint;
    dedupWindowEntries: bigint;
    retainedCommits: bigint;
    punchedBlocks: bigint;
  }>;
  metrics(): Promise<StoreMetrics>;
  lifecycleEvents(afterSequence?: bigint, limit?: number): Promise<LifecycleEventBatch>;
  partDistribution(options?: LifecycleOptions): Promise<PartDistribution>;
  contentLiveness(options?: LifecycleOptions): Promise<ContentLiveness>;
  close(durable?: boolean): Promise<void>;
}
