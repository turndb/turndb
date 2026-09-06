/**
 * turndb — a content-addressed columnar store for AI traces.
 *
 * The engine is Rust compiled to `wasm32-wasip1`; this file is the thin layer that moves bytes
 * across the boundary and turns status codes back into exceptions. There is no native addon, no
 * prebuild matrix and no postinstall — one `.wasm` runs everywhere Node does.
 *
 * ## What this binding is for
 *
 * Writing traces and reading them back by id or id-range. It deliberately exposes NO SQL: the
 * query engine would dominate the artifact, and the two things an application actually does — a
 * point lookup and a page scan — are already served by the id order. Analytics run through the
 * `turndb` CLI against the same file, which needs no daemon and no second copy of the data.
 *
 * ## Durability, in one sentence
 *
 * `put` is not durable; `sync()` is the ACK point. `flush()` is a separate thing again — it publishes
 * writes into the columnar plane so OTHER readers can see them. This handle sees its own unflushed
 * writes without either.
 *
 * ## Single writer
 *
 * **This package is always the `wasm32-wasip1` build** — the host OS does not switch it onto the
 * native engine — and WASI has no advisory locking, so the engine **cannot** enforce exclusion.
 * The native build's OS-enforced container lock is not in play here, on any host.
 *
 * The host layer permits only one live `Store` in a process, but that is not cross-process
 * exclusion. The obligation is still the embedder's: **at most one open writer per store file
 * across every process.** What a violation does is stated once, with the measurement behind it, in
 * this package's README under "Cross-process exclusion is yours to provide" — briefly: an
 * acknowledged write can be lost silently from a store that still reads and verifies clean, so an
 * integrity check is not the instrument for it.
 */

import { WASI } from 'node:wasi';
import { mkdir, readFile, stat } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { basename, dirname, join, resolve } from 'node:path';

const WASM_PATH = join(dirname(fileURLToPath(import.meta.url)), 'turndb.wasm');

/** Where the store's parent directory is mounted inside the sandbox. Callers never see this. */
const GUEST_ROOT = '/store';
/** Preview1 reserves 0..2 for stdio, so our only preopen is descriptor 3. */
const GUEST_ROOT_FD = 3;

import {
  contractProfile,
  openLimit,
  openReaderHandle,
  openWriterHandle,
  prefixUpperBound,
  readProfile,
  Store,
  TurndbError,
} from './core.mjs';

export { prefixUpperBound, Store, TurndbError };

/** Operations, limits, and explicit absences reachable through this npm/WASI binding. */
export async function capabilities() {
  // An existing store already owns the one runtime. Reading the immutable binding profile does not
  // need another directory capability and must remain available while that store is open.
  if (runtimePromise != null) {
    const runtime = await runtimePromise;
    if (runtime.active) return contractProfile(runtime);
  }
  const runtime = await acquireRuntime(process.cwd());
  try {
    return contractProfile(runtime);
  } finally {
    releaseRuntime(runtime);
  }
}

/** Mechanisms and format facts compiled into the WASI guest, independent of binding reachability. */
export async function compiledCapabilities() {
  if (runtimePromise != null) {
    const runtime = await runtimePromise;
    if (runtime.active) return readProfile(runtime, 'tdb_capabilities');
  }
  const runtime = await acquireRuntime(process.cwd());
  try {
    return readProfile(runtime, 'tdb_capabilities');
  } finally {
    releaseRuntime(runtime);
  }
}

let cachedModule = null;
let runtimePromise = null;
let acquireTail = Promise.resolve();

function wasiFor(hostDir) {
  return new WASI({
    version: 'preview1',
    args: ['turndb'],
    env: {},
    // Only the current store's parent directory is reachable from inside. The engine cannot see the rest of
    // the filesystem even if asked, which is a property of the target worth keeping.
    preopens: { [GUEST_ROOT]: hostDir },
    returnOnExit: true,
  });
}

async function createRuntime(hostDir) {
  const wasi = wasiFor(hostDir);
  // The runtime a `Store` holds: the instance, and what closing the handle releases.
  const state = { imports: wasi.getImportObject() };
  // WebAssembly imports are fixed at instantiation, while a WASI preopen is fixed when its WASI
  // object is created. Route every syscall through a replaceable table so later handles can mount
  // a different directory without constructing a second engine or exposing a common ancestor.
  const routedImports = Object.fromEntries(
    Object.entries(state.imports).map(([namespace, functions]) => [
      namespace,
      Object.fromEntries(
        Object.keys(functions).map((name) => [
          name,
          (...args) => state.imports[namespace][name](...args),
        ]),
      ),
    ]),
  );
  cachedModule ??= await WebAssembly.compile(await readFile(WASM_PATH));
  const instance = await WebAssembly.instantiate(cachedModule, routedImports);
  wasi.initialize(instance);
  const runtime = { instance, state, active: false, needsWasi: false, hostDir };
  runtime.release = () => releaseRuntime(runtime);
  return runtime;
}

function releaseRuntime(runtime) {
  const errno = runtime.state.imports.wasi_snapshot_preview1.fd_close(GUEST_ROOT_FD);
  runtime.active = false;
  runtime.needsWasi = true;
  if (errno !== 0) {
    throw new TurndbError(`closing the WASI store-directory capability failed with errno ${errno}`);
  }
}

async function acquireRuntime(hostDir) {
  const previous = acquireTail;
  let release;
  acquireTail = new Promise((resolve) => {
    release = resolve;
  });
  await previous;
  try {
    if (runtimePromise == null) {
      runtimePromise = createRuntime(hostDir);
      runtimePromise.catch(() => {
        runtimePromise = null;
      });
    }
    const runtime = await runtimePromise;
    if (runtime.active) {
      throw new TurndbError(
        `opening ${hostDir}: this process already has a store open — close its handle before opening another`,
      );
    }
    if (runtime.needsWasi || runtime.hostDir !== hostDir) {
      const wasi = wasiFor(hostDir);
      runtime.state.imports = wasi.getImportObject();
      // Give the new WASI capability object this instance's memory before any engine call reaches
      // it. Each WASI object is initialized once; the engine instance and its linear memory stay.
      try {
        wasi.initialize(runtime.instance);
      } catch (e) {
        runtime.state.imports.wasi_snapshot_preview1.fd_close(GUEST_ROOT_FD);
        throw e;
      }
      runtime.hostDir = hostDir;
      runtime.needsWasi = false;
    }
    runtime.active = true;
    return runtime;
  } finally {
    release();
  }
}

/**
 * Open (or create) a store at `file`.
 *
 * @param {string} dir  Host `.turndb` path. Its parent is created if absent.
 * @param {{blockTarget?: number, level?: number, maxRecordBytes?: number,
 *   maxBatchBytes?: number, maxBatchRecords?: number, maxIdentifierBytes?: number,
 *   maxStoredFrameBytes?: number, maxDecodedFrameBytes?: number,
 *   maxDirectoryEntries?: number, maxWalFrames?: number, maxFoldBlocks?: number}} [opts]
 *   `blockTarget` is the bytes gathered before a block seals (default 4 MiB) — bigger compresses
 *   harder and costs more per read. `level` is the zstd level — **this package defaults it to 3,
 *   not the engine's 19**, because this build is single-threaded: the block seal compresses on the
 *   calling thread inside whichever `putBody` crosses the boundary, and 4 MiB at level 19 is a
 *   ~1.7s event-loop stall where level 3 is ~80ms (measured through this build on synthetic
 *   bodies, Node 22, a single workstation). Level 3 costs more disk; the delta varies materially with
 *   workload ordering and configuration, so measure your own workload rather than trusting a
 *   figure (see README "When a write stalls"). Pass `level: 19` to choose ratio over latency knowingly; pass `0` for the
 *   engine default (currently 19). Both options are write-side only: a reader never needs to know
 *   either, so this choice is per-open and never a format commitment.
 * @returns {Promise<Store>}
 */
export async function open(dir, opts = {}) {
  // Validate the limits before touching the filesystem, so a bad option costs nothing.
  for (const name of [
    'maxRecordBytes', 'maxBatchBytes', 'maxBatchRecords', 'maxIdentifierBytes',
    'maxStoredFrameBytes', 'maxDecodedFrameBytes', 'maxDirectoryEntries', 'maxWalFrames',
    'maxFoldBlocks',
  ]) openLimit(opts[name], name);
  const hostPath = resolve(dir);
  const hostDir = dirname(hostPath);
  // WASI preopens the host directory before the guest runs, so create the parent first.
  // Without this the first call a new user makes fails inside `uvwasi_init` with a bare errno
  // that names neither the path nor the cause. The preopen is the PARENT: the store is one file
  // inside it, and its `-wal` sidecar lives beside it under the same mount.
  try {
    await mkdir(hostDir, { recursive: true });
  } catch (cause) {
    throw new TurndbError(
      `opening ${hostPath}: the store's parent directory could not be created: ${cause.message}`,
      'INVALID_ARGUMENT',
    );
  }
  const runtime = await acquireRuntime(hostDir);
  return openWriterHandle(runtime, `${GUEST_ROOT}/${basename(hostPath)}`, opts, hostDir);
}

/**
 * Open a TurnDB container read-only. The returned handle refuses every mutating method.
 *
 * WASI preopens directories, not files, so the file's parent is what the guest is given and the
 * file is named inside it.
 */
export async function openFile(file, opts = {}) {
  for (const name of [
    'maxStoredFrameBytes', 'maxDecodedFrameBytes', 'maxDirectoryEntries', 'maxWalFrames',
    'maxFoldBlocks',
  ]) openLimit(opts[name], name);
  const hostFile = resolve(file);
  const hostDir = dirname(hostFile);
  // A single file cannot be created by opening it, so this is a refusal rather than a mkdir — but
  // it has to happen here, because the WASI preopen of the parent fails first and reports an errno
  // that names neither the file nor the reason.
  try {
    const info = await stat(hostFile);
    if (!info.isFile()) {
      throw new TurndbError(`opening ${hostFile}: not a regular file`, 'INVALID_ARGUMENT');
    }
  } catch (cause) {
    if (cause instanceof TurndbError) throw cause;
    throw new TurndbError(
      `opening ${hostFile}: no such TurnDB container`,
      cause.code === 'ENOENT' ? 'NOT_FOUND' : 'INVALID_ARGUMENT',
    );
  }
  const runtime = await acquireRuntime(hostDir);
  return openReaderHandle(runtime, `${GUEST_ROOT}/${basename(hostFile)}`, opts, hostFile);
}

export default { open, openFile, capabilities, compiledCapabilities, Store, TurndbError };
