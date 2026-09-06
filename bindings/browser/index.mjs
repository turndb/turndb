const RANGE_MISS = /TURNDB_RANGE:(\d+):(\d+)/;

export class TurnDbError extends Error {
  constructor(code, message, cause) {
    super(message, { cause });
    this.name = 'TurnDbError';
    this.code = code;
  }
}

function normalize(error, fallback = 'IO') {
  if (error instanceof TurnDbError) return error;
  const message = error?.message ?? String(error);
  return new TurnDbError(error?.code ?? fallback, message, error);
}

function missing(error) {
  const match = RANGE_MISS.exec(String(error));
  return match ? { offset: BigInt(match[1]), length: Number(match[2]) } : null;
}

function abort(signal) {
  if (signal?.aborted) throw signal.reason ?? new DOMException('operation aborted', 'AbortError');
}

export class BufferReadAt {
  constructor(bytes, label = 'buffer') {
    this.bytes = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    this.length = BigInt(this.bytes.byteLength);
    this.label = label;
    this.stats = { rangeRequests: 0, networkBytes: 0, cacheHits: 0, cacheMisses: 0 };
  }

  readSync(offset, length) {
    const at = Number(offset);
    if (!Number.isSafeInteger(at) || at < 0 || length < 0 || at + length > this.bytes.length) {
      throw new RangeError(`read of ${length} bytes at ${offset} exceeds ${this.length}`);
    }
    this.stats.cacheHits++;
    return this.bytes.slice(at, at + length);
  }

  async ensure() {}

  async ensureRanges() {}
}

/** Merge `[{ offset, length }]` ranges whose gaps are at most `gap` bytes, ascending. */
function mergeRanges(ranges, gap) {
  const sorted = ranges
    .map((range) => ({ start: BigInt(range.offset), end: BigInt(range.offset) + BigInt(range.length) }))
    .filter((range) => range.end > range.start)
    .sort((a, b) => (a.start < b.start ? -1 : a.start > b.start ? 1 : 0));
  const out = [];
  for (const range of sorted) {
    const last = out[out.length - 1];
    if (last && range.start <= last.end + BigInt(gap)) {
      if (range.end > last.end) last.end = range.end;
    } else {
      out.push({ ...range });
    }
  }
  return out;
}

/**
 * A block-cached source over any range fetcher. Exported so a host can subclass it with its own
 * `fetchRange` — an object-store binding, a message port — and get the block LRU, the transient
 * footprints, and the pinned retries the HTTP and Blob transports use.
 */
export class BlockReadAt {
  constructor(length, { blockSize = 64 * 1024, maxBlocks = 64, label = 'range source' } = {}) {
    if (!Number.isSafeInteger(blockSize) || blockSize < 4096) throw new RangeError('blockSize must be at least 4096');
    if (!Number.isSafeInteger(maxBlocks) || maxBlocks < 2) throw new RangeError('maxBlocks must be at least 2');
    this.length = BigInt(length);
    this.blockSize = blockSize;
    this.maxBlocks = maxBlocks;
    this.label = label;
    this.blocks = new Map();
    this.transient = [];
    this.stats = { rangeRequests: 0, networkBytes: 0, cacheHits: 0, cacheMisses: 0 };
  }

  releaseTransient() {
    this.transient = [];
  }

  /** Hold an already-ensured range transient until `releaseTransient()`, whatever the LRU does. */
  pin(offset, length) {
    if (length === 0) return;
    const end = offset + BigInt(length);
    if (this.transient.some((held) => held.start <= offset && held.end >= end)) return;
    const bytes = this.readSync(offset, length);
    if (bytes) this.transient.push({ start: offset, end, bytes });
  }

  readSync(offset, length) {
    const end = offset + BigInt(length);
    if (offset < 0n || end > this.length) {
      throw new RangeError(`read of ${length} bytes at ${offset} exceeds ${this.length}`);
    }
    if (length === 0) return new Uint8Array();
    const admitted = this.transient.find((range) => range.start <= offset && range.end >= end);
    if (admitted) {
      this.stats.cacheHits++;
      const within = Number(offset - admitted.start);
      return admitted.bytes.slice(within, within + length);
    }
    const first = offset / BigInt(this.blockSize);
    const last = (end - 1n) / BigInt(this.blockSize);
    const needed = [];
    for (let block = first; block <= last; block++) {
      const key = block.toString();
      const bytes = this.blocks.get(key);
      if (!bytes) {
        this.stats.cacheMisses++;
        return undefined;
      }
      needed.push([block, key, bytes]);
    }
    const out = new Uint8Array(length);
    let written = 0;
    let at = offset;
    for (const [block, key, bytes] of needed) {
      this.blocks.delete(key);
      this.blocks.set(key, bytes);
      const blockStart = block * BigInt(this.blockSize);
      const within = Number(at - blockStart);
      const take = Math.min(bytes.length - within, length - written);
      out.set(bytes.subarray(within, within + take), written);
      written += take;
      at += BigInt(take);
    }
    this.stats.cacheHits++;
    return out;
  }

  async ensure(offset, length, signal) {
    abort(signal);
    const end = offset + BigInt(length);
    if (length === 0) return;
    const first = offset / BigInt(this.blockSize);
    const last = (end - 1n) / BigInt(this.blockSize);
    if (last - first + 1n > BigInt(this.maxBlocks)) {
      // The core admits one atomic read before allocation. Preserve that exact request across
      // retries separately from the steady-state LRU; otherwise a request larger than the cache
      // would evict its own first block before the synchronous callback could consume it.
      const bytes = new Uint8Array(length);
      let written = 0;
      for (let block = first; block <= last; block++) {
        abort(signal);
        const start = block * BigInt(this.blockSize);
        const stop = start + BigInt(this.blockSize) < this.length
          ? start + BigInt(this.blockSize)
          : this.length;
        const fetched = await this.fetchRange(start, stop, signal);
        const copyStart = offset > start ? offset : start;
        const copyEnd = end < stop ? end : stop;
        const from = Number(copyStart - start);
        const take = Number(copyEnd - copyStart);
        bytes.set(fetched.subarray(from, from + take), written);
        written += take;
      }
      this.transient.push({ start: offset, end, bytes });
      return;
    }
    for (let block = first; block <= last; block++) {
      abort(signal);
      const key = block.toString();
      if (this.blocks.has(key)) continue;
      const start = block * BigInt(this.blockSize);
      const blockEnd = start + BigInt(this.blockSize);
      const stop = blockEnd < this.length ? blockEnd : this.length;
      const bytes = await this.fetchRange(start, stop, signal);
      if (bytes.byteLength !== Number(stop - start)) {
        throw new Error(`${this.label} returned ${bytes.byteLength} bytes for [${start}, ${stop})`);
      }
      this.blocks.set(key, bytes);
      while (this.blocks.size > this.maxBlocks) this.blocks.delete(this.blocks.keys().next().value);
    }
  }

  /**
   * Fill a declared footprint — the `[{ offset, length }]` ranges a verification unit will read —
   * ahead of the unit, as transient exact ranges rather than LRU blocks. A unit's footprint can
   * exceed the block cache many times over; served through the LRU it would evict its own early
   * blocks and the unit would restart on every miss. Held transient, the unit runs in one pass
   * and `releaseTransient()` returns the memory when it is done. Ranges separated by at most one
   * block are fetched together so a footprint of many small pieces costs few requests.
   */
  async ensureRanges(ranges, signal) {
    for (const range of mergeRanges(ranges, this.blockSize)) {
      abort(signal);
      const covered = this.transient.some((held) => held.start <= range.start && held.end >= range.end);
      if (covered) continue;
      const bytes = await this.fetchRange(range.start, range.end, signal);
      if (bytes.byteLength !== Number(range.end - range.start)) {
        throw new Error(`${this.label} returned ${bytes.byteLength} bytes for [${range.start}, ${range.end})`);
      }
      this.transient.push({ start: range.start, end: range.end, bytes });
    }
  }
}

export class BlobReadAt extends BlockReadAt {
  constructor(blob, options = {}) {
    super(blob.size, { ...options, label: options.label ?? blob.name ?? 'Blob' });
    this.blob = blob;
  }

  async fetchRange(start, end, signal) {
    abort(signal);
    const bytes = new Uint8Array(await this.blob.slice(Number(start), Number(end)).arrayBuffer());
    abort(signal);
    return bytes;
  }
}

export class HttpRangeReadAt extends BlockReadAt {
  constructor(url, length, options = {}) {
    super(length, { ...options, label: options.label ?? url });
    this.url = url;
    this.fetchImpl = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  static async open(url, options = {}) {
    try {
      const fetchImpl = options.fetch ?? globalThis.fetch.bind(globalThis);
      const response = await fetchImpl(url, {
        headers: { Range: 'bytes=0-0' },
        signal: options.signal,
      });
      if (response.status !== 206) {
        throw new Error(`HTTP source must honor Range with 206; ${url} returned ${response.status}`);
      }
      const contentRange = response.headers.get('content-range');
      const match = /^bytes 0-0\/(\d+)$/.exec(contentRange ?? '');
      if (!match) throw new Error(`HTTP source returned invalid Content-Range ${JSON.stringify(contentRange)}`);
      const source = new HttpRangeReadAt(url, BigInt(match[1]), options);
      const first = new Uint8Array(await response.arrayBuffer());
      if (first.byteLength !== 1) {
        throw new Error(`HTTP range probe returned ${first.byteLength} bytes, expected 1`);
      }
      source.stats.rangeRequests = 1;
      source.stats.networkBytes = first.byteLength;
      // Seed only when it happens to be a complete first block; ordinarily the block fetch below
      // replaces this one-byte probe, keeping cache representation simple and exact.
      if (first.byteLength === Math.min(source.blockSize, Number(source.length))) {
        source.blocks.set('0', first);
      }
      return source;
    } catch (error) {
      throw normalize(error);
    }
  }

  async fetchRange(start, end, signal) {
    const response = await this.fetchImpl(this.url, {
      headers: { Range: `bytes=${start}-${end - 1n}` },
      signal,
    });
    if (response.status !== 206) {
      throw new Error(`HTTP range [${start}, ${end}) returned ${response.status}, expected 206`);
    }
    const want = `bytes ${start}-${end - 1n}/${this.length}`;
    if (response.headers.get('content-range') !== want) {
      throw new Error(`HTTP range returned ${JSON.stringify(response.headers.get('content-range'))}, expected ${want}`);
    }
    const bytes = new Uint8Array(await response.arrayBuffer());
    this.stats.rangeRequests++;
    this.stats.networkBytes += bytes.byteLength;
    return bytes;
  }
}

/**
 * Run a synchronous wasm operation over `source`, filling each range the core reports missing
 * and retrying until it completes. Transient ranges are released on completion and on failure.
 */
async function retrying(source, operation, signal) {
  while (true) {
    abort(signal);
    try {
      const result = operation();
      source.releaseTransient?.();
      return result;
    } catch (error) {
      const range = missing(error);
      if (!range) {
        source.releaseTransient?.();
        throw normalize(error, 'INTERNAL');
      }
      try {
        await source.ensure(range.offset, range.length, signal);
        // Pin what was just fetched until this operation completes. The LRU alone is not enough:
        // an operation whose working set exceeds the cache would evict its own first block while
        // fetching its last and restart forever. Pinned, every retry keeps what earlier retries
        // fetched, so each retry makes progress whatever the cache size; the pins go with the
        // transient set when the operation completes or fails.
        source.pin?.(range.offset, range.length);
      } catch (ensureError) {
        source.releaseTransient?.();
        throw normalize(ensureError);
      }
    }
  }
}

/**
 * Verify a container over any source: the same evidence the native `verify` produces, in
 * declared units. Each unit's byte footprint is fetched ahead of it, so a unit runs in one pass
 * whatever the block cache holds; `onProgress` sees `{ done, total, unit }` after each unit.
 * The result is `{ report, units }`, where `report` composes only once every unit ran.
 */
export async function verifySource(wasm, source, { signal, onProgress } = {}) {
  let from = 0;
  for (;;) {
    const step = await verifyUnits(wasm, source, { from, signal, onProgress });
    if (step.report) return { report: step.report, units: step.total };
    from = step.next;
  }
}

/**
 * The resumable form of {@link verifySource}: run the units from `from` until every unit has run,
 * `limit` units have run, or `deadlineMs` of wall clock has passed, and return where to resume.
 *
 * The plan is a pure function of the container bytes, so a fresh verifier over the same bytes
 * plans the same units in the same order; a host whose work is cut into bounded steps (an edge
 * worker with a CPU budget per invocation) opens one per step and continues from `next`. The
 * result carries `report` only once the last unit has run in this call; before that it is
 * `null`, because a partial run is scoped evidence and never a whole-store result.
 */
export async function verifyUnits(wasm, source, { from = 0, limit = Infinity, deadlineMs = Infinity, signal, onProgress } = {}) {
  const read = source.readSync.bind(source);
  const verifier = await retrying(
    source,
    () => wasm.BrowserVerifier.open(read, source.length, source.label),
    signal,
  );
  try {
    const units = verifier.units();
    const started = Date.now();
    let index = from;
    while (index < units.length) {
      const footprint = await retrying(source, () => verifier.footprint(index), signal);
      try {
        await source.ensureRanges?.(footprint, signal);
      } catch (error) {
        source.releaseTransient?.();
        throw normalize(error);
      }
      // `retrying` releases the transient footprint when the unit completes or fails.
      await retrying(source, () => verifier.run(index), signal);
      index += 1;
      onProgress?.({ done: index, total: units.length, unit: units[index - 1] });
      if (index - from >= limit || Date.now() - started >= deadlineMs) break;
    }
    // Units before `from` are assumed to have run in an earlier call over the same bytes; the
    // report is composed only when this call ran the last unit, and the verifier refuses to
    // report before every unit it planned has run — so a resumed run must restart at zero if
    // it needs the composed report from one verifier. Hosts that step through the plan keep
    // the composed report from the call that finished it.
    const finished = index >= units.length;
    let report = null;
    if (finished) {
      for (let earlier = 0; earlier < from; earlier++) {
        await retrying(source, () => verifier.run(earlier), signal);
      }
      report = verifier.report();
    }
    return { next: index, total: units.length, report };
  } finally {
    verifier.close();
  }
}

export class BrowserDatabase {
  constructor(wasm, source, handle) {
    this.wasm = wasm;
    this.source = source;
    this.handle = handle;
  }

  static async open(wasm, source, { signal } = {}) {
    const read = source.readSync.bind(source);
    const handle = await retrying(
      source,
      () => wasm.BrowserStore.open(read, source.length, source.label),
      signal,
    );
    return new BrowserDatabase(wasm, source, handle);
  }

  /** {@link verifySource} over this database's source, from a fresh verifier. */
  static verify(wasm, source, options) {
    return verifySource(wasm, source, options);
  }

  static openBuffer(wasm, bytes, options) {
    return BrowserDatabase.open(wasm, new BufferReadAt(bytes, options?.label), options);
  }

  static openBlob(wasm, blob, options) {
    return BrowserDatabase.open(wasm, new BlobReadAt(blob, options), options);
  }

  static async openUrl(wasm, url, options = {}) {
    try {
      const source = await HttpRangeReadAt.open(url, options);
      return BrowserDatabase.open(wasm, source, options);
    } catch (error) {
      throw normalize(error);
    }
  }

  retry(operation, signal) {
    return retrying(this.source, operation, signal);
  }

  scan(request, { signal } = {}) {
    return this.retry(() => this.handle.scan(request), signal);
  }

  explainScan(request, { signal } = {}) {
    return this.retry(() => this.handle.explainScan(request), signal);
  }

  schema({ signal } = {}) {
    return this.retry(() => this.handle.schema(), signal);
  }

  readContent(id, name, { signal } = {}) {
    return this.retry(() => this.handle.readContent(id, name), signal);
  }

  capabilities() {
    return this.wasm.BrowserStore.capabilities();
  }

  fetchStats() {
    return { ...this.source.stats, cachedBlocks: this.source.blocks?.size ?? 1 };
  }

  close() {
    if (!this.handle) return;
    this.source.releaseTransient?.();
    this.handle.close();
    this.handle = null;
  }
}
