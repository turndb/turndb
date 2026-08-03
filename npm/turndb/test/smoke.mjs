import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import './_artifact.mjs';
import { open, capabilities, TurndbError } from '../index.mjs';

async function withStore(fn, opts) {
  const dir = await mkdtemp(join(tmpdir(), 'turndb-test-'));
  const s = await open(dir, opts);
  try { return await fn(s, dir); } finally { try { s.close(); } catch {} await rm(dir, { recursive: true, force: true }); }
}

test('capabilities describe the WASI guest rather than its host', async () => {
  const c = await capabilities();
  assert.equal(c.portable_wasm, true);
  assert.equal(c.writer_exclusion, 'embedder_enforced');
  assert.equal(c.physical_erasure, 'refold_only');
  assert.equal(c.threads, false);
  assert.equal(c.columnar, false);
  assert.equal(c.sql, false);
  assert.equal(c.part_format_write, 4);
  assert.equal(c.write_admission_limits, true);
  assert.equal(c.store_space_usage, true);
  assert.equal(c.allocated_space_usage, false);
  assert.equal(c.format_migration, true);
  assert.equal(c.max_record_bytes_default, 64 << 20);
  assert.equal(c.max_batch_bytes_default, 256 << 20);
  assert.equal(c.max_batch_records_default, 4096);
  assert.equal(c.max_identifier_bytes_default, 4096);

  await withStore((s) => assert.deepEqual(s.capabilities(), c));
});

test('a record round-trips byte-exact, including non-UTF8 bytes', async () => {
  await withStore((s) => {
    // The cardinal invariant. Arbitrary bytes, not just text — a body is bytes.
    const body = new Uint8Array(1024);
    for (let i = 0; i < body.length; i++) body[i] = (i * 31) % 256;
    s.putBody('bin/1', body);
    s.sync();
    assert.deepEqual(s.get('bin/1'), body);
    s.flush();
    assert.deepEqual(s.get('bin/1'), body, 'must survive the flush into the columnar plane');
  });
});

test('the writer sees its own unflushed writes', async () => {
  await withStore((s) => {
    s.putBody('a', 'hello');
    // No sync, no flush — the handle must still answer, which is what lets a live view read back
    // what it just wrote without paying a flush.
    assert.equal(s.getText('a'), 'hello');
  });
});

test('attributes keep order and duplicate keys', async () => {
  await withStore((s) => {
    const attrs = [['k', 'first'], ['n', 42], ['k', 'second'], ['f', 1.5], ['ok', true]];
    s.putBody('r', 'body', attrs);
    s.sync();
    const got = s.getRecord('r');
    assert.equal(got.attrs.length, 5);
    assert.deepEqual(got.attrs.map(([k]) => k), ['k', 'n', 'k', 'f', 'ok']);
    assert.deepEqual(got.attrs[0], ['k', 'first']);
    assert.deepEqual(got.attrs[2], ['k', 'second']);
    assert.equal(got.attrs[1][1], 42n);
    assert.equal(got.attrs[3][1], 1.5);
    assert.equal(got.attrs[4][1], true);
    assert.equal(Buffer.from(got.body).toString(), 'body');
  });
});

test('i64 attributes never round through a JavaScript Number', async () => {
  await withStore((s) => {
    const min = -9223372036854775808n;
    const max = 9223372036854775807n;
    s.putBody('ints', 'body', [['min', min], ['max', max]]);
    assert.deepEqual(s.getRecord('ints').attrs, [['min', min], ['max', max]]);
    assert.throws(
      () => s.putBody('unsafe', 'body', { n: Number.MAX_SAFE_INTEGER + 1 }),
      /pass a BigInt/,
    );
  });
});

test('extended scalar attributes preserve their type and exact value', async () => {
  await withStore((s) => {
    const attrs = [
      ['u', { u: 18446744073709551615n }],
      ['raw', Uint8Array.from([0, 255, 128])],
      ['at', { timestampNs: -9223372036854775808n }],
      ['nothing', null],
    ];
    s.putBody('extended', 'body', attrs);
    const got = Object.fromEntries(s.getRecord('extended').attrs);
    assert.deepEqual(got.u, { u: 18446744073709551615n });
    assert.deepEqual(got.raw, Uint8Array.from([0, 255, 128]));
    assert.deepEqual(got.at, { timestampNs: -9223372036854775808n });
    assert.equal(got.nothing, null);
  });
});

test('non-finite floats cross the JSON-only portable ABI deliberately', async () => {
  await withStore((s) => {
    s.putBody('floats', 'body', [['nan', { f: NaN }], ['pos', { f: Infinity }], ['neg', { f: -Infinity }]]);
    const attrs = Object.fromEntries(s.getRecord('floats').attrs);
    assert.equal(Number.isNaN(attrs.nan), true);
    assert.equal(attrs.pos, Infinity);
    assert.equal(attrs.neg, -Infinity);
  });
});

test('deduplication actually happens across records', async () => {
  await withStore((s) => {
    // The whole thesis: the same context re-sent many times costs once.
    const shared = 'the resent conversation. '.repeat(4000);
    for (let i = 0; i < 40; i++) s.putBody(`t/${String(i).padStart(4, '0')}`, shared + i);
    s.sync();
    s.flush();
    assert.equal(s.stats().records, 40);
    for (let i = 0; i < 40; i++) assert.equal(s.getText(`t/${String(i).padStart(4, '0')}`), shared + i);
  });
});

test('scanIds pages in id order, reverses, and bounds by prefix', async () => {
  await withStore((s) => {
    for (const m of ['alice', 'bob']) for (let i = 0; i < 5; i++) s.putBody(`${m}/${i}`, `x${i}`);
    s.sync(); s.flush();
    assert.deepEqual(s.scanIds({ prefix: 'alice/' }), ['alice/0','alice/1','alice/2','alice/3','alice/4']);
    assert.deepEqual(s.scanIds({ prefix: 'alice/', limit: 2 }), ['alice/0', 'alice/1']);
    assert.deepEqual(s.scanIds({ prefix: 'alice/', reverse: true, limit: 2 }), ['alice/4', 'alice/3']);
    // A prefix range must not leak the neighbour, which is the property paging depends on.
    assert.ok(s.scanIds({ prefix: 'alice/' }).every((id) => id.startsWith('alice/')));
    assert.equal(s.scanIds({ prefix: 'nobody/' }).length, 0);
  });
});

test('a batch applies atomically and delete shadows', async () => {
  await withStore((s) => {
    const n = s.applyBatch([
      { id: 'b/1', body: 'one', attrs: { kind: 'x' } },
      { id: 'b/2', body: 'two' },
    ]);
    assert.equal(n, 2);
    s.sync();
    assert.equal(s.getText('b/1'), 'one');
    s.applyBatch([{ id: 'b/1', delete: true }]);
    s.sync();
    assert.equal(s.get('b/1'), null, 'a tombstone must resolve to absence');
    assert.equal(s.getText('b/2'), 'two');
  });
});

test('portable writes honor the same configurable admission policy', async () => {
  await withStore((s) => {
    assert.throws(() => s.putBody('x', ''), /worst-case WAL frame/);
    assert.equal(s.get('x'), null);
  }, { maxRecordBytes: 1 });

  await withStore((s) => {
    assert.throws(
      () => s.applyBatch([{ id: 'a', delete: true }, { id: 'b', delete: true }]),
      /exceeding the configured limit of 1/,
    );
    assert.equal(s.get('a'), null, 'a refused oversized batch applies nothing');
  }, { maxBatchRecords: 1 });

  await withStore((s) => {
    assert.throws(() => s.putBody('abcde', ''), /record id.*5 UTF-8 bytes/);
  }, { maxIdentifierBytes: 4 });

  const dir = await mkdtemp(join(tmpdir(), 'turndb-invalid-limits-'));
  try {
    await assert.rejects(open(dir, { maxBatchBytes: 0 }), /between 1 and 4294967295/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('errors carry the engine message, not a generic one', async () => {
  await withStore((s) => {
    s.close();
    assert.throws(() => s.putBody('x', 'y'), TurndbError);
    // A malformed batch must be refused before anything is applied.
  });
  await withStore((s) => {
    assert.throws(() => s.applyBatch([{ id: 'ok', body: 'a' }, { id: 42, body: 'b' }]), (e) => {
      assert.ok(e instanceof TurndbError);
      assert.match(e.message, /batch item 1/, `message should name the bad item, got: ${e.message}`);
      return true;
    });
    s.sync();
    assert.equal(s.get('ok'), null, 'a rejected batch must apply NOTHING');
  });
});

test('data survives close and reopen', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'turndb-reopen-'));
  try {
    let s = await open(dir);
    s.putBody('persist/1', 'durable');
    s.sync(); s.flush(); s.close();
    s = await open(dir);
    assert.equal(s.getText('persist/1'), 'durable');
    assert.equal(s.stats().records, 1);
    s.close();
  } finally { await rm(dir, { recursive: true, force: true }); }
});

test('a large body crosses the boundary intact', async () => {
  await withStore((s) => {
    // Big enough to force linear memory to grow, which detaches every cached ArrayBuffer view —
    // the bug this pins is a stale Uint8Array over a detached buffer.
    const big = Buffer.alloc(12 << 20, 'abcdefgh');
    s.putBody('big', big);
    s.sync();
    assert.equal(Buffer.compare(Buffer.from(s.get('big')), big), 0);
  });
});
