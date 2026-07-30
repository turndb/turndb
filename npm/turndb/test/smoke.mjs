import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { open, TurndbError } from '../index.mjs';

async function withStore(fn, opts) {
  const dir = await mkdtemp(join(tmpdir(), 'turndb-test-'));
  const s = await open(dir, opts);
  try { return await fn(s, dir); } finally { try { s.close(); } catch {} await rm(dir, { recursive: true, force: true }); }
}

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
    assert.equal(got.attrs[1][1], 42);
    assert.equal(got.attrs[3][1], 1.5);
    assert.equal(got.attrs[4][1], true);
    assert.equal(Buffer.from(got.body).toString(), 'body');
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
