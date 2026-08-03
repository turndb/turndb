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
    examined: number;
    returned: number;
    shadowedAttrOccurrences: number;
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

export interface SqlQueryOptions {
  /** DataFusion execution memory; defaults to 256 MiB. IPC output and store caches are separate. */
  maxMemoryBytes?: bigint;
}

export interface SqlStats {
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
  physicalErasure: 'punch_or_refold' | 'refold_only';
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
  maxRecordBytesDefault: bigint;
  maxBatchBytesDefault: bigint;
  maxBatchRecordsDefault: number;
  maxIdentifierBytesDefault: number;
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
}

export interface SnapshotOpenOptions {
  maxConcurrentSqlMemoryBytes?: bigint;
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
export declare function recoverManifest(
  path: string,
  options?: { maxRollbackCommits?: bigint },
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
  options?: LifecycleOptions,
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
  static openAt(path: string, commit: bigint, options?: SnapshotOpenOptions): Promise<NativeSnapshot>;
  readonly commit: bigint;
  readonly maxConcurrentSqlMemoryBytes: bigint;
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
  sync(): Promise<void>;
  flush(): Promise<boolean>;
  scan(request?: ScanRequest): Promise<ScanPage>;
  explainScan(request?: ScanRequest): Promise<ScanExplanation>;
  /** Publishes earlier writes as an immutable cut before planning the query. */
  querySql(sql: string, params?: SqlParam[], options?: SqlQueryOptions): Promise<NativeSqlQuery>;
  readContent(id: string, name: string): Promise<Buffer | null>;
  snapshot(): Promise<NativeSnapshot>;
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
  health(): Promise<{
    commit: bigint;
    foldGeneration: number;
    parts: bigint;
    partRows: bigint;
    memtableEntries: bigint;
    memtableBytes: bigint;
    walBytes: bigint;
    foldDiskBytes: bigint;
    foldSegments: number;
    foldCacheHits: bigint;
    foldCacheMisses: bigint;
    partCacheBytes: bigint;
    partCacheBudget: bigint;
    dedupWindowEntries: bigint;
    retainedCommits: bigint;
    punchedBlocks: bigint;
  }>;
  close(durable?: boolean): Promise<void>;
}
