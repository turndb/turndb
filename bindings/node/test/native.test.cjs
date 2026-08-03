'use strict';

const assert = require('node:assert/strict');
const child = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { capabilities, NativeSnapshot, NativeStore, retainedCommits, TurnDbError } = require('..');

function temporaryStore(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-native-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test('reports the native capability profile without a portable fallback', () => {
  assert.deepEqual(capabilities(), {
    partFormatWrite: 3,
    partFormatReadMax: 3,
    writerExclusion: 'os_enforced',
    physicalErasure: process.platform === 'linux' ? 'punch_or_refold' : 'refold_only',
    positionedIo: true,
    threads: true,
    columnar: false,
    sql: false,
    portableWasm: false,
    nativeNode: true,
    napiVersion: 6,
    commandQueueCapacity: 64,
    commandQueueCapacityMax: 65536,
    immutableSnapshots: true,
    lifecycleOperations: true,
    healthSnapshots: true,
    schemaDiscovery: true,
  });
});

test('configures a bounded per-store command backlog without breaking the default open call', async (t) => {
  const defaultStore = await NativeStore.open(temporaryStore(t));
  assert.equal(defaultStore.commandQueueCapacity, 64);
  await defaultStore.close();

  const configured = await NativeStore.open(temporaryStore(t), { commandQueueCapacity: 3 });
  assert.equal(configured.commandQueueCapacity, 3);
  await configured.close();

  await assert.rejects(
    NativeStore.open(temporaryStore(t), { commandQueueCapacity: 0 }),
    (error) => error instanceof TurnDbError
      && error.code === 'INVALID_ARGUMENT'
      && /between 1 and 65536/.test(error.message)
  );
  await assert.rejects(
    NativeStore.open(temporaryStore(t), { commandQueueCapacity: 65537 }),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT'
  );
});

test('refuses a missing native artifact instead of silently loading WASM', () => {
  const env = { ...process.env };
  delete env.TURNDB_NATIVE_PATH;
  assert.throws(
    () => child.execFileSync(process.execPath, ['-e', 'require(".")'], {
      cwd: path.resolve(__dirname, '..'),
      env,
      stdio: 'pipe',
    }),
    (error) => {
      assert.match(error.stderr.toString(), /does not silently fall back/);
      return true;
    }
  );
});

test('round-trips exact typed fields and independently named content', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  assert(store instanceof NativeStore);
  t.after(async () => {
    try { await store.close(); } catch {}
  });

  await store.write(
    [{
      kind: 'put',
      id: 'trace/1',
      contents: [
        { name: 'request', bytes: Buffer.from('shared') },
        { name: 'response', bytes: Buffer.from('shared') },
        { name: 'empty', bytes: Buffer.alloc(0) },
      ],
      attrs: [
        { name: 'tag', kind: 'string', stringValue: 'first' },
        { name: 'tag', kind: 'string', stringValue: 'second' },
        { name: 'minimum', kind: 'int', intValue: -9223372036854775808n },
        { name: 'nan', kind: 'float', floatValue: NaN },
        { name: 'sampled', kind: 'bool', boolValue: true },
      ],
    }],
    true
  );

  const page = await store.scan({
    attrs: ['tag', 'minimum', 'nan', 'sampled'],
    contents: [
      { name: 'request', mode: 'metadata' },
      { name: 'response', mode: 'bytes' },
      { name: 'empty', mode: 'bytes' },
      { name: 'absent', mode: 'bytes' },
    ],
  });
  assert.equal(page.rows.length, 1);
  assert.deepEqual(page.rows[0].attrs.map(({ name }) => name), [
    'tag', 'tag', 'minimum', 'nan', 'sampled',
  ]);
  assert.equal(page.rows[0].attrs[2].intValue, -9223372036854775808n);
  assert(Number.isNaN(page.rows[0].attrs[3].floatValue));
  assert.equal(page.rows[0].contents[0].bytes, undefined);
  assert.equal(page.rows[0].contents[0].len, 6n);
  assert.match(page.rows[0].contents[0].identity, /^[0-9a-f]{64}$/);
  assert.equal(page.rows[0].contents[1].bytes.toString(), 'shared');
  assert.equal(page.rows[0].contents[1].identity, page.rows[0].contents[0].identity);
  assert.equal(page.rows[0].contents[2].present, true);
  assert.equal(page.rows[0].contents[2].bytes.length, 0);
  assert.match(page.rows[0].contents[2].identity, /^[0-9a-f]{64}$/);
  assert.notEqual(page.rows[0].contents[2].identity, page.rows[0].contents[0].identity);
  assert.equal(page.rows[0].contents[3].present, false);
  assert.equal(page.rows[0].contents[3].identity, undefined);
  assert.equal(page.rows[0].contents[3].bytes, undefined);
  assert.equal((await store.readContent('trace/1', 'request')).toString(), 'shared');
  assert.equal(await store.readContent('trace/1', 'absent'), null);
});

test('pages and filters in Rust and refuses cursor misuse', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  t.after(async () => {
    try { await store.close(); } catch {}
  });
  await store.write(
    Array.from({ length: 7 }, (_, i) => ({
      kind: 'put',
      id: `r${i}`,
      attrs: [{ name: 'even', kind: 'bool', boolValue: i % 2 === 0 }],
    })),
    false
  );

  const request = {
    limit: 2,
    maxExamined: 3,
    attrs: ['even'],
    predicates: [{
      kind: 'attr',
      op: 'eq',
      value: { name: 'even', kind: 'bool', boolValue: true },
    }],
  };
  const ids = [];
  let cursor;
  do {
    const page = await store.scan({ ...request, cursor });
    ids.push(...page.rows.map(({ id }) => id));
    cursor = page.next;
  } while (cursor);
  assert.deepEqual(ids, ['r0', 'r2', 'r4', 'r6']);

  const first = await store.scan({ ...request, limit: 1 });
  await assert.rejects(
    store.scan({ ...request, from: 'r2', cursor: first.next }),
    /cursor belongs to different bounds or predicates/
  );
});

test('publishes exact immutable cuts and reopens retained commits', async (t) => {
  const dir = temporaryStore(t);
  const store = await NativeStore.open(dir);
  t.after(async () => {
    try { await store.close(); } catch {}
  });

  await store.write([{ kind: 'put', id: 'r1' }]);
  const first = await store.snapshot();
  assert(first instanceof NativeSnapshot);
  t.after(async () => {
    try { await first.close(); } catch {}
  });
  assert(first.commit > 0n);
  assert.deepEqual((await first.scan()).rows.map(({ id }) => id), ['r1']);

  await store.write([{ kind: 'put', id: 'r2' }]);
  assert.deepEqual((await first.scan()).rows.map(({ id }) => id), ['r1']);
  // A separately opened reader sees only the manifest published by the first snapshot, not r2 in
  // the writer's WAL/memtable.
  const published = await NativeSnapshot.open(dir);
  assert.deepEqual((await published.scan()).rows.map(({ id }) => id), ['r1']);
  await published.close();

  const second = await store.snapshot();
  assert.deepEqual((await second.scan()).rows.map(({ id }) => id), ['r1', 'r2']);
  const commits = await retainedCommits(dir);
  assert(commits.includes(first.commit));
  assert(commits.includes(second.commit));

  const retained = await NativeSnapshot.openAt(dir, first.commit);
  assert.equal(retained.commit, first.commit);
  assert.deepEqual((await retained.scan()).rows.map(({ id }) => id), ['r1']);
  await retained.close();
  await assert.rejects(retained.scan(), /closed/);
  await second.close();
});

test('validates exact values and lifecycle at the boundary', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  await assert.rejects(
    store.write([{
      kind: 'put',
      id: 'bad',
      attrs: [{ name: 'wide', kind: 'int', intValue: 9223372036854775808n }],
    }]),
    (error) => {
      assert(error instanceof TurnDbError);
      assert.equal(error.code, 'INVALID_ARGUMENT');
      assert.match(error.message, /outside the signed i64 range/);
      return true;
    }
  );
  await assert.rejects(
    store.write([{
      kind: 'put',
      id: 'bad',
      attrs: [{ name: 'mixed', kind: 'bool', boolValue: true, stringValue: 'also' }],
    }]),
    /exactly one typed value/
  );
  await store.close(false);
  await assert.rejects(store.scan(), (error) => {
    assert(error instanceof TurnDbError);
    assert.equal(error.code, 'CLOSED');
    return true;
  });
  await assert.rejects(store.close(), (error) => {
    assert.equal(error.code, 'CLOSED');
    return true;
  });
});

test('classifies writer contention without parsing prose in the consumer', async (t) => {
  const dir = temporaryStore(t);
  const store = await NativeStore.open(dir);
  await assert.rejects(NativeStore.open(dir), (error) => {
    assert(error instanceof TurnDbError);
    assert.equal(error.code, 'CONTENTION');
    assert(error.cause instanceof Error);
    return true;
  });
  await store.close();
});

test('runs compaction verification and physical erasure through the actor', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  t.after(async () => {
    try { await store.close(); } catch {}
  });
  for (let part = 0; part < 3; part++) {
    await store.write([{
      kind: 'put',
      id: `r${part}`,
      contents: [{ name: 'payload', bytes: Buffer.from(`payload-${part}`) }],
    }]);
    assert.equal(await store.flush(), true);
  }

  const compact = await store.compact(true);
  assert.equal(compact.partsBefore, 3n);
  assert.equal(compact.partsAfter, 1n);
  assert.equal(compact.merge.inputs, 3n);
  assert.equal(compact.merge.recordsOut, 3n);
  assert.equal(compact.merge.foldBytesTouched, 0n);

  const verified = await store.verify();
  assert.equal(verified.parts, 1n);
  assert(verified.partSections > 0n);
  assert(verified.partDigests > 0n);
  assert(verified.foldBlocks > 0n);
  assert.equal(verified.trailingUncommittedBytes, 0n);

  const erased = await store.erase(['r1', 'never-existed']);
  assert.equal(erased.requested, 2n);
  assert.equal(erased.tombstoned, 1n);
  assert.equal(erased.absent, 1n);
  assert(erased.refold);
  assert.equal(erased.refold.recordsKept, 2n);
  assert.deepEqual((await store.scan()).rows.map(({ id }) => id), ['r0', 'r2']);
  assert.equal(await store.readContent('r1', 'payload'), null);

  const punched = await store.punch();
  assert.equal(typeof punched.blocksExamined, 'bigint');
  assert.equal(typeof punched.blocksPunched, 'bigint');
  const refolded = await store.refold();
  assert.equal(refolded.recordsKept, 2n);
  assert.equal(typeof refolded.bytesReclaimed, 'bigint');
  assert.equal((await store.verify()).parts, 1n);
});

test('reports cheap health across staging and publication', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  t.after(async () => {
    try { await store.close(); } catch {}
  });
  const empty = await store.health();
  assert.equal(empty.parts, 0n);
  assert.equal(empty.memtableEntries, 0n);
  assert.equal(empty.walBytes, 0n);

  await store.write([{
    kind: 'put',
    id: 'health/1',
    contents: [{ name: 'payload', bytes: Buffer.from('health payload') }],
  }]);
  const staged = await store.health();
  assert.equal(staged.memtableEntries, 1n);
  assert(staged.memtableBytes > 0n);
  assert(staged.walBytes > 0n);
  assert.equal(staged.dedupWindowEntries, 1n);

  await store.flush();
  const published = await store.health();
  assert(published.commit > empty.commit);
  assert.equal(published.parts, 1n);
  assert.equal(published.partRows, 1n);
  assert.equal(published.memtableEntries, 0n);
  assert.equal(published.walBytes, 0n);
  assert(published.foldDiskBytes > 0n);
  assert.equal(published.retainedCommits, 1n);
  assert.equal(typeof published.foldCacheHits, 'bigint');
  assert.equal(typeof published.partCacheBudget, 'bigint');
});

test('discovers typed field and content namespaces without reading values', async (t) => {
  const dir = temporaryStore(t);
  const store = await NativeStore.open(dir);
  t.after(async () => {
    try { await store.close(); } catch {}
  });

  await store.write([{
    kind: 'put',
    id: 'schema/1',
    contents: [{ name: 'request', bytes: Buffer.from('request body') }],
    attrs: [{ name: 'mixed', kind: 'string', stringValue: 'one' }],
  }]);
  assert.deepEqual(await store.schema(), {
    attributes: [{ name: 'mixed', types: ['string'] }],
    contents: ['request'],
    mayIncludeShadowedFields: false,
  });
  await store.flush();

  await store.write([{
    kind: 'put',
    id: 'schema/2',
    contents: [{ name: 'response', bytes: Buffer.from('response body') }],
    attrs: [
      { name: 'a', kind: 'bool', boolValue: true },
      { name: 'mixed', kind: 'int', intValue: 2n },
      { name: 'mixed', kind: 'float', floatValue: 3.5 },
    ],
  }]);

  const healthBefore = await store.health();
  assert.deepEqual(await store.schema(), {
    attributes: [
      { name: 'a', types: ['bool'] },
      { name: 'mixed', types: ['string', 'int', 'float'] },
    ],
    contents: ['request', 'response'],
    mayIncludeShadowedFields: true,
  });
  const healthAfter = await store.health();
  assert.equal(healthAfter.foldCacheHits, healthBefore.foldCacheHits);
  assert.equal(healthAfter.foldCacheMisses, healthBefore.foldCacheMisses);

  const published = await NativeSnapshot.open(dir);
  t.after(async () => {
    try { await published.close(); } catch {}
  });
  assert.deepEqual(await published.schema(), {
    attributes: [{ name: 'mixed', types: ['string'] }],
    contents: ['request'],
    mayIncludeShadowedFields: true,
  });
});
