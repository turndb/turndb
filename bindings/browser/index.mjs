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
}

class BlockReadAt {
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

export class BrowserDatabase {
  constructor(wasm, source, handle) {
    this.wasm = wasm;
    this.source = source;
    this.handle = handle;
  }

  static async open(wasm, source, { signal } = {}) {
    const read = source.readSync.bind(source);
    while (true) {
      abort(signal);
      try {
        const handle = wasm.BrowserStore.open(read, source.length, source.label);
        source.releaseTransient?.();
        return new BrowserDatabase(wasm, source, handle);
      } catch (error) {
        const range = missing(error);
        if (!range) {
          source.releaseTransient?.();
          throw normalize(error, 'INTERNAL');
        }
        try {
          await source.ensure(range.offset, range.length, signal);
        } catch (ensureError) {
          source.releaseTransient?.();
          throw normalize(ensureError);
        }
      }
    }
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

  async retry(operation, signal) {
    while (true) {
      abort(signal);
      try {
        const result = operation();
        this.source.releaseTransient?.();
        return result;
      } catch (error) {
        const range = missing(error);
        if (!range) {
          this.source.releaseTransient?.();
          throw normalize(error, 'INTERNAL');
        }
        try {
          await this.source.ensure(range.offset, range.length, signal);
        } catch (ensureError) {
          this.source.releaseTransient?.();
          throw normalize(ensureError);
        }
      }
    }
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
