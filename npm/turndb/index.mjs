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

/** Thrown for every engine-reported failure, carrying its stable class and full message. */
export class TurndbError extends Error {
  constructor(message, code = 'INTERNAL') {
    super(message);
    this.name = 'TurndbError';
    this.code = code;
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

function openLimit(value, name) {
  if (value === undefined) return 0;
  if (!Number.isInteger(value) || value < 1 || value > 0xffff_ffff) {
    throw new TurndbError(`${name} must be an integer between 1 and 4294967295`);
  }
  return value;
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

function readProfile(runtime, exportName) {
  const e = runtime.instance.exports;
  const code = e[exportName]();
  const mem = new Uint8Array(e.memory.buffer);
  if (code < 0) {
    const message = new TextDecoder().decode(
      mem.subarray(e.tdb_err_ptr(), e.tdb_err_ptr() + e.tdb_err_len()),
    );
    const code = new TextDecoder().decode(
      mem.subarray(e.tdb_err_code_ptr(), e.tdb_err_code_ptr() + e.tdb_err_code_len()),
    );
    throw new TurndbError(message, code || 'INTERNAL');
  }
  const json = new TextDecoder().decode(
    mem.subarray(e.tdb_out_ptr(), e.tdb_out_ptr() + e.tdb_out_len()),
  );
  return JSON.parse(json);
}

/** Operations, limits, and explicit absences reachable through this npm/WASI binding. */
export async function capabilities() {
  // An existing store already owns the one runtime. Reading the immutable binding profile does not
  // need another directory capability and must remain available while that store is open.
  if (runtimePromise != null) {
    const runtime = await runtimePromise;
    if (runtime.active) return readProfile(runtime, 'tdb_binding_capabilities');
  }
  const runtime = await acquireRuntime(process.cwd());
  try {
    return readProfile(runtime, 'tdb_binding_capabilities');
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
    else if (typeof v === 'bigint') out.push([k, 'i', v.toString()]);
    else if (v === null) out.push([k, 'n', null]);
    else if (v instanceof Uint8Array) out.push([k, 'x', Array.from(v)]);
    else if (typeof v === 'number') {
      // Integer-valued floats are stored as ints, which is almost always what a caller means.
      // Pass a BigInt, or `{ f: n }`, when the distinction matters the other way.
      if (Number.isInteger(v)) {
        if (!Number.isSafeInteger(v)) {
          throw new TypeError(
            `integer attribute ${k} is outside JavaScript's exact Number range; pass a BigInt`,
          );
        }
        out.push([k, 'i', v.toString()]);
      } else out.push([k, 'f', encodeFloat(v)]);
    } else if (v && typeof v === 'object' && 'f' in v) {
      out.push([k, 'f', encodeFloat(Number(v.f))]);
    }
    else if (v && typeof v === 'object' && 'i' in v) {
      const i = v.i;
      if (typeof i === 'bigint') out.push([k, 'i', i.toString()]);
      else if (typeof i === 'number' && Number.isSafeInteger(i)) out.push([k, 'i', i.toString()]);
      else throw new TypeError(`integer attribute ${k} must be a safe integer or BigInt`);
    }
    else if (v && typeof v === 'object' && 'u' in v) {
      const u = v.u;
      if (typeof u === 'bigint' && u >= 0n && u <= 18446744073709551615n) {
        out.push([k, 'u', u.toString()]);
      } else if (typeof u === 'number' && Number.isSafeInteger(u) && u >= 0) {
        out.push([k, 'u', u.toString()]);
      } else throw new TypeError(`unsigned attribute ${k} must be a non-negative u64`);
    }
    else if (v && typeof v === 'object' && 'timestampNs' in v) {
      const timestamp = v.timestampNs;
      if (typeof timestamp === 'bigint') out.push([k, 't', timestamp.toString()]);
      else if (typeof timestamp === 'number' && Number.isSafeInteger(timestamp)) {
        out.push([k, 't', timestamp.toString()]);
      } else throw new TypeError(`timestamp attribute ${k} must be a signed i64 BigInt`);
    }
    else throw new TypeError(`attribute ${k} has unsupported type ${typeof v}`);
  }
  return JSON.stringify(out);
}

function encodeFloat(v) {
  if (Number.isNaN(v)) return 'NaN';
  if (v === Infinity) return 'inf';
  if (v === -Infinity) return '-inf';
  return v;
}

function decodeAttrs(tagged) {
  return tagged.map(([k, tag, v]) => [
    k,
    tag === 'i'
      ? BigInt(v)
      : tag === 'u'
        ? { u: BigInt(v) }
        : tag === 't'
          ? { timestampNs: BigInt(v) }
      : tag === 'f'
        ? decodeFloat(v)
        : tag === 'x'
          ? Uint8Array.from(v)
          : v,
  ]);
}

function decodeFloat(v) {
  if (v === 'inf') return Infinity;
  if (v === '-inf') return -Infinity;
  return Number(v);
}

/** One `[name, tag, value]` triple, tagged by exactly the rules the writer uses. */
function encodeAttrTriple(name, value) {
  return JSON.parse(encodeAttrs([[name, value]]))[0];
}

function encodePredicate(p, i) {
  if (!p || typeof p !== 'object') throw new TypeError(`predicate ${i} must be an object`);
  switch (p.kind) {
    case 'id':
      if (typeof p.value !== 'string') throw new TypeError(`predicate ${i} needs a string value`);
      return { kind: 'id', op: p.op, value: assertEncodable(p.value, `predicate ${i} value`) };
    case 'attr':
      if (typeof p.name !== 'string') throw new TypeError(`predicate ${i} needs a name`);
      return { kind: 'attr', op: p.op, attr: encodeAttrTriple(p.name, p.value) };
    case 'attr_exists':
    case 'content_exists':
      if (typeof p.name !== 'string') throw new TypeError(`predicate ${i} needs a name`);
      if (typeof p.present !== 'boolean') {
        throw new TypeError(`predicate ${i} needs a boolean \`present\``);
      }
      return { kind: p.kind, name: assertEncodable(p.name, `predicate ${i} name`), present: p.present };
    default:
      throw new TypeError(`predicate ${i} has unknown kind ${JSON.stringify(p.kind)}`);
  }
}

/**
 * Every key {@link Store.scan} understands.
 *
 * The engine refuses an unknown field too, but this layer builds the wire object key by key — so
 * without this check a misspelling would be dropped here and never reach the engine to be refused.
 * The caller would get a silent default and no indication anything was wrong.
 */
const SCAN_REQUEST_KEYS = new Set([
  'from',
  'to',
  'prefix',
  'direction',
  'cursor',
  'limit',
  'maxExamined',
  'maxResolutionEntries',
  'maxReconstructedBytes',
  'attrs',
  'contents',
  'predicates',
]);

function encodeScanRequest(opts) {
  if (opts == null || typeof opts !== 'object') {
    throw new TypeError('scan request must be an object');
  }
  for (const key of Object.keys(opts)) {
    if (!SCAN_REQUEST_KEYS.has(key)) {
      throw new TypeError(`scan request has unknown field ${JSON.stringify(key)}`);
    }
  }
  let { from, to, prefix } = opts;
  if (prefix != null) {
    assertEncodable(prefix, 'prefix');
    from = prefix;
    to = prefixUpperBound(prefix) ?? undefined;
  }
  const req = {};
  if (from != null) req.from = assertEncodable(from, 'from');
  if (to != null) req.to = assertEncodable(to, 'to');
  if (opts.direction != null) req.direction = opts.direction;
  if (opts.cursor != null) req.cursor = opts.cursor;
  if (opts.limit != null) req.limit = opts.limit;
  if (opts.maxExamined != null) req.maxExamined = opts.maxExamined;
  if (opts.maxResolutionEntries != null) req.maxResolutionEntries = opts.maxResolutionEntries;
  // Decimal text, because a JSON number cannot carry every u64 exactly.
  if (opts.maxReconstructedBytes != null) {
    req.maxReconstructedBytes = opts.maxReconstructedBytes.toString();
  }
  if (opts.attrs != null) req.attrs = opts.attrs;
  if (opts.contents != null) req.contents = opts.contents;
  if (opts.predicates != null) req.predicates = opts.predicates.map(encodePredicate);
  return req;
}

function decodeScanPage(v) {
  return {
    rows: v.rows.map((row) => ({
      id: row.id,
      attrs: decodeAttrs(row.attrs),
      contents: row.contents.map((c) => {
        const out = { name: c.name, present: c.present };
        if (c.len !== undefined) out.len = BigInt(c.len);
        if (c.pieces !== undefined) out.pieces = c.pieces;
        if (c.identity !== undefined) out.identity = c.identity;
        if (c.bytes !== undefined) out.bytes = Buffer.from(c.bytes, 'base64');
        return out;
      }),
    })),
    ...(v.next == null ? {} : { next: v.next }),
    stats: {
      durationNs: BigInt(v.stats.durationNs),
      examined: v.stats.examined,
      returned: v.stats.returned,
      duplicateAttrOccurrences: v.stats.duplicateAttrOccurrences,
      contentValuesReconstructed: v.stats.contentValuesReconstructed,
      reconstructedBytes: BigInt(v.stats.reconstructedBytes),
      reconstructionBudgetExhausted: v.stats.reconstructionBudgetExhausted,
      io: Object.fromEntries(Object.entries(v.stats.io).map(([k, n]) => [k, BigInt(n)])),
      resolution: {
        physicalRows: BigInt(v.stats.resolution.physicalRows),
        supersededRows: BigInt(v.stats.resolution.supersededRows),
        tombstones: BigInt(v.stats.resolution.tombstones),
        memtableEntries: BigInt(v.stats.resolution.memtableEntries),
        budgetExhausted: v.stats.resolution.budgetExhausted,
      },
    },
  };
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
  #readLimits;

  constructor(runtime, handle, readLimits) {
    this.#runtime = runtime;
    this.#exports = runtime.instance.exports;
    this.#handle = handle;
    this.#readLimits = readLimits;
    storeFinalizer.register(this, { runtime, handle }, this);
  }

  get closed() {
    return this.#handle < 0;
  }

  /** Operations and limits reachable through this binding. */
  capabilities() {
    this.#alive();
    return readProfile(this.#runtime, 'tdb_binding_capabilities');
  }

  /** Exact frame-byte and persistent object-count admission configured for this handle. */
  readLimits() {
    this.#alive();
    return { ...this.#readLimits };
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

  /** The engine's stable class and full message — neither is inferred from the other. */
  #err() {
    const ptr = this.#exports.tdb_err_ptr();
    const len = this.#exports.tdb_err_len();
    const codePtr = this.#exports.tdb_err_code_ptr();
    const codeLen = this.#exports.tdb_err_code_len();
    return {
      message:
        len === 0
          ? 'turndb reported a failure with no message'
          : this.#dec.decode(this.#mem().subarray(ptr, ptr + len)),
      code:
        codeLen === 0
          ? 'INTERNAL'
          : this.#dec.decode(this.#mem().subarray(codePtr, codePtr + codeLen)),
    };
  }

  #check(code) {
    if (code < 0) {
      const error = this.#err();
      throw new TurndbError(error.message, error.code);
    }
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

  /**
   * Apply generic records and deletions atomically.
   *
   * A successful result says whether this exact batch is durable. With `durable: true`, the engine
   * syncs before returning; a caller may discard its source copy only after receiving
   * `{ durable: true }`. A thrown error is deliberately not an acknowledgement.
   *
   * Named contents are an ordered array rather than an object. Content names must be unique within
   * one record, while attributes deliberately keep order and duplicate names.
   *
   * @param {Array<{kind:'put', id:string,
   *   contents:Array<{name:string,bytes:Uint8Array|Buffer|string}>, attrs?:object|Array<[string,unknown]>}
   *   | {kind:'delete',id:string}>} operations
   * @param {{durable?:boolean}} [options]
   * @returns {{applied:number,durable:boolean}}
   */
  write(operations, options = {}) {
    this.#alive();
    if (!Array.isArray(operations)) throw new TypeError('write operations must be an array');
    const durable = options.durable ?? false;
    if (typeof durable !== 'boolean') throw new TypeError('write durable option must be a boolean');
    const items = operations.map((operation, i) => {
      if (!operation || typeof operation !== 'object') {
        throw new TypeError(`write operation ${i} must be an object`);
      }
      assertId(operation.id);
      if (operation.kind === 'delete') {
        if ('contents' in operation || 'attrs' in operation) {
          throw new TypeError(`delete write operation ${i} must not carry contents or attrs`);
        }
        return ['del', operation.id];
      }
      if (operation.kind !== 'put') {
        throw new TypeError(`write operation ${i} kind must be "put" or "delete"`);
      }
      if (!Array.isArray(operation.contents)) {
        throw new TypeError(`put write operation ${i} contents must be an array`);
      }
      const contents = operation.contents.map((content, contentIndex) => {
        if (!content || typeof content !== 'object' || typeof content.name !== 'string') {
          throw new TypeError(`write operation ${i} content ${contentIndex} needs a string name`);
        }
        assertEncodable(content.name, `write operation ${i} content ${contentIndex} name`);
        const bytes = typeof content.bytes === 'string' ? this.#enc.encode(content.bytes) : content.bytes;
        if (!(bytes instanceof Uint8Array)) {
          throw new TypeError(`write operation ${i} content ${contentIndex} bytes must be bytes or a string`);
        }
        return [content.name, Buffer.from(bytes).toString('base64')];
      });
      return ['put', operation.id, contents, JSON.parse(encodeAttrs(operation.attrs))];
    });
    const a = [this.#putText(JSON.stringify(items))];
    try {
      this.#check(this.#exports.tdb_write(this.#handle, ...a[0], durable ? 1 : 0));
      return JSON.parse(this.#outText());
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
   * measured on synthetic stores up to 1.9 GB, host `przym`; level 3 unmeasured). It never fires
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

  /**
   * One structured page: projected attributes, named-content metadata or bytes, a checked
   * continuation cursor, and the page's exact work statistics.
   *
   * The difference from {@link Store.scanIds} is that the engine does the filtering and the
   * projection. `attrs` and `contents` are projections — a page that selects no content opens no
   * fold block, which is what makes a metadata-only timeline cheap. `predicates` are evaluated in
   * Rust against exact stored values, so a float comparison honours the stored NaN payload rather
   * than whatever JavaScript would have done to it.
   *
   * `cursor` is opaque and checked: pass back `next` with the same range, direction, and
   * predicates. Projection and page size may change between pages.
   *
   * **Not available on this build:** the native binding's `timeoutMs`/`signal`. This engine is
   * single-threaded with no clock in the guest, so there is nothing to interrupt a scan from — and
   * accepting the options to ignore them would be worse than not offering them.
   *
   * @param {object} [request]
   * @returns {{rows: Array<{id: string, attrs: Array<[string, unknown]>, contents: object[]}>, next?: string, stats: object}}
   */
  scan(request = {}) {
    this.#alive();
    const a = [this.#putText(JSON.stringify(encodeScanRequest(request)))];
    try {
      this.#check(this.#exports.tdb_scan(this.#handle, ...a[0]));
      return decodeScanPage(JSON.parse(this.#outText()));
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
   * Verify the complete committed snapshot and return exact evidence for every leg.
   *
   * Staged writes are outside this scope. Call {@link sync} and {@link flush} first when they must
   * be included. `incomplete` is a successful verification with an explicitly unestablished legacy
   * fact; corruption throws `TurndbError` with `code === 'CORRUPTION'`.
   */
  verify() {
    this.#alive();
    this.#check(this.#exports.tdb_verify(this.#handle));
    const report = JSON.parse(this.#outText());
    report.fold.bytes = BigInt(report.fold.bytes);
    report.fold.trailingUncommittedBytes = BigInt(report.fold.trailingUncommittedBytes);
    report.contentBytes = BigInt(report.contentBytes);
    return report;
  }

  /**
   * Cheap operational facts. `state: 'available'` means the handle answered; it is NOT an integrity
   * verdict. Call {@link verify} for that.
   */
  health() {
    this.#alive();
    this.#check(this.#exports.tdb_health(this.#handle));
    const health = JSON.parse(this.#outText());
    for (const key of [
      'commit',
      'partRows',
      'walBytes',
      'walFrames',
      'foldDiskBytes',
      'foldCacheHits',
      'foldCacheMisses',
      'maxStoredFrameBytes',
      'maxDecodedFrameBytes',
      'maxDirectoryEntries',
      'maxWalFrames',
      'maxFoldBlocks',
      'punchedBlocks',
    ]) {
      health[key] = BigInt(health[key]);
    }
    return health;
  }

  /** Cumulative operation counters and durations since this handle opened. */
  metrics() {
    this.#alive();
    this.#check(this.#exports.tdb_metrics(this.#handle));
    const metrics = JSON.parse(this.#outText());
    const operationKeys = [
      'openRecovery', 'sync', 'flush', 'compaction', 'backup', 'verification', 'punch',
      'refold', 'erase', 'formatMigration',
    ];
    for (const key of operationKeys) {
      for (const field of [
        'attempts', 'succeeded', 'failed', 'cancelled', 'totalDurationNs', 'lastDurationNs',
        'maxDurationNs',
      ]) metrics[key][field] = BigInt(metrics[key][field]);
    }
    metrics.recoveredWalFrames = BigInt(metrics.recoveredWalFrames);
    metrics.verificationCorruptionFailures = BigInt(metrics.verificationCorruptionFailures);
    for (const field of ['pieces', 'dedupHits', 'logicalBytes', 'novelBytes']) {
      metrics.foldedContent[field] = BigInt(metrics.foldedContent[field]);
    }
    return metrics;
  }

  /** Non-destructive lifecycle journal read after an independent sequence cursor. */
  lifecycleEvents({ after = 0n, limit } = {}) {
    this.#alive();
    if (typeof after !== 'bigint' || after < 0n) {
      throw new TypeError('lifecycle after must be a non-negative bigint');
    }
    if (limit !== undefined && (!Number.isInteger(limit) || limit < 0)) {
      throw new TypeError('lifecycle limit must be a non-negative integer');
    }
    const input = this.#putText(JSON.stringify({ after: after.toString(), ...(limit === undefined ? {} : { limit }) }));
    try {
      this.#check(this.#exports.tdb_lifecycle_events(this.#handle, ...input));
      const batch = JSON.parse(this.#outText());
      batch.oldestAvailableSequence = batch.oldestAvailableSequence == null
        ? null
        : BigInt(batch.oldestAvailableSequence);
      batch.latestSequence = BigInt(batch.latestSequence);
      batch.droppedEvents = BigInt(batch.droppedEvents);
      for (const event of batch.events) {
        event.sequence = BigInt(event.sequence);
        event.durationNs = BigInt(event.durationNs);
      }
      return batch;
    } finally {
      this.#free([input]);
    }
  }

  /** Exact live/dead/reclaimable content facts for a flushed committed snapshot. */
  contentLiveness() {
    this.#alive();
    this.#check(this.#exports.tdb_content_liveness(this.#handle));
    const report = JSON.parse(this.#outText());
    for (const field of [
      'livePieces', 'liveLogicalBytes', 'deadLogicalBytes', 'strandedDeadLogicalBytes',
    ]) report[field] = BigInt(report[field]);
    for (const block of [report.liveBlocks, report.reclaimableBlocks]) {
      for (const field of ['blocks', 'rawBytes', 'storedBytes']) block[field] = BigInt(block[field]);
    }
    return report;
  }

  /** Reachability-aware file usage; allocated bytes are explicitly absent on WASI. */
  spaceUsage() {
    this.#alive();
    this.#check(this.#exports.tdb_space_usage(this.#handle));
    const usage = JSON.parse(this.#outText());
    for (const amount of [usage.live, usage.retainedOnly, usage.unclassified, usage.total]) {
      amount.logicalBytes = BigInt(amount.logicalBytes);
      if (amount.allocatedBytes.state === 'measured') {
        amount.allocatedBytes.bytes = BigInt(amount.allocatedBytes.bytes);
      }
    }
    if (usage.filesystemAvailableBytes.state === 'measured') {
      usage.filesystemAvailableBytes.bytes = BigInt(usage.filesystemAvailableBytes.bytes);
    }
    return usage;
  }

  /** Erase named ids and return this operation's logical and reclamation outcomes. */
  eraseIds(ids) {
    this.#alive();
    if (!Array.isArray(ids) || ids.some((id) => typeof id !== 'string')) {
      throw new TypeError('eraseIds needs an array of string ids');
    }
    const input = this.#putText(JSON.stringify(ids));
    try {
      this.#check(this.#exports.tdb_erase_ids(this.#handle, ...input));
      const result = JSON.parse(this.#outText());
      if (result.reclamation.state === 'measured') {
        result.reclamation.bytes = BigInt(result.reclamation.bytes);
      }
      return result;
    } finally {
      this.#free([input]);
    }
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
 * @param {{blockTarget?: number, level?: number, maxRecordBytes?: number,
 *   maxBatchBytes?: number, maxBatchRecords?: number, maxIdentifierBytes?: number,
 *   maxStoredFrameBytes?: number, maxDecodedFrameBytes?: number,
 *   maxDirectoryEntries?: number, maxWalFrames?: number, maxFoldBlocks?: number}} [opts]
 *   `blockTarget` is the bytes gathered before a block seals (default 4 MiB) — bigger compresses
 *   harder and costs more per read. `level` is the zstd level — **this package defaults it to 3,
 *   not the engine's 19**, because this build is single-threaded: the block seal compresses on the
 *   calling thread inside whichever `putBody` crosses the boundary, and 4 MiB at level 19 is a
 *   ~1.7s event-loop stall where level 3 is ~80ms (measured through this build on synthetic
 *   bodies, Node 22, host `przym`). Level 3 costs more disk; the delta varies materially with
 *   workload ordering and configuration, so measure your own workload rather than trusting a
 *   figure (see README "When a write stalls"). Pass `level: 19` to choose ratio over latency knowingly; pass `0` for the
 *   engine default (currently 19). Both options are write-side only: a reader never needs to know
 *   either, so this choice is per-open and never a format commitment.
 * @returns {Promise<Store>}
 */
export async function open(dir, opts = {}) {
  const maxRecordBytes = openLimit(opts.maxRecordBytes, 'maxRecordBytes');
  const maxBatchBytes = openLimit(opts.maxBatchBytes, 'maxBatchBytes');
  const maxBatchRecords = openLimit(opts.maxBatchRecords, 'maxBatchRecords');
  const maxIdentifierBytes = openLimit(opts.maxIdentifierBytes, 'maxIdentifierBytes');
  const maxStoredFrameBytes = openLimit(opts.maxStoredFrameBytes, 'maxStoredFrameBytes');
  const maxDecodedFrameBytes = openLimit(opts.maxDecodedFrameBytes, 'maxDecodedFrameBytes');
  const maxDirectoryEntries = openLimit(opts.maxDirectoryEntries, 'maxDirectoryEntries');
  const maxWalFrames = openLimit(opts.maxWalFrames, 'maxWalFrames');
  const maxFoldBlocks = openLimit(opts.maxFoldBlocks, 'maxFoldBlocks');
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
      handle = instance.exports.tdb_open_v3(
        ptr,
        path.length,
        opts.blockTarget ?? 0,
        opts.level ?? 3,
        maxRecordBytes,
        maxBatchBytes,
        maxBatchRecords,
        maxIdentifierBytes,
        maxStoredFrameBytes,
        maxDecodedFrameBytes,
        maxDirectoryEntries,
        maxWalFrames,
        maxFoldBlocks,
      );
    } finally {
      instance.exports.tdb_free(ptr, path.length);
    }

    if (handle < 0) {
      const ep = instance.exports.tdb_err_ptr();
      const el = instance.exports.tdb_err_len();
      const msg = new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer).subarray(ep, ep + el));
      const cp = instance.exports.tdb_err_code_ptr();
      const cl = instance.exports.tdb_err_code_len();
      const code = new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer, cp, cl));
      throw new TurndbError(`opening ${hostDir}: ${msg}`, code || 'INTERNAL');
    }
  } catch (e) {
    try {
      releaseRuntime(runtime);
    } catch (closeError) {
      e.cause ??= closeError;
    }
    throw e;
  }
  const profile = readProfile(runtime, 'tdb_capabilities');
  return new Store(runtime, handle, {
    maxStoredFrameBytes: maxStoredFrameBytes || profile.max_stored_frame_bytes_default,
    maxDecodedFrameBytes: maxDecodedFrameBytes || profile.max_decoded_frame_bytes_default,
    maxDirectoryEntries: maxDirectoryEntries || profile.max_directory_entries_default,
    maxWalFrames: maxWalFrames || profile.max_wal_frames_default,
    maxFoldBlocks: maxFoldBlocks || profile.max_fold_blocks_default,
  });
}

export default { open, capabilities, compiledCapabilities, Store, TurndbError };
