// Differential gate: the portable structured scan against the native one.
//
// Both bindings call the same `Store::scan`, but each marshals the request and the page through its
// own layer — a JSON/base64 ABI here, N-API buffers and bigints there — and each spells the request
// in its own dialect. That is exactly where a binding-level surface goes quietly wrong: a dropped
// duplicate attribute, a page that stops one row early, a cursor that means something different on
// the other side. So this writes one store, opens it under both, and compares.
//
// Deliberately NOT part of `npm/turndb/test/`: that suite must run against nothing but the shipped
// `.wasm`, and this needs a native addon built from the same tree.
//
// Usage: TURNDB_NATIVE_PATH=target/debug/libturndb_node.so node npm/differential-scan.mjs
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createRequire } from 'node:module';
// Same guard the package suite uses. Without it this harness would compare a freshly built native
// addon against a stale `.wasm` and report the disagreement as a portable-binding bug — or, worse,
// report agreement that the shipped artifact does not actually have.
import './turndb/test/_artifact.mjs';
import { open as openPortable } from './turndb/index.mjs';

const require = createRequire(import.meta.url);

if (!process.env.TURNDB_NATIVE_PATH) {
  console.error('differential-scan: set TURNDB_NATIVE_PATH to a built turndb-node library');
  process.exit(2);
}
process.env.TURNDB_NATIVE_PATH = resolve(process.env.TURNDB_NATIVE_PATH);
const native = require('../bindings/node/index.cjs');

const RECORDS = 40;

/** Bodies that repeat a long shared prefix, so pieces are shared across records. */
function body(i) {
  const shared = 'You are a careful assistant. Prior turn content repeated verbatim. '.repeat(20);
  return `${shared}|turn ${i}|${'x'.repeat(i * 3)}`;
}

function id(i) {
  return `m/alice/${String(i).padStart(4, '0')}`;
}

/**
 * One attribute set, spelled for each side. Duplicate keys and mixed types are the point: a
 * projection that flattens to a map, or a binding that loses the int/uint/float distinction, has to
 * fail here rather than in production.
 */
function portableAttrs(i) {
  return [
    ['kind', i % 2 === 0 ? 'llm_exchange' : 'tool_action'],
    ['ts', BigInt(1000 + i)],
    ['ratio', { f: i / 7 }],
    ['ok', i % 3 === 0],
    ['tag', 'first'],
    ['big', { u: 18446744073709551615n - BigInt(i) }],
    ['raw', Uint8Array.from([0, i % 256, 255])],
    ['at', { timestampNs: -1700000000000000000n + BigInt(i) }],
    ['nothing', null],
    ['tag', 'second'],
  ];
}

// ── Normalization ───────────────────────────────────────────────────────────
//
// The two dialects are compared through one canonical form rather than by making either pretend to
// be the other.

function canonPortableValue(v) {
  if (typeof v === 'string') return ['string', v];
  if (typeof v === 'boolean') return ['bool', String(v)];
  if (typeof v === 'bigint') return ['int', v.toString()];
  if (v === null) return ['null', ''];
  if (v instanceof Uint8Array) return ['binary', Buffer.from(v).toString('hex')];
  if (typeof v === 'number') return ['float', Object.is(v, -0) ? '-0' : String(v)];
  if (typeof v === 'object' && 'u' in v) return ['uint', v.u.toString()];
  if (typeof v === 'object' && 'timestampNs' in v) return ['timestamp_ns', v.timestampNs.toString()];
  throw new Error(`unclassified portable attribute value: ${JSON.stringify(v)}`);
}

function canonNativeValue(a) {
  switch (a.kind) {
    case 'string':
      return ['string', a.stringValue];
    case 'bool':
      return ['bool', String(a.boolValue)];
    case 'int':
      return ['int', a.intValue.toString()];
    case 'uint':
      return ['uint', a.uintValue.toString()];
    case 'float':
      return ['float', Object.is(a.floatValue, -0) ? '-0' : String(a.floatValue)];
    case 'binary':
      return ['binary', Buffer.from(a.binaryValue).toString('hex')];
    case 'timestamp_ns':
      return ['timestamp_ns', a.timestampNsValue.toString()];
    case 'null':
      return ['null', ''];
    default:
      throw new Error(`unclassified native attribute kind: ${a.kind}`);
  }
}

const canonContent = (c) => ({
  name: c.name,
  present: c.present,
  len: c.len === undefined ? null : c.len.toString(),
  pieces: c.pieces === undefined ? null : c.pieces,
  identity: c.identity ?? null,
  bytes: c.bytes === undefined ? null : Buffer.from(c.bytes).toString('hex'),
});

function canonPortablePage(p) {
  return {
    rows: p.rows.map((r) => ({
      id: r.id,
      attrs: r.attrs.map(([k, v]) => [k, ...canonPortableValue(v)]),
      contents: r.contents.map(canonContent),
    })),
    next: p.next ?? null,
  };
}

function canonNativePage(p) {
  return {
    rows: p.rows.map((r) => ({
      id: r.id,
      attrs: r.attrs.map((a) => [a.name, ...canonNativeValue(a)]),
      contents: r.contents.map(canonContent),
    })),
    next: p.next ?? null,
  };
}

/**
 * The statistics that describe the ANSWER rather than the machine that produced it.
 *
 * `durationNs` is wall time. The io byte and cache counters are excluded deliberately: both
 * bindings read the same bytes, but cache residency differs between a fresh native snapshot and a
 * portable handle that has already served earlier requests in this process, so comparing them would
 * assert something neither binding promises. `foldBlocksTouched` IS compared — whether a page
 * reconstructed content at all is a contract, not an implementation detail.
 */
const canonStats = (s) => ({
  examined: s.examined,
  returned: s.returned,
  duplicateAttrOccurrences: s.duplicateAttrOccurrences,
  contentValuesReconstructed: s.contentValuesReconstructed,
  reconstructedBytes: s.reconstructedBytes.toString(),
  reconstructionBudgetExhausted: s.reconstructionBudgetExhausted,
  foldBlocksTouched: s.io.foldBlocksTouched.toString(),
  resolution: {
    physicalRows: s.resolution.physicalRows.toString(),
    supersededRows: s.resolution.supersededRows.toString(),
    tombstones: s.resolution.tombstones.toString(),
    memtableEntries: s.resolution.memtableEntries.toString(),
    budgetExhausted: s.resolution.budgetExhausted,
  },
});

// ── The request battery, in both dialects ───────────────────────────────────

const attrPred = (name, op, portableValue, nativeAttr) => ({
  portable: { kind: 'attr', name, op, value: portableValue },
  native: { kind: 'attr', op, value: { name, ...nativeAttr } },
});

const CASES = [
  {
    name: 'whole range, no projection',
    portable: { from: 'm/', limit: 100 },
    native: { from: 'm/', limit: 100 },
  },
  {
    name: 'attribute projection with duplicates',
    portable: { from: 'm/', limit: 100, attrs: ['tag', 'ts', 'nothing'] },
    native: { from: 'm/', limit: 100, attrs: ['tag', 'ts', 'nothing'] },
  },
  {
    name: 'every attribute type projected',
    portable: { from: 'm/', limit: 100, attrs: ['kind', 'ts', 'ratio', 'ok', 'big', 'raw', 'at', 'nothing'] },
    native: { from: 'm/', limit: 100, attrs: ['kind', 'ts', 'ratio', 'ok', 'big', 'raw', 'at', 'nothing'] },
  },
  {
    name: 'metadata-only content',
    portable: { from: 'm/', limit: 100, contents: [{ name: 'body', mode: 'metadata' }] },
    native: { from: 'm/', limit: 100, contents: [{ name: 'body', mode: 'metadata' }] },
  },
  {
    name: 'reconstructed content bytes',
    portable: { from: 'm/', limit: 100, contents: [{ name: 'body', mode: 'bytes' }] },
    native: { from: 'm/', limit: 100, contents: [{ name: 'body', mode: 'bytes' }] },
  },
  {
    name: 'reverse',
    portable: { from: 'm/', limit: 100, direction: 'reverse' },
    native: { from: 'm/', limit: 100, direction: 'reverse' },
  },
  {
    name: 'string equality predicate',
    portable: {
      from: 'm/',
      limit: 100,
      predicates: [attrPred('kind', 'eq', 'llm_exchange', { kind: 'string', stringValue: 'llm_exchange' }).portable],
    },
    native: {
      from: 'm/',
      limit: 100,
      predicates: [attrPred('kind', 'eq', 'llm_exchange', { kind: 'string', stringValue: 'llm_exchange' }).native],
    },
  },
  {
    name: 'integer range predicate at the boundary',
    portable: {
      from: 'm/',
      limit: 100,
      predicates: [attrPred('ts', 'gte', 1020n, { kind: 'int', intValue: 1020n }).portable],
    },
    native: {
      from: 'm/',
      limit: 100,
      predicates: [attrPred('ts', 'gte', 1020n, { kind: 'int', intValue: 1020n }).native],
    },
  },
  {
    name: 'u64 predicate above 2^53',
    portable: {
      from: 'm/',
      limit: 100,
      predicates: [
        attrPred('big', 'gte', { u: 18446744073709551600n }, { kind: 'uint', uintValue: 18446744073709551600n }).portable,
      ],
    },
    native: {
      from: 'm/',
      limit: 100,
      predicates: [
        attrPred('big', 'gte', { u: 18446744073709551600n }, { kind: 'uint', uintValue: 18446744073709551600n }).native,
      ],
    },
  },
  {
    name: 'binary attribute equality',
    portable: {
      from: 'm/',
      limit: 100,
      predicates: [
        attrPred('raw', 'eq', Uint8Array.from([0, 5, 255]), {
          kind: 'binary',
          binaryValue: Buffer.from([0, 5, 255]),
        }).portable,
      ],
    },
    native: {
      from: 'm/',
      limit: 100,
      predicates: [
        attrPred('raw', 'eq', Uint8Array.from([0, 5, 255]), {
          kind: 'binary',
          binaryValue: Buffer.from([0, 5, 255]),
        }).native,
      ],
    },
  },
  {
    name: 'id predicate',
    portable: { from: 'm/', limit: 100, predicates: [{ kind: 'id', op: 'gt', value: 'm/alice/0030' }] },
    native: { from: 'm/', limit: 100, predicates: [{ kind: 'id', op: 'gt', idValue: 'm/alice/0030' }] },
  },
  {
    name: 'attr_exists',
    portable: { from: 'm/', limit: 100, predicates: [{ kind: 'attr_exists', name: 'ratio', present: true }] },
    native: { from: 'm/', limit: 100, predicates: [{ kind: 'attr_exists', name: 'ratio', present: true }] },
  },
  {
    name: 'content_exists',
    portable: { from: 'm/', limit: 100, predicates: [{ kind: 'content_exists', name: 'body', present: true }] },
    native: { from: 'm/', limit: 100, predicates: [{ kind: 'content_exists', name: 'body', present: true }] },
  },
  {
    name: 'examination bound forces a partial page',
    portable: { from: 'm/', limit: 100, maxExamined: 7 },
    native: { from: 'm/', limit: 100, maxExamined: 7 },
  },
  {
    name: 'reconstruction ceiling forces a partial page',
    portable: { from: 'm/', limit: 100, contents: [{ name: 'body', mode: 'bytes' }], maxReconstructedBytes: 4096n },
    native: { from: 'm/', limit: 100, contents: [{ name: 'body', mode: 'bytes' }], maxReconstructedBytes: 4096n },
  },
];

async function main() {
  const dir = await mkdtemp(join(tmpdir(), 'turndb-diff-'));
  let failures = 0;
  let checked = 0;

  try {
    // One store, written once, read by both.
    const w = await openPortable(dir);
    try {
      for (let i = 0; i < RECORDS; i++) w.putBody(id(i), body(i), portableAttrs(i));
      w.putBody('m/bob/0000', 'bob', [['kind', 'llm_exchange'], ['ts', 9999n]]);
      w.delete('m/alice/0007'); // a tombstone the readers must agree about
      w.sync();
      w.flush();
    } finally {
      w.close();
    }

    const p = await openPortable(dir);
    const n = await native.NativeSnapshot.open(dir);
    try {
      for (const c of CASES) {
        const pp = p.scan(c.portable);
        const np = await n.scan(c.native);
        checked++;
        try {
          assert.deepEqual(canonPortablePage(pp), canonNativePage(np));
          assert.deepEqual(canonStats(pp.stats), canonStats(np.stats));
          assert.ok(pp.rows.length > 0 || c.name.includes('exists'), `${c.name}: empty result proves nothing`);
          console.log(`  ok   ${c.name} (${pp.rows.length} rows)`);
        } catch (e) {
          failures++;
          console.log(`  FAIL ${c.name}`);
          console.log(`       ${e.message.split('\n').slice(0, 12).join('\n       ')}`);
        }
      }

      // Cursors are produced by the engine, so a continuation minted by one binding must be
      // accepted by the other and resume at the same row. This is the strongest single statement
      // that the two surfaces are one contract rather than two lookalikes.
      const first = p.scan({ from: 'm/', limit: 5 });
      assert.ok(first.next, 'a bounded first page must carry a cursor');
      const nativeResumed = await n.scan({ from: 'm/', limit: 5, cursor: first.next });
      const portableResumed = p.scan({ from: 'm/', limit: 5, cursor: first.next });
      checked++;
      try {
        assert.deepEqual(canonNativePage(nativeResumed), canonPortablePage(portableResumed));
        console.log('  ok   a portable cursor resumes identically under native');
      } catch (e) {
        failures++;
        console.log(`  FAIL cursor cross-acceptance\n       ${e.message.split('\n').slice(0, 8).join('\n       ')}`);
      }

      // And the reverse direction of the same claim.
      const nFirst = await n.scan({ from: 'm/', limit: 5 });
      const portableFromNative = p.scan({ from: 'm/', limit: 5, cursor: nFirst.next });
      const nativeFromNative = await n.scan({ from: 'm/', limit: 5, cursor: nFirst.next });
      checked++;
      try {
        assert.deepEqual(canonPortablePage(portableFromNative), canonNativePage(nativeFromNative));
        console.log('  ok   a native cursor resumes identically under portable');
      } catch (e) {
        failures++;
        console.log(`  FAIL reverse cursor cross-acceptance\n       ${e.message.split('\n').slice(0, 8).join('\n       ')}`);
      }

      // Full paged traversal under both, compared as whole id sequences.
      const walk = async (pageFn) => {
        const out = [];
        let cursor;
        for (let guard = 0; guard < 200; guard++) {
          const page = await pageFn(cursor);
          out.push(...page.rows.map((r) => r.id));
          if (!page.next) break;
          cursor = page.next;
        }
        return out;
      };
      const pWalk = await walk((cursor) => p.scan({ from: 'm/', limit: 3, cursor }));
      const nWalk = await walk((cursor) => n.scan({ from: 'm/', limit: 3, cursor }));
      checked++;
      try {
        assert.deepEqual(pWalk, nWalk);
        assert.equal(pWalk.length, RECORDS, 'the traversal must cover every present record');
        assert.equal(new Set(pWalk).size, pWalk.length, 'no id twice');
        assert.ok(!pWalk.includes('m/alice/0007'), 'the tombstoned id must not reappear');
        console.log(`  ok   paged traversal agrees across bindings (${pWalk.length} rows)`);
      } catch (e) {
        failures++;
        console.log(`  FAIL paged traversal\n       ${e.message.split('\n').slice(0, 8).join('\n       ')}`);
      }
    } finally {
      await n.close();
      p.close();
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }

  console.log(
    `\ndifferential-scan: ${checked - failures}/${checked} comparisons agree ` +
      `(${RECORDS} records, one store, two bindings)`,
  );
  process.exit(failures === 0 ? 0 : 1);
}

await main();
