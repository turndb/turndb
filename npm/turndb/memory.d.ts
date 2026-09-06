/**
 * The memory host: the portable engine over an in-memory directory, for runtimes without
 * `node:wasi` (a browser Web Worker, a Cloudflare Worker, Bun, Deno) and for Node when a store
 * must never touch a disk. See `memory.mjs` for the durability statement this host makes.
 */

import type { Capabilities, CompiledCapabilities, OpenFileOptions, OpenOptions, Store, TurndbError } from './index.js';

export { prefixUpperBound, Store, TurndbError } from './index.js';

/** The in-memory directory the guest sees, addressable by the host. */
export declare class MemoryFileSystem {
  constructor(options?: { preopen?: string });
  /** Every file path under the root, sorted, relative to the preopen. */
  files(): string[];
  /** The exact bytes of a file, copied, or `null` when absent. */
  read(path: string): Uint8Array | null;
  /** Place a file; parents are created; an existing file is replaced. */
  write(path: string, bytes: Uint8Array | ArrayBuffer): void;
  /** Remove a file or empty directory; absent is not an error. */
  remove(path: string): boolean;
  /** Bytes the guest wrote to stdout and stderr, decoded. */
  output(): { stdout: string; stderr: string };
  /** The `wasi_snapshot_preview1` import namespace bound to a memory getter. */
  imports(getMemory: () => WebAssembly.Memory): Record<string, (...args: never[]) => number>;
  readonly preopen: string;
  /** How many `fd_sync` calls the guest made; durability barriers are no-ops over memory. */
  fdSyncCalls: number;
}

export declare class WasiExit extends Error {
  readonly code: number;
}

export interface MemoryHostOptions {
  /** The engine: a compiled module, a `Response` to compile from, or the wasm bytes. */
  module: WebAssembly.Module | Response | BufferSource | Promise<WebAssembly.Module | Response | BufferSource>;
  /** Where the memory directory is mounted inside the guest. Default `/store`. */
  preopen?: string;
}

/** One wasm instance over one in-memory directory; stores opened from it share the instance. */
export declare class MemoryHost {
  static create(options: MemoryHostOptions): Promise<MemoryHost>;
  readonly files: MemoryFileSystem;
  readonly openHandles: number;
  list(): string[];
  read(name: string): Uint8Array | null;
  write(name: string, bytes: Uint8Array | ArrayBuffer): void;
  remove(name: string): boolean;
  output(): { stdout: string; stderr: string };
  /** Open (or create) a writer over `name` inside the memory directory. */
  open(name: string, opts?: OpenOptions): Store;
  /** Open a container the directory holds, read-only. */
  openFile(name: string, opts?: OpenFileOptions): Store;
  capabilities(): Capabilities & { host: 'memory' };
  compiledCapabilities(): CompiledCapabilities;
}

declare const _default: {
  MemoryHost: typeof MemoryHost;
  MemoryFileSystem: typeof MemoryFileSystem;
  Store: typeof Store;
  TurndbError: typeof TurndbError;
};
export default _default;
