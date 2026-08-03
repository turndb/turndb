/// <reference types="node" />

export type Attr =
  | { name: string; kind: 'string'; stringValue: string }
  | { name: string; kind: 'int'; intValue: bigint }
  | { name: string; kind: 'float'; floatValue: number }
  | { name: string; kind: 'bool'; boolValue: boolean };

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
  };
}

export type SqlParam =
  | { kind: 'null' }
  | { kind: 'string'; stringValue: string }
  | { kind: 'int'; intValue: bigint }
  | { kind: 'float'; floatValue: number }
  | { kind: 'bool'; boolValue: boolean }
  | { kind: 'binary'; binaryValue: Buffer };

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
  immutableSnapshots: true;
  lifecycleOperations: true;
  healthSnapshots: true;
  schemaDiscovery: true;
  scanCancellation: true;
  scanReconstructionBudget: true;
  scanReconstructedBytesDefault: bigint;
  arrowIpc: boolean;
  parameterizedSql: boolean;
  sqlMemoryBytesDefault?: bigint;
}

export interface OpenOptions {
  /** Accepted operations waiting behind the one executing; defaults to 64. */
  commandQueueCapacity?: number;
}

export type AttributeType = 'string' | 'int' | 'float' | 'bool';

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

export declare class NativeSqlQuery {
  readonly schemaIpc: Buffer;
  /** Pull one batch; null is stable at EOF. Dropping or closing the query cancels remaining work. */
  next(options?: { timeoutMs?: number; signal?: AbortSignal }): Promise<SqlBatch | null>;
  stats(): Promise<SqlStats>;
  close(): Promise<void>;
}

export declare class NativeSnapshot {
  static open(path: string): Promise<NativeSnapshot>;
  static openAt(path: string, commit: bigint): Promise<NativeSnapshot>;
  readonly commit: bigint;
  scan(request?: ScanRequest): Promise<ScanPage>;
  querySql(sql: string, params?: SqlParam[], options?: SqlQueryOptions): Promise<NativeSqlQuery>;
  readContent(id: string, name: string): Promise<Buffer | null>;
  schema(): Promise<StoreSchema>;
  close(): Promise<void>;
}

export declare class NativeStore {
  static open(path: string, options?: OpenOptions): Promise<NativeStore>;
  readonly commandQueueCapacity: number;
  write(ops: WriteOp[], durable?: boolean): Promise<void>;
  sync(): Promise<void>;
  flush(): Promise<boolean>;
  scan(request?: ScanRequest): Promise<ScanPage>;
  /** Publishes earlier writes as an immutable cut before planning the query. */
  querySql(sql: string, params?: SqlParam[], options?: SqlQueryOptions): Promise<NativeSqlQuery>;
  readContent(id: string, name: string): Promise<Buffer | null>;
  snapshot(): Promise<NativeSnapshot>;
  schema(): Promise<StoreSchema>;
  compact(full?: boolean): Promise<{
    flushed: boolean;
    partsBefore: bigint;
    partsAfter: bigint;
    merge?: MergeStats;
  }>;
  verify(): Promise<{
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
  erase(ids: string[]): Promise<{
    requested: bigint;
    tombstoned: bigint;
    absent: bigint;
    refold?: RefoldResult;
  }>;
  punch(): Promise<{ blocksExamined: bigint; blocksPunched: bigint }>;
  refold(): Promise<RefoldResult>;
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
