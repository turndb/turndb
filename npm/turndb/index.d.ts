/**
 * turndb — a content-addressed columnar store for AI traces.
 *
 * `put` is not durable; `sync()` is the ACK point. `flush()` is separate again: it seals writes
 * into the columnar plane for OTHER readers. This handle sees its own unflushed writes without
 * either.
 */

/** An attribute value. Pass `{i}`/`{f}` to force int-vs-float when it matters. */
export type AttrValue = string | number | boolean | bigint | { i: number } | { f: number };

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
  /** zstd level. Default 19. Write-side only — a reader never needs to know it. */
  level?: number;
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
  attrs: Array<[string, unknown]>;
}

export interface Stats {
  records: number;
  parts: number;
}

/** Every engine-reported failure, carrying the engine's own message. */
export declare class TurndbError extends Error {
  readonly name: 'TurndbError';
}

/**
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
  /** Merge parts if the threshold is reached. Returns whether a merge ran. */
  autoCompact(): boolean;
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
  stats(): Stats;
  /** Release the writer lock. Does NOT sync — call {@link Store.sync} first. */
  close(): void;
}

/** Open (or create) a store at `dir`. One writer per directory, per process. */
export declare function open(dir: string, opts?: OpenOptions): Promise<Store>;

/**
 * The first id that cannot start with `prefix`, or `null` when the range is unbounded above.
 *
 * Exported because its boundary behaviour is part of the contract: it carries by code POINT, so a
 * prefix ending at U+FFFF or inside an astral pair yields a valid bound rather than an inverted
 * range or an unpaired surrogate. Compare bounds as UTF-8 bytes — ids sort by bytes, and JS `<`
 * compares UTF-16 code units, which disagree for astral characters.
 */
export declare function prefixUpperBound(prefix: string): string | null;

declare const _default: { open: typeof open; Store: typeof Store; TurndbError: typeof TurndbError };
export default _default;
