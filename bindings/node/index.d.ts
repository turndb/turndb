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
  attrs?: string[];
  contents?: Array<{ name: string; mode: 'metadata' | 'bytes' }>;
  predicates?: Predicate[];
}

export interface ProjectedContent {
  name: string;
  present: boolean;
  len?: bigint;
  pieces?: number;
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
  };
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
  immutableSnapshots: true;
  lifecycleOperations: true;
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

export declare class NativeSnapshot {
  static open(path: string): Promise<NativeSnapshot>;
  static openAt(path: string, commit: bigint): Promise<NativeSnapshot>;
  readonly commit: bigint;
  scan(request?: ScanRequest): Promise<ScanPage>;
  readContent(id: string, name: string): Promise<Buffer | null>;
  close(): Promise<void>;
}

export declare class NativeStore {
  static open(path: string): Promise<NativeStore>;
  write(ops: WriteOp[], durable?: boolean): Promise<void>;
  sync(): Promise<void>;
  flush(): Promise<boolean>;
  scan(request?: ScanRequest): Promise<ScanPage>;
  readContent(id: string, name: string): Promise<Buffer | null>;
  snapshot(): Promise<NativeSnapshot>;
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
  close(durable?: boolean): Promise<void>;
}
