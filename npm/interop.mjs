import assert from 'node:assert/strict';
import { open } from './turndb/index.mjs';

function body(i) {
  const shared = 'You are a careful assistant. Prior turn content repeated verbatim. '.repeat(200);
  return `${shared}|unique turn ${i}|${'x'.repeat(i * 7)}`;
}

function id(i) {
  return `member/${String(1_700_000_000_000 + i).padStart(13, '0')}/${String(i).padStart(4, '0')}#input`;
}

function attrs(i, forWrite = false) {
  return [
    ['model', 'cross-runtime'],
    ['turn', BigInt(i)],
    // Explicit write wrapper keeps 0/7 and 7/7 in the float column. Reads return the bare number.
    ['ratio', forWrite ? { f: i / 7 } : i / 7],
    ['ok', i % 2 === 0],
    ['tag', 'first'],
    ['u', { u: 18446744073709551615n - BigInt(i) }],
    ['raw', Uint8Array.from([0, i, 255])],
    ['at', { timestampNs: -1700000000000000000n + BigInt(i) }],
    ['nothing', null],
    ['tag', 'second'],
  ];
}

async function writePortable(dir, n) {
  const store = await open(dir, { level: 3 });
  try {
    for (let i = 0; i < n; i++) store.putBody(id(i), body(i), attrs(i, true));
    store.sync();
    store.flush();
  } finally {
    store.close();
  }
}

async function readPortable(dir, n) {
  const store = await open(dir);
  try {
    const expectedIds = Array.from({ length: n }, (_, i) => id(i));
    assert.deepEqual(store.scanIds({ limit: n + 1 }), expectedIds);
    for (let i = 0; i < n; i++) {
      const record = store.getRecord(id(i));
      assert(record, `${id(i)} missing`);
      assert.equal(Buffer.from(record.body).toString(), body(i));
      assert.deepEqual(record.attrs, attrs(i));
    }
  } finally {
    store.close();
  }
}

const [mode, dir, count = '64'] = process.argv.slice(2);
const n = Number.parseInt(count, 10);
if (!dir || !Number.isSafeInteger(n) || n < 1 || !['write-portable', 'read-portable'].includes(mode)) {
  throw new Error('usage: interop.mjs <write-portable|read-portable> <store-dir> [records]');
}
if (mode === 'write-portable') await writePortable(dir, n);
else await readPortable(dir, n);
console.log(`OK  ${mode} ${n} records at ${dir}`);
