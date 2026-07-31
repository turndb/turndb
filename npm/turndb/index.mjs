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
 * `turndb` CLI against the same directory, which needs no daemon and no second copy of the data.
 *
 * ## Durability, in one sentence
 *
 * `put` is not durable; `sync()` is the ACK point. `flush()` is a separate thing again — it seals
 * writes into the columnar plane so OTHER readers can see them. This handle sees its own unflushed
 * writes without either.
 *
 * ## Single writer
 *
 * **This package is always the `wasm32-wasip1` build** — the host OS does not switch it onto the
 * native engine — and WASI has no advisory locking, so the engine **cannot** enforce exclusion.
 * The native build's `flock` is not in play here, on any host.
 *
 * The host layer permits only one live `Store` in a process, but that is not cross-process
 * exclusion. The obligation is still the embedder's: **at most one open writer per store directory
 * across every process.** Two processes can interleave their write-ahead logs and corrupt the
 * store, and detection is not guaranteed.
 */

import { WASI } from 'node:wasi';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join, resolve } from 'node:path';

const WASM_PATH = join(dirname(fileURLToPath(import.meta.url)), 'turndb.wasm');

/** Where the store directory is mounted inside the sandbox. Callers never see this. */
const GUEST_ROOT = '/store';
/** Preview1 reserves 0..2 for stdio, so our only preopen is descriptor 3. */
const GUEST_ROOT_FD = 3;

/** Thrown for every engine-reported failure, carrying the engine's own message. */
export class TurndbError extends Error {
  constructor(message) {
    super(message);
    this.name = 'TurndbError';
  }
}

/**
 * Refuse a string JS can hold but UTF-8 cannot represent.
 *
 * JS strings are UTF-16 and may contain unpaired surrogates; `TextEncoder` maps those to U+FFFD
 * *silently*. So `putBody('a\uD800', …)` and `putBody('a\uDC00', …)` both land on `a�` and the
 * second overwrites the first — two records the caller believes are distinct become one, with no
 * error, in a store whose cardinal invariant is byte-exact reconstruction. Refusing is the engine's
 * own discipline: a store that cannot be written is recoverable, one that lies is not.
 */
function assertEncodable(s, what) {
  // Deliberately silent on non-strings: existing paths already reject or coerce them, and the
  // engine's batch error names the offending item index, which is better than anything thrown here.
  if (typeof s !== 'string') return s;
  const ok =
    typeof s.isWellFormed === 'function'
      ? s.isWellFormed()
      : !/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/.test(s);
  if (!ok) {
    throw new TurndbError(
      `${what} contains an unpaired surrogate, which UTF-8 cannot represent — refusing rather ` +
        `than substituting U+FFFD, which would silently alias it onto a different id`,
    );
  }
  return s;
}

/**
 * An id must be a string, and must be one UTF-8 can represent.
 *
 * Coercion is not a convenience here, it is an aliasing bug: `#putText` would have encoded `{}` as
 * `"[object Object]"`, which collides with the literal string of the same name — measured, three
 * writes producing two records with one body lost. That is the same silent-overwrite this module
 * refuses unpaired surrogates for, arriving through a different door, and the colliding value is a
 * string a real serialization bug has already produced in this codebase.
 *
 * `applyBatch` deliberately does NOT use this: the engine rejects a non-string id there with a
 * message naming the offending item's index, which is more useful than anything thrown from here.
 */
function assertId(id, what = 'id') {
  if (typeof id !== 'string') {
    throw new TurndbError(
      `${what} must be a string, got ${typeof id} — refusing rather than coercing, because ` +
        `String(value) silently aliases distinct inputs onto one record`,
    );
  }
  return assertEncodable(id, what);
}

/**
 * The first id that cannot start with `prefix` — the exclusive upper bound of its range — or
 * `null` when no such id exists and the range is therefore unbounded above.
 *
 * Computed over CODE POINTS, carrying left across trailing U+10FFFF. The obvious version bumps the
 * last UTF-16 code *unit*, which is wrong three ways: it breaks surrogate pairs into unpaired ones,
 * it wraps at U+FFFF to produce a bound BELOW the prefix (an inverted, silently-empty range), and
 * it has no answer for a prefix of all-maximal scalars. Carrying handles the first two; the third
 * genuinely has no upper bound, because no valid Unicode string sorts above that prefix family —
 * and `null` says so rather than inventing a boundary.
 *
 * Exported for tests: the boundary cases are the whole point and they deserve direct assertions.
 */
export function prefixUpperBound(prefix) {
  // Guarded here rather than only at the call site: this is exported and documented as contract, so
  // a caller may use it to build `from`/`to` directly. Unguarded, a malformed prefix carried into a
  // malformed bound — `'\uD800'` produced `'\uD801'`, which encodes to U+FFFD — reintroducing the
  // wrong-boundary defect through the very helper added to remove it.
  assertEncodable(prefix, 'prefix');
  const cps = Array.from(prefix);
  for (let i = cps.length - 1; i >= 0; i--) {
    const cp = cps[i].codePointAt(0);
    if (cp < 0x10ffff) {
      // D800..DFFF are surrogate code points, not scalars — step over the hole.
      const next = cp + 1 === 0xd800 ? 0xe000 : cp + 1;
      return cps.slice(0, i).join('') + String.fromCodePoint(next);
    }
    // A trailing U+10FFFF cannot be incremented; drop it and carry into the scalar to its left.
  }
  return null;
}

/**
 * Encode attributes into the ABI's tagged form: `[[key, tag, value], ...]`.
 *
 * turndb preserves attribute ORDER and DUPLICATE KEYS because byte-exact reconstruction depends on
 * both, and a JS object can represent neither — so an object input is a convenience that quietly
 * gives up those properties, while an array of `[key, value]` pairs keeps them. The tag is explicit
 * because `1` and `1.0` are the same JS number but different stored values.
 */
function encodeAttrs(attrs) {
  if (attrs == null) return '[]';
  const pairs = Array.isArray(attrs) ? attrs : Object.entries(attrs);
  const out = [];
  for (const pair of pairs) {
    if (!Array.isArray(pair) || pair.length < 2) {
      throw new TypeError('each attribute must be a [key, value] pair');
    }
    const [k, v] = pair;
    if (typeof k !== 'string') throw new TypeError(`attribute key must be a string, got ${typeof k}`);
    assertEncodable(k, 'attribute key');
    if (typeof v === 'string') out.push([k, 's', assertEncodable(v, `attribute ${k}`)]);
    else if (typeof v === 'boolean') out.push([k, 'b', v]);
    else if (typeof v === 'bigint') out.push([k, 'i', Number(v)]);
    else if (typeof v === 'number') {
      // Integer-valued floats are stored as ints, which is almost always what a caller means.
      // Pass a BigInt, or `{ f: n }`, when the distinction matters the other way.
      out.push(Number.isInteger(v) ? [k, 'i', v] : [k, 'f', v]);
    } else if (v && typeof v === 'object' && 'f' in v) out.push([k, 'f', Number(v.f)]);
    else if (v && typeof v === 'object' && 'i' in v) out.push([k, 'i', Number(v.i)]);
    else throw new TypeError(`attribute ${k} has unsupported type ${typeof v}`);
  }
  return JSON.stringify(out);
}

function decodeAttrs(tagged) {
  return tagged.map(([k, tag, v]) => [k, tag === 'i' || tag === 'f' ? Number(v) : v]);
}

const storeFinalizer = new FinalizationRegistry(({ runtime, handle }) => {
  // A forgotten close must not wedge this process forever. Finalization is only a fallback: it
  // cannot report either error and gives no timing guarantee, so callers still close explicitly.
  try {
    runtime.instance.exports.tdb_close(handle);
  } finally {
    try {
      releaseRuntime(runtime);
    } catch {}
  }
});

/** An open turndb store. Create with {@link open}. */
export class Store {
  #runtime;
  #exports;
  #handle;
  #enc = new TextEncoder();
  #dec = new TextDecoder();

  constructor(runtime, handle) {
    this.#runtime = runtime;
    this.#exports = runtime.instance.exports;
    this.#handle = handle;
    storeFinalizer.register(this, { runtime, handle }, this);
  }

  get closed() {
    return this.#handle < 0;
  }

  #mem() {
    // Re-read every time: the buffer is DETACHED and replaced whenever linear memory grows, so a
    // cached view silently becomes a view over nothing.
    return new Uint8Array(this.#exports.memory.buffer);
  }

  /** Copy bytes into the instance and return `[ptr, len]`, both zero for empty. */
  #put(bytes) {
    if (!bytes || bytes.length === 0) return [0, 0];
    const ptr = this.#exports.tdb_alloc(bytes.length);
    if (ptr === 0) throw new TurndbError(`failed to allocate ${bytes.length} bytes in the instance`);
    this.#mem().set(bytes, ptr);
    return [ptr, bytes.length];
  }

  #putText(s) {
    return this.#put(this.#enc.encode(s ?? ''));
  }

  #free(pairs) {
    for (const [ptr, len] of pairs) if (ptr !== 0) this.#exports.tdb_free(ptr, len);
  }

  /** The engine's own error message — never flattened into something generic. */
  #err() {
    const ptr = this.#exports.tdb_err_ptr();
    const len = this.#exports.tdb_err_len();
    if (len === 0) return 'turndb reported a failure with no message';
    return this.#dec.decode(this.#mem().subarray(ptr, ptr + len));
  }

  #check(code) {
    if (code < 0) throw new TurndbError(this.#err());
    return code;
  }

  /** A copy of the output buffer. Copied because the next call overwrites it. */
  #out() {
    const ptr = this.#exports.tdb_out_ptr();
    const len = this.#exports.tdb_out_len();
    return this.#mem().slice(ptr, ptr + len);
  }

  #outText() {
    return this.#dec.decode(this.#out());
  }

  #alive() {
    if (this.#handle < 0) throw new TurndbError('store is closed');
  }

  /**
   * Write one record. NOT durable until {@link sync}.
   *
   * @param {string} id  Sort key as well as identity. Ids sort lexicographically, so an id designed
   *   with the query in mind (`member/timestamp/...`) gives prefix-then-time paging out of
   *   {@link scanIds} with no secondary index.
   * @param {Uint8Array|Buffer|string} body
   * @param {Record<string,unknown>|Array<[string,unknown]>} [attrs]
   */
  putBody(id, body, attrs) {
    this.#alive();
    assertId(id);
    const bytes = typeof body === 'string' ? this.#enc.encode(body) : body;
    // Validate and encode the attributes BEFORE reserving anything in the instance. `encodeAttrs`
    // throws on a malformed attribute, and it used to be called inside the array literal that also
    // performed the id and body allocations — so a throw escaped before `a` was bound, the
    // `finally` never ran, and the id and body allocations leaked. Refusing an input must not cost
    // the process memory it cannot get back: a rejected write has to stay recoverable.
    const attrsText = encodeAttrs(attrs);
    const a = [this.#putText(id), this.#put(bytes), this.#putText(attrsText)];
    try {
      this.#check(this.#exports.tdb_put_body(this.#handle, ...a[0], ...a[1], ...a[2]));
    } finally {
      this.#free(a);
    }
  }

  /**
   * Apply many records atomically — the batch replays all-or-nothing, so a crash cannot leave a
   * partial export committed. This is the shape an OTLP export should use: one call per export.
   *
   * @param {Array<{id: string, body?: Uint8Array|string, attrs?: object, delete?: boolean}>} records
   * @returns {number} records applied
   */
  applyBatch(records) {
    this.#alive();
    const items = records.map((r) => {
      assertEncodable(r.id, 'id');
      if (r.delete) return ['del', r.id];
      const bytes = typeof r.body === 'string' ? this.#enc.encode(r.body) : r.body;
      return ['put', r.id, Buffer.from(bytes ?? new Uint8Array()).toString('base64'), JSON.parse(encodeAttrs(r.attrs))];
    });
    const a = [this.#putText(JSON.stringify(items))];
    try {
      return this.#check(this.#exports.tdb_apply(this.#handle, ...a[0]));
    } finally {
      this.#free(a);
    }
  }

  /** Tombstone a record. Not durable until {@link sync}. */
  delete(id) {
    this.#alive();
    assertId(id);
    const a = [this.#putText(id)];
    try {
      this.#check(this.#exports.tdb_delete(this.#handle, ...a[0]));
    } finally {
      this.#free(a);
    }
  }

  /** Make everything written so far durable. **This is the ACK point.** */
  sync() {
    this.#alive();
    this.#check(this.#exports.tdb_sync(this.#handle));
  }

  /**
   * Seal the memtable into an immutable part.
   *
   * Separate from {@link sync} on purpose: this handle already sees its own unflushed writes, so
   * flushing is about making them visible to OTHER readers and to the columnar plane. Flushing too
   * often costs compression — blocks sealed short compress worse — so batch it.
   */
  flush() {
    this.#alive();
    this.#check(this.#exports.tdb_flush(this.#handle));
  }

  /**
   * Total merge when the live part list reaches the engine's threshold. Returns whether a merge ran.
   *
   * The stall is the caller's: this build runs the merge on the calling thread, and a total
   * merge's wall time is linear in the store's on-disk content (~5s/GB at level 19, wasm —
   * measured on synthetic stores up to 1.9 GB, a single workstation; level 3 unmeasured). It never fires
   * on its own — nothing compacts inside `putBody`/`sync`/`flush` — so
   * schedule it when a multi-second pause is acceptable, or use {@link Store.maybeCompact} to
   * bound the pause instead. Total merges are also the only ones that settle deletes: a tombstone
   * can only be dropped when the merge covers every live part.
   */
  autoCompact() {
    this.#alive();
    return this.#check(this.#exports.tdb_auto_compact(this.#handle)) === 1;
  }

  /**
   * Bounded compaction: if at least `trigger` parts are live, merge the oldest `run` of them.
   * Returns whether a merge ran.
   *
   * The dial for callers with a latency budget: the merge's input — and therefore the stall — is
   * capped at the oldest `run` parts instead of the whole store. Call it after flushes; repeated
   * calls amortize what {@link Store.autoCompact} would do in one linear-in-the-store pause.
   * The trade: bounded merges never settle deletes (tombstones are carried, not dropped), so run a
   * total merge occasionally if the store sees deletions.
   *
   * @param {{trigger?: number, run?: number}} [opts]  Defaults `trigger: 8` (the engine's own
   *   total-merge threshold) and `run: 4`.
   * @returns {boolean}
   */
  maybeCompact(opts = {}) {
    this.#alive();
    const trigger = opts.trigger ?? 8;
    const run = opts.run ?? 4;
    if (!Number.isInteger(trigger) || trigger < 2 || !Number.isInteger(run) || run < 2) {
      throw new TurndbError(
        `maybeCompact: trigger and run must be integers >= 2 (got trigger=${trigger}, run=${run})`,
      );
    }
    return this.#check(this.#exports.tdb_maybe_compact(this.#handle, trigger, run)) === 1;
  }

  /**
   * The record's body, byte-exact, or `null` if absent or deleted.
   * @returns {Uint8Array|null}
   */
  get(id) {
    this.#alive();
    assertId(id);
    const a = [this.#putText(id)];
    try {
      return this.#check(this.#exports.tdb_reconstruct(this.#handle, ...a[0])) === 1 ? this.#out() : null;
    } finally {
      this.#free(a);
    }
  }

  /** The body decoded as UTF-8 text, or `null`. */
  getText(id) {
    const b = this.get(id);
    return b === null ? null : this.#dec.decode(b);
  }

  /**
   * The full record — body plus attributes, with order and duplicate keys intact.
   * @returns {{id: string, body: Uint8Array, attrs: Array<[string, unknown]>}|null}
   */
  getRecord(id) {
    this.#alive();
    assertId(id);
    const a = [this.#putText(id)];
    try {
      if (this.#check(this.#exports.tdb_get_record(this.#handle, ...a[0])) !== 1) return null;
      const v = JSON.parse(this.#outText());
      return { id: v.id, body: Buffer.from(v.body, 'base64'), attrs: decodeAttrs(v.attrs) };
    } finally {
      this.#free(a);
    }
  }

  /**
   * Live ids in `[from, to)`, in id order, at most `limit`.
   *
   * The paging primitive. Ids sort lexicographically, so a `prefix/` range is a contiguous run and
   * costs a binary search plus a walk of exactly that run — not a scan of the store.
   *
   * @param {{from?: string, to?: string, prefix?: string, limit?: number, reverse?: boolean}} [opts]
   * @returns {string[]}
   */
  scanIds(opts = {}) {
    this.#alive();
    let { from = '', to = '', prefix, limit = 100, reverse = false } = opts;
    if (prefix != null) {
      // The half-open range holding exactly the ids that start with `prefix`. An empty prefix, and
      // one made entirely of U+10FFFF, both have no upper bound — which is the unbounded scan, not
      // an empty one. `''` is how the ABI spells unbounded on either end.
      assertEncodable(prefix, 'prefix');
      from = prefix;
      to = prefixUpperBound(prefix) ?? '';
    }
    assertEncodable(from, 'from');
    assertEncodable(to, 'to');
    const a = [this.#putText(from), this.#putText(to)];
    try {
      this.#check(this.#exports.tdb_scan_ids(this.#handle, ...a[0], ...a[1], limit, reverse ? 1 : 0));
      return JSON.parse(this.#outText());
    } finally {
      this.#free(a);
    }
  }

  /** @returns {{records: number, parts: number}} */
  stats() {
    this.#alive();
    this.#check(this.#exports.tdb_stats(this.#handle));
    return JSON.parse(this.#outText());
  }

  /**
   * Close the store and release its handle.
   *
   * Deliberately not "releases the writer lock": this build holds no advisory lock to release (see
   * the note on single-writer above). Closing frees the handle; it does not hand exclusion back to
   * anyone, because the engine never had it.
   *
   * Does NOT sync — call {@link sync} first if the writes must survive. Deliberately explicit:
   * a close that silently synced would hide a failing disk behind a method nobody checks.
   */
  close() {
    if (this.#handle < 0) return;
    const h = this.#handle;
    this.#handle = -1;
    storeFinalizer.unregister(this);
    let failure;
    try {
      this.#check(this.#exports.tdb_close(h));
    } catch (e) {
      failure = e;
    }
    try {
      releaseRuntime(this.#runtime);
    } catch (e) {
      failure ??= e;
    }
    if (failure) throw failure;
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
    // Only the current store directory is reachable from inside. The engine cannot see the rest of
    // the filesystem even if asked, which is a property of the target worth keeping.
    preopens: { [GUEST_ROOT]: hostDir },
    returnOnExit: true,
  });
}

async function createRuntime(hostDir) {
  const wasi = wasiFor(hostDir);
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
  return { instance, state, active: false, needsWasi: false, hostDir };
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
 * Open (or create) a store at `dir`.
 *
 * @param {string} dir  Host directory. Created if absent.
 * @param {{blockTarget?: number, level?: number}} [opts]
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
  const hostDir = resolve(dir);
  const runtime = await acquireRuntime(hostDir);
  const { instance } = runtime;

  const enc = new TextEncoder();
  const path = enc.encode(GUEST_ROOT);
  let handle;
  try {
    const ptr = instance.exports.tdb_alloc(path.length);
    new Uint8Array(instance.exports.memory.buffer).set(path, ptr);
    try {
      handle = instance.exports.tdb_open(ptr, path.length, opts.blockTarget ?? 0, opts.level ?? 3);
    } finally {
      instance.exports.tdb_free(ptr, path.length);
    }

    if (handle < 0) {
      const ep = instance.exports.tdb_err_ptr();
      const el = instance.exports.tdb_err_len();
      const msg = new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer).subarray(ep, ep + el));
      throw new TurndbError(`opening ${hostDir}: ${msg}`);
    }
  } catch (e) {
    try {
      releaseRuntime(runtime);
    } catch (closeError) {
      e.cause ??= closeError;
    }
    throw e;
  }
  return new Store(runtime, handle);
}

export default { open, Store, TurndbError };
