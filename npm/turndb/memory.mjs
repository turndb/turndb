/**
 * The memory host: the portable engine over an in-memory directory, for every runtime without
 * `node:wasi` — a browser Web Worker, a Cloudflare Worker, Bun, Deno — and for Node when the
 * store should never touch a disk.
 *
 * ```js
 * import { MemoryHost } from 'turndb/memory';
 *
 * const host = await MemoryHost.create({ module: await WebAssembly.compileStreaming(fetch(wasmUrl)) });
 * const store = host.open('trace.turndb');
 * store.write([{ kind: 'put', id: 'span/…', contents: [{ name: 'body', bytes }], attrs }]);
 * store.sync();
 * store.flush();
 * store.close();
 * const bytes = host.read('trace.turndb');   // the container, byte-exact, ready to upload
 * ```
 *
 * The same `Store` class as the Node host, over the same `.wasm`. What differs is stated rather
 * than hidden: durability barriers are no-ops over memory (`sync` and `flush` still order the
 * engine's own state, and a store exported after `close` is a settled container), writer
 * exclusion is the embedder's as on every WASI host, and the whole store plus its WAL live in the
 * host's memory until the host is dropped.
 */

import {
  contractProfile,
  openReaderHandle,
  openWriterHandle,
  prefixUpperBound,
  readProfile,
  Store,
  TurndbError,
} from './core.mjs';
import { MemoryFileSystem, WasiExit } from './wasi-memfs.mjs';

export { MemoryFileSystem, prefixUpperBound, Store, TurndbError, WasiExit };

/** Where the memory directory is mounted inside the guest. Callers never see it. */
const GUEST_ROOT = '/store';

async function compileModule(module) {
  const value = await module;
  if (value instanceof WebAssembly.Module) return value;
  if (typeof Response !== 'undefined' && value instanceof Response) {
    return WebAssembly.compileStreaming
      ? WebAssembly.compileStreaming(value)
      : WebAssembly.compile(await value.arrayBuffer());
  }
  if (value instanceof ArrayBuffer || ArrayBuffer.isView(value)) return WebAssembly.compile(value);
  throw new TypeError('MemoryHost.create needs a WebAssembly.Module, a Response, or the wasm bytes');
}

/**
 * One wasm instance over one in-memory directory. Stores opened from it share the instance; the
 * engine keeps its handles apart, and closing one leaves the others open.
 */
export class MemoryHost {
  #fs;
  #instance;
  #runtime;
  #open = 0;

  constructor(fs, instance) {
    this.#fs = fs;
    this.#instance = instance;
    const host = this;
    this.#runtime = {
      instance,
      release() {
        host.#open = Math.max(0, host.#open - 1);
      },
    };
  }

  /**
   * Compile (if needed) and instantiate the engine over a fresh memory directory.
   *
   * @param {{ module: WebAssembly.Module | Response | BufferSource | Promise<unknown>, preopen?: string }} options
   */
  static async create({ module, preopen = GUEST_ROOT } = {}) {
    if (module == null) throw new TypeError('MemoryHost.create needs the engine module');
    const compiled = await compileModule(module);
    const fs = new MemoryFileSystem({ preopen });
    let instance;
    const imports = { wasi_snapshot_preview1: fs.imports(() => instance.exports.memory) };
    instance = await WebAssembly.instantiate(compiled, imports);
    // A reactor exports `_initialize` when it has constructors to run; this engine's build has
    // none today, and calling it when present keeps that a build detail rather than a host one.
    instance.exports._initialize?.();
    return new MemoryHost(fs, instance);
  }

  /** The directory the guest sees: list, read, place and remove files by name. */
  get files() {
    return this.#fs;
  }

  /** Every file the directory holds, sorted. */
  list() {
    return this.#fs.files();
  }

  /** The exact bytes of a file, copied, or `null`. A closed store is a complete container here. */
  read(name) {
    return this.#fs.read(name);
  }

  /** Place bytes at `name` — a container to open read-only, or a fixture. */
  write(name, bytes) {
    this.#fs.write(name, bytes);
  }

  /** Remove a file; absent is not an error. */
  remove(name) {
    return this.#fs.remove(name);
  }

  /** Bytes the guest wrote to stdout and stderr; the engine writes nothing there in normal use. */
  output() {
    return this.#fs.output();
  }

  /** How many handles are open on this host. */
  get openHandles() {
    return this.#open;
  }

  /**
   * Open (or create) a writer over `name` inside the memory directory. Options are the Node
   * host's {@link OpenOptions}; the zstd level defaults to 3 for the same single-threaded reason.
   */
  open(name, opts = {}) {
    const store = openWriterHandle(this.#runtime, this.#guestPath(name), opts, `memory:${name}`);
    this.#open += 1;
    return store;
  }

  /** Open a container the directory holds, read-only. */
  openFile(name, opts = {}) {
    if (this.#fs.read(name) == null) {
      throw new TurndbError(`opening memory:${name}: no such TurnDB container`, 'NOT_FOUND');
    }
    const store = openReaderHandle(this.#runtime, this.#guestPath(name), opts, `memory:${name}`);
    this.#open += 1;
    return store;
  }

  /** Operations, limits, and explicit absences reachable through this host. */
  capabilities() {
    return contractProfile(this.#runtime, { host: 'memory' });
  }

  /** Mechanisms and format facts compiled into the engine, independent of the host. */
  compiledCapabilities() {
    return readProfile(this.#runtime, 'tdb_capabilities');
  }

  #guestPath(name) {
    if (typeof name !== 'string' || name === '' || name.startsWith('/') || name.includes('..')) {
      throw new TurndbError(`store name must be a relative path inside the memory directory, got ${JSON.stringify(name)}`, 'INVALID_ARGUMENT');
    }
    return `${this.#fs.preopen}/${name}`;
  }
}

export default { MemoryHost, MemoryFileSystem, Store, TurndbError, prefixUpperBound };
