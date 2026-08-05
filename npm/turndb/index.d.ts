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

export interface Capabilities {
  part_format_write: number;
  part_format_read_max: number;
  writer_exclusion: 'os_enforced' | 'embedder_enforced';
  physical_erasure: 'punch_or_refold' | 'refold_only';
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
  lifecycle_event_capacity: number;
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

/** Every engine-reported failure, carrying the engine's own message. */
export declare class TurndbError extends Error {
  readonly name: 'TurndbError';
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
  /** Tombstone a record. Not durable until {@link Store.sync}. */
  delete(id: string): void;
  /** Make everything written so far durable. **The ACK point.** */
  sync(): void;
  /** Seal the memtable into an immutable part, making writes visible to other readers. */
  flush(): void;
  /**
   * Total merge when the live part list reaches the engine's threshold. Returns whether a merge ran.
   *
   * The stall is the caller's: wall time is linear in the store's on-disk content (~5s/GB at
   * level 19, wasm — synthetic stores up to 1.9 GB, a single workstation; level 3 unmeasured), on the
   * calling thread. Never fires on its own — schedule it when a
   * multi-second pause is acceptable, or bound the pause with {@link Store.maybeCompact}. Only
   * total merges settle deletes.
   */
  autoCompact(): boolean;
  /**
   * Bounded compaction: if at least `trigger` parts are live (default 8), merge the oldest `run`
   * of them (default 4). Returns whether a merge ran.
   *
   * The latency-budget dial: the stall is capped by the merged run instead of the whole store.
   * Bounded merges never settle deletes — run {@link Store.autoCompact} occasionally if the store
   * sees deletions.
   */
  maybeCompact(opts?: { trigger?: number; run?: number }): boolean;
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
   * The native binding's `timeoutMs`/`signal` are deliberately absent: this build is
   * single-threaded, so there is nothing to interrupt a scan from.
   */
  scan(request?: ScanRequest): ScanPage;
  stats(): Stats;
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
 * Open (or create) a store at `dir`.
 *
 * **At most one open writer per store directory across every process.** This package is always the
 * `wasm32-wasip1` build and WASI has no advisory locking, so the engine cannot enforce
 * cross-process exclusion. Two writers corrupt the store, and detection is not guaranteed.
 *
 * Within one process, sequential opens — including opens of different directories — reuse one WASI
 * instance. The directory capability is switched between handles without widening the sandbox to a
 * common ancestor. Consequently only one `Store` may be open in a process at a time; use separate
 * processes when multiple stores must be held open concurrently.
 */
export declare function open(dir: string, opts?: OpenOptions): Promise<Store>;

/** Guarantees of the compiled portable core, independent of the host OS running WASI. */
export declare function capabilities(): Promise<Capabilities>;

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
