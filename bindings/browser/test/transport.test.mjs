import assert from 'node:assert/strict';
import test from 'node:test';

import {
  BlobReadAt, BrowserDatabase, BufferReadAt, HttpRangeReadAt, TurnDbError,
} from '../index.mjs';

const bytes = Uint8Array.from({ length: 200_000 }, (_, index) => index % 251);

test('buffer positioned reads are exact and bounded', () => {
  const source = new BufferReadAt(bytes);
  assert.deepEqual(source.readSync(65_530n, 20), bytes.slice(65_530, 65_550));
  assert.throws(() => source.readSync(BigInt(bytes.length), 1), RangeError);
});

test('Blob cache fetches only requested blocks and stitches crossings', async () => {
  const source = new BlobReadAt(new Blob([bytes]), { blockSize: 64 * 1024, maxBlocks: 2 });
  assert.equal(source.readSync(65_530n, 20), undefined);
  await source.ensure(65_530n, 20);
  assert.deepEqual(source.readSync(65_530n, 20), bytes.slice(65_530, 65_550));
  assert.equal(source.blocks.size, 2);

  const atomic = new BlobReadAt(new Blob([bytes]), { blockSize: 4096, maxBlocks: 2 });
  await atomic.ensure(0n, 20_000);
  assert.equal(atomic.blocks.size, 0, 'oversized atomic admission does not displace the LRU');
  assert.deepEqual(atomic.readSync(0n, 20_000), bytes.slice(0, 20_000));
  atomic.releaseTransient();
  assert.equal(atomic.readSync(0n, 20_000), undefined);
});

test('HTTP source requires exact 206 responses and counts fetched bytes', async () => {
  const calls = [];
  const fetch = async (_url, options) => {
    const range = /^bytes=(\d+)-(\d+)$/.exec(options.headers.Range);
    calls.push(options.headers.Range);
    const start = Number(range[1]);
    const end = Number(range[2]);
    return new Response(bytes.slice(start, end + 1), {
      status: 206,
      headers: { 'Content-Range': `bytes ${start}-${end}/${bytes.length}` },
    });
  };
  const source = await HttpRangeReadAt.open('https://example.test/store', { fetch, blockSize: 64 * 1024 });
  await source.ensure(100_000n, 4);
  assert.deepEqual(source.readSync(100_000n, 4), bytes.slice(100_000, 100_004));
  assert.deepEqual(calls, ['bytes=0-0', 'bytes=65536-131071']);
  assert.equal(source.stats.networkBytes, 1 + 64 * 1024);
  await assert.rejects(
    HttpRangeReadAt.open('https://example.test/bad', {
      fetch: async () => new Response(new Uint8Array(), {
        status: 206,
        headers: { 'Content-Range': `bytes 0-0/${bytes.length}` },
      }),
    }),
    /returned 0 bytes, expected 1/,
  );
  await assert.rejects(
    BrowserDatabase.openUrl({}, 'https://example.test/no-range', {
      fetch: async () => new Response(bytes, { status: 200 }),
    }),
    (error) => error instanceof TurnDbError && error.code === 'IO',
  );
});

test('database retries exact missing ranges instead of falling back to whole-file reads', async () => {
  class FakeStore {
    static open(read) {
      if (!read(0n, 16)) throw new Error('TURNDB_RANGE:0:16');
      return new FakeStore(read);
    }
    constructor(read) { this.read = read; }
    scan() {
      if (!this.read(131_072n, 8)) throw new Error('TURNDB_RANGE:131072:8');
      return { rows: ['ok'] };
    }
    static capabilities() { return { profile: 'browser' }; }
  }
  const db = await BrowserDatabase.open({ BrowserStore: FakeStore }, new BlobReadAt(new Blob([bytes])));
  assert.deepEqual(await db.scan({ contractVersion: 1 }), { rows: ['ok'] });
  assert(db.fetchStats().cachedBlocks <= 64);
});

test('a declared footprint is fetched ahead as transient ranges and released after', async () => {
  const requests = [];
  const source = new BlobReadAt(new Blob([bytes]), { blockSize: 4096, maxBlocks: 2 });
  const fetchRange = source.fetchRange.bind(source);
  source.fetchRange = async (start, end, signal) => {
    requests.push([start, end]);
    return fetchRange(start, end, signal);
  };
  // Two ranges one block apart merge into one request; a distant range is its own.
  await source.ensureRanges([
    { offset: '100', length: '50' },
    { offset: '4200', length: '10' },
    { offset: '150000', length: '3000' },
  ]);
  assert.deepEqual(requests, [[100n, 4210n], [150000n, 153000n]]);
  assert.deepEqual(source.readSync(120n, 30), bytes.slice(120, 150), 'served from the transient range');
  assert.deepEqual(source.readSync(151000n, 2000), bytes.slice(151000, 153000));
  assert.equal(source.readSync(4300n, 10), undefined, 'bytes outside the footprint are still misses');
  assert.equal(source.blocks.size, 0, 'a footprint never displaces the LRU');
  // A range already held transient is not fetched again.
  await source.ensureRanges([{ offset: '110', length: '20' }]);
  assert.equal(requests.length, 2);
  source.releaseTransient();
  assert.equal(source.readSync(120n, 30), undefined, 'released footprints are gone');
});
