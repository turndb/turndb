'use strict';

const assert = require('node:assert/strict');
const child = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const {
  capabilities, NativeSnapshot, NativeSqlQuery, NativeStore, recoverManifest, retainedCommits,
  restoreBackup, TurnDbError,
} = require('..');

function temporaryStore(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-native-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

test('reports the native capability profile without a portable fallback', () => {
  assert.deepEqual(capabilities(), {
    partFormatWrite: 4,
    partFormatReadMax: 4,
    writerExclusion: 'os_enforced',
    physicalErasure: process.platform === 'linux' ? 'punch_or_refold' : 'refold_only',
    positionedIo: true,
    threads: true,
    columnar: true,
    sql: true,
    portableWasm: false,
    nativeNode: true,
    napiVersion: 6,
    commandQueueCapacity: 64,
    commandQueueCapacityMax: 65536,
    writeAdmissionLimits: true,
    maxRecordBytesDefault: 67108864n,
    maxBatchBytesDefault: 268435456n,
    maxBatchRecordsDefault: 4096,
    maxIdentifierBytesDefault: 4096,
    immutableSnapshots: true,
    lifecycleOperations: true,
    backupRestore: true,
    recoveryControls: true,
    healthSnapshots: true,
    schemaDiscovery: true,
    scanCancellation: true,
    lifecycleCancellation: true,
    boundedCompaction: true,
    scanReconstructionBudget: true,
    scanReconstructedBytesDefault: 33554432n,
    arrowIpc: true,
    parameterizedSql: true,
    sqlMemoryBytesDefault: 268435456n,
    sqlAggregateMemoryBytesDefault: 1073741824n,
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

test('enforces configurable write admission with stable error classes', async (t) => {
  const recordBounded = await NativeStore.open(temporaryStore(t), {
    maxRecordBytes: 22n,
    maxBatchBytes: 100n,
    maxBatchRecords: 2,
    maxIdentifierBytes: 4,
  });
  await recordBounded.write([{ kind: 'put', id: 'x' }]);
  await assert.rejects(
    recordBounded.write([{ kind: 'put', id: 'xx' }]),
    (error) => error instanceof TurnDbError
      && error.code === 'RESOURCE_EXHAUSTED'
      && /worst-case WAL frame of 23 bytes/.test(error.message),
  );
  await assert.rejects(
    recordBounded.write([{ kind: 'delete', id: 'abcde' }]),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );
  await assert.rejects(
    recordBounded.write([
      { kind: 'delete', id: 'a' },
      { kind: 'delete', id: 'b' },
      { kind: 'delete', id: 'c' },
    ]),
    (error) => error instanceof TurnDbError && error.code === 'RESOURCE_EXHAUSTED',
  );
  await recordBounded.close();

  const batchBounded = await NativeStore.open(temporaryStore(t), {
    maxRecordBytes: 100n,
    maxBatchBytes: 53n,
  });
  await assert.rejects(
    batchBounded.write([{ kind: 'delete', id: 'a' }, { kind: 'delete', id: 'b' }]),
    (error) => error instanceof TurnDbError
      && error.code === 'RESOURCE_EXHAUSTED'
      && /representation of 54 bytes/.test(error.message),
  );
  await batchBounded.close();

  await assert.rejects(
    NativeStore.open(temporaryStore(t), { maxRecordBytes: 0n }),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );
});

test('bounds aggregate SQL reservations across a store and its snapshots', async (t) => {
  const limit = 48n << 20n;
  const perQuery = 32n << 20n;
  const store = await NativeStore.open(temporaryStore(t), {
    maxConcurrentSqlMemoryBytes: limit,
  });
  assert.equal(store.maxConcurrentSqlMemoryBytes, limit);
  await store.write([{ kind: 'put', id: 'one' }]);
  const snapshot = await store.snapshot();
  assert.equal(snapshot.maxConcurrentSqlMemoryBytes, limit);

  const first = await snapshot.querySql('SELECT id FROM records', undefined, {
    maxMemoryBytes: perQuery,
  });
  assert.equal(store.reservedSqlMemoryBytes, perQuery);
  assert.equal(snapshot.reservedSqlMemoryBytes, perQuery);
  await assert.rejects(
    store.querySql('SELECT id FROM records', undefined, { maxMemoryBytes: perQuery }),
    (error) => error instanceof TurnDbError && error.code === 'RESOURCE_EXHAUSTED',
  );
  await first.close();
  assert.equal(store.reservedSqlMemoryBytes, 0n);

  const afterRelease = await store.querySql('SELECT id FROM records', undefined, {
    maxMemoryBytes: perQuery,
  });
  await afterRelease.close();
  await snapshot.close();
  await store.close();

  await assert.rejects(
    NativeStore.open(temporaryStore(t), { maxConcurrentSqlMemoryBytes: 0n }),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
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
        { name: 'unsigned', kind: 'uint', uintValue: 18446744073709551615n },
        { name: 'binary', kind: 'binary', binaryValue: Buffer.from([0, 0xff, 0x80]) },
        { name: 'at', kind: 'timestamp_ns', timestampNsValue: -9223372036854775808n },
        { name: 'nothing', kind: 'null' },
      ],
    }],
    true
  );

  const page = await store.scan({
    attrs: ['tag', 'minimum', 'nan', 'sampled', 'unsigned', 'binary', 'at', 'nothing'],
    contents: [
      { name: 'request', mode: 'metadata' },
      { name: 'response', mode: 'bytes' },
      { name: 'empty', mode: 'bytes' },
      { name: 'absent', mode: 'bytes' },
    ],
  });
  assert.equal(page.rows.length, 1);
  assert.deepEqual(page.rows[0].attrs.map(({ name }) => name), [
    'tag', 'tag', 'minimum', 'nan', 'sampled', 'unsigned', 'binary', 'at', 'nothing',
  ]);
  assert.equal(page.rows[0].attrs[2].intValue, -9223372036854775808n);
  assert(Number.isNaN(page.rows[0].attrs[3].floatValue));
  assert.equal(page.rows[0].attrs[5].uintValue, 18446744073709551615n);
  assert.deepEqual(page.rows[0].attrs[6].binaryValue, Buffer.from([0, 0xff, 0x80]));
  assert.equal(page.rows[0].attrs[7].timestampNsValue, -9223372036854775808n);
  assert.equal(page.rows[0].attrs[8].kind, 'null');
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

test('bounds reconstructed scan bytes without splitting or skipping rows', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  t.after(async () => {
    try { await store.close(); } catch {}
  });
  await store.write(
    ['a', 'b', 'c'].map((id) => ({
      kind: 'put',
      id,
      contents: [{ name: 'payload', bytes: Buffer.from('123456') }],
    }))
  );

  const request = {
    contents: [{ name: 'payload', mode: 'bytes' }],
    maxReconstructedBytes: 10n,
  };
  const first = await store.scan(request);
  assert.deepEqual(first.rows.map(({ id }) => id), ['a']);
  assert.equal(first.stats.reconstructedBytes, 6n);
  assert.equal(first.stats.reconstructionBudgetExhausted, true);
  assert.equal(first.stats.examined, 2);

  const second = await store.scan({ ...request, cursor: first.next });
  assert.deepEqual(second.rows.map(({ id }) => id), ['b']);
  assert.equal(second.stats.reconstructionBudgetExhausted, true);

  const third = await store.scan({ ...request, cursor: second.next });
  assert.deepEqual(third.rows.map(({ id }) => id), ['c']);
  assert.equal(third.stats.reconstructionBudgetExhausted, false);
  assert.equal(third.next, undefined);

  await assert.rejects(
    store.scan({ maxReconstructedBytes: 0n }),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT'
  );
});

test('streams bounded parameterized read-only SQL as Arrow IPC', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  t.after(async () => {
    try { await store.close(); } catch {}
  });
  await store.write([
    {
      kind: 'put', id: 'a',
      attrs: [
        { name: 'kind', kind: 'string', stringValue: 'keep' },
        { name: 'tokens', kind: 'int', intValue: 1n },
      ],
    },
    {
      kind: 'put', id: 'b',
      attrs: [
        { name: 'kind', kind: 'string', stringValue: 'drop' },
        { name: 'tokens', kind: 'int', intValue: 2n },
      ],
    },
    {
      kind: 'put', id: 'c',
      attrs: [
        { name: 'kind', kind: 'string', stringValue: 'keep' },
        { name: 'tokens', kind: 'int', intValue: 3n },
        { name: 'u', kind: 'uint', uintValue: 18446744073709551615n },
        { name: 'raw', kind: 'binary', binaryValue: Buffer.from([0, 255]) },
        { name: 'at', kind: 'timestamp_ns', timestampNsValue: -1n },
      ],
    },
  ]);

  // Writer SQL takes and publishes an exact actor-ordered snapshot, so accepted unflushed rows are
  // included and query execution no longer occupies the writer actor.
  const query = await store.querySql(
    'SELECT id, tokens FROM records WHERE kind = $1 AND tokens > $2 AND u = $3 AND raw = $4 AND at = $5 ORDER BY id',
    [
      { kind: 'string', stringValue: 'keep' },
      { kind: 'int', intValue: 1n },
      { kind: 'uint', uintValue: 18446744073709551615n },
      { kind: 'binary', binaryValue: Buffer.from([0, 255]) },
      { kind: 'timestamp_ns', timestampNsValue: -1n },
    ],
    { maxMemoryBytes: 32n << 20n }
  );
  assert(query instanceof NativeSqlQuery);
  assert(Buffer.isBuffer(query.schemaIpc));
  assert.equal(query.schemaIpc.readUInt32LE(0), 0xffffffff);
  assert.equal((await store.health()).memtableEntries, 0n);

  const batch = await query.next();
  assert.equal(batch.rows, 1);
  assert(Buffer.isBuffer(batch.ipc));
  assert.equal(batch.ipc.readUInt32LE(0), 0xffffffff);
  assert(batch.stats.rows > 0n);
  assert.equal(await query.next(), null);
  assert.deepEqual(await query.stats(), batch.stats);

  const snapshot = await store.snapshot();
  const forbidden = snapshot.querySql('CREATE TABLE forbidden (value INT)');
  await assert.rejects(
    forbidden,
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT'
  );

  const starved = await snapshot.querySql(
    'SELECT id FROM records ORDER BY id',
    undefined,
    { maxMemoryBytes: 1n << 20n }
  );
  await assert.rejects(
    starved.next(),
    (error) => error instanceof TurnDbError && error.code === 'RESOURCE_EXHAUSTED'
  );

  const timed = await snapshot.querySql('SELECT id FROM records');
  await assert.rejects(
    timed.next({ timeoutMs: 0 }),
    (error) => error instanceof TurnDbError && error.code === 'CANCELLED'
  );

  const preAborted = new AbortController();
  preAborted.abort();
  const cancelled = await snapshot.querySql('SELECT id FROM records');
  await assert.rejects(
    cancelled.next({ signal: preAborted.signal }),
    (error) => error instanceof TurnDbError && error.code === 'CANCELLED'
  );
  assert.equal(await cancelled.next(), null);

  await assert.rejects(
    snapshot.querySql('SELECT id FROM records', undefined, { maxMemoryBytes: 0n }),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT'
  );
  const closed = await snapshot.querySql('SELECT id FROM records');
  await closed.close();
  assert.equal(await closed.next(), null);
  await query.close();
  await snapshot.close();
});

test('enforces deterministic scan deadlines and pre-aborted signals', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  t.after(async () => {
    try { await store.close(); } catch {}
  });

  await assert.rejects(
    store.scan({ timeoutMs: 0 }),
    (error) => error instanceof TurnDbError
      && error.code === 'CANCELLED'
      && /deadline exceeded/.test(error.message)
  );

  const alreadyAborted = new AbortController();
  alreadyAborted.abort();
  await assert.rejects(
    store.scan({ signal: alreadyAborted.signal }),
    (error) => error instanceof TurnDbError && error.code === 'CANCELLED'
  );

  // A core Source test cancels after its first record read and proves in-flight work discards its
  // partial page. An empty native scan may correctly finish before an abort issued after submission,
  // so the ABI test intentionally avoids asserting a scheduler race.
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

test('backs up an actor-ordered cut and safely restores a writable store', async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-native-backup-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const dir = path.join(root, 'store');
  const artifact = path.join(root, 'snapshot.turndb');
  const restoredDir = path.join(root, 'restored');
  const store = await NativeStore.open(dir);
  t.after(async () => {
    try { await store.close(); } catch {}
  });

  await store.write([{ kind: 'put', id: 'before' }]);
  const backup = await store.backup(artifact);
  assert(backup.files >= 3n);
  assert.equal(backup.bytes, BigInt(fs.statSync(artifact).size));
  assert(backup.commit > 0n);

  await store.write([{ kind: 'put', id: 'after' }], true);
  const restored = await restoreBackup(artifact, restoredDir);
  assert.deepEqual(restored, backup);
  const restoredStore = await NativeStore.open(restoredDir);
  assert.deepEqual((await restoredStore.scan()).rows.map(({ id }) => id), ['before']);
  await restoredStore.write([{ kind: 'put', id: 'restored-write' }], true);
  assert.deepEqual(
    (await restoredStore.scan()).rows.map(({ id }) => id),
    ['before', 'restored-write'],
  );
  await restoredStore.close();

  await assert.rejects(
    store.backup(artifact),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );
  await assert.rejects(
    restoreBackup(artifact, restoredDir),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );

  const corrupt = Buffer.from(fs.readFileSync(artifact));
  corrupt[0] ^= 1;
  const corruptPath = path.join(root, 'corrupt.turndb');
  const absent = path.join(root, 'corrupt-restore');
  fs.writeFileSync(corruptPath, corrupt);
  await assert.rejects(
    restoreBackup(corruptPath, absent),
    (error) => error instanceof TurnDbError && error.code === 'CORRUPTION',
  );
  assert.equal(fs.existsSync(absent), false);
  await assert.rejects(
    restoreBackup(path.join(root, 'missing.turndb'), absent),
    (error) => error instanceof TurnDbError && error.code === 'NOT_FOUND',
  );
});

test('recovers only a fully validated retained manifest under writer exclusion', async (t) => {
  const dir = temporaryStore(t);
  let store = await NativeStore.open(dir);
  await store.write([{
    kind: 'put',
    id: 'survives',
    contents: [{ name: 'payload', bytes: Buffer.from('content validated during recovery') }],
  }]);
  await store.flush();
  await store.close();

  const manifestPath = path.join(dir, 'MANIFEST');
  const damageManifest = () => {
    const bytes = Buffer.from(fs.readFileSync(manifestPath));
    bytes[10] ^= 0xff;
    fs.writeFileSync(manifestPath, bytes);
  };
  damageManifest();
  const report = await recoverManifest(dir);
  assert.equal(report.rollbackCommits, 0n);
  assert.equal(report.records, 1n);
  assert.equal(report.contentValues, 1n);
  assert(report.partSections > 0n);

  const snapshot = await NativeSnapshot.open(dir);
  assert.deepEqual((await snapshot.scan()).rows.map(({ id }) => id), ['survives']);
  await snapshot.close();
  await assert.rejects(
    recoverManifest(dir),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );

  store = await NativeStore.open(dir);
  damageManifest();
  await assert.rejects(
    recoverManifest(dir),
    (error) => error instanceof TurnDbError && error.code === 'CONTENTION',
  );
  await store.close(false);
  await recoverManifest(dir);
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
      id: 'bad-uint',
      attrs: [{ name: 'wide', kind: 'uint', uintValue: 18446744073709551616n }],
    }]),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );
  await assert.rejects(
    store.write([{
      kind: 'put',
      id: 'bad-time',
      attrs: [{ name: 'at', kind: 'timestamp_ns', timestampNsValue: 9223372036854775808n }],
    }]),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );
  await assert.rejects(
    store.write([{
      kind: 'put',
      id: 'bad-null',
      attrs: [{ name: 'none', kind: 'null', boolValue: false }],
    }]),
    /except null carries none/,
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

test('compacts one exact bounded work unit and classifies budgets for schedulers', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  t.after(async () => {
    try { await store.close(); } catch {}
  });
  for (let part = 0; part < 3; part++) {
    await store.write([{
      kind: 'put',
      id: `bounded/${part}`,
      contents: [{ name: 'payload', bytes: Buffer.from(`bounded payload ${part}`) }],
    }]);
    await store.flush();
  }

  const result = await store.compactBounded({
    maxInputParts: 2,
    maxInputRows: 2n,
    maxInputBytes: 1n << 40n,
  });
  assert.equal(result.partsBefore, 3n);
  assert.equal(result.partsAfter, 2n);
  assert.deepEqual(result.plan, {
    startPart: 0n,
    inputParts: 2n,
    inputRows: 2n,
    inputBytes: result.plan.inputBytes,
    dropsTombstones: false,
  });
  assert(result.plan.inputBytes > 0n);
  assert(result.outputBytes > 0n);
  assert.equal(result.merge.inputs, 2n);

  await assert.rejects(
    store.compactBounded({ maxInputParts: 2, maxInputRows: 1n, maxInputBytes: 1n << 40n }),
    (error) => error instanceof TurnDbError && error.code === 'RESOURCE_EXHAUSTED',
  );
  assert.throws(
    () => store.compactBounded({ maxInputParts: 1, maxInputRows: 10n, maxInputBytes: 1n << 40n }),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );
  await assert.rejects(
    store.compactBounded(
      { maxInputParts: 2, maxInputRows: 10n, maxInputBytes: 1n << 40n },
      { timeoutMs: 0 },
    ),
    (error) => error instanceof TurnDbError && error.code === 'CANCELLED',
  );

  const settled = await store.compactBounded({
    maxInputParts: 2,
    maxInputRows: 10n,
    maxInputBytes: 1n << 40n,
  });
  assert.equal(settled.partsAfter, 1n);
  assert.equal(settled.plan.dropsTombstones, true);
  const idle = await store.compactBounded({
    maxInputParts: 2,
    maxInputRows: 10n,
    maxInputBytes: 1n << 40n,
  });
  assert.equal(idle.partsBefore, 1n);
  assert.equal(idle.partsAfter, 1n);
  assert.equal(idle.plan, undefined);
  assert.equal(idle.outputBytes, undefined);
  assert.equal(idle.merge, undefined);
});

test('lifecycle deadlines and aborts refuse at safe pre-mutation checkpoints', async (t) => {
  const store = await NativeStore.open(temporaryStore(t));
  t.after(async () => {
    try { await store.close(); } catch {}
  });
  await store.write([{
    kind: 'put',
    id: 'still-present',
    contents: [{ name: 'payload', bytes: Buffer.from('must survive refusal') }],
  }]);

  const cancelled = (promise) => assert.rejects(
    promise,
    (error) => error instanceof TurnDbError && error.code === 'CANCELLED',
  );
  await cancelled(store.compact(true, { timeoutMs: 0 }));
  await cancelled(store.verify({ timeoutMs: 0 }));
  await cancelled(store.punch({ timeoutMs: 0 }));
  await cancelled(store.refold({ timeoutMs: 0 }));
  await cancelled(store.erase(['still-present'], { timeoutMs: 0 }));

  const aborted = new AbortController();
  aborted.abort();
  await cancelled(store.verify({ signal: aborted.signal }));
  assert.deepEqual((await store.scan()).rows.map(({ id }) => id), ['still-present']);
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
    attrs: [
      { name: 'mixed', kind: 'string', stringValue: 'one' },
      { name: 'unsigned', kind: 'uint', uintValue: 1n },
      { name: 'binary', kind: 'binary', binaryValue: Buffer.from([1]) },
      { name: 'at', kind: 'timestamp_ns', timestampNsValue: 2n },
      { name: 'nothing', kind: 'null' },
    ],
  }]);
  assert.deepEqual(await store.schema(), {
    attributes: [
      { name: 'at', types: ['timestamp_ns'] },
      { name: 'binary', types: ['binary'] },
      { name: 'mixed', types: ['string'] },
      { name: 'nothing', types: ['null'] },
      { name: 'unsigned', types: ['uint'] },
    ],
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
      { name: 'at', types: ['timestamp_ns'] },
      { name: 'binary', types: ['binary'] },
      { name: 'mixed', types: ['string', 'int', 'float'] },
      { name: 'nothing', types: ['null'] },
      { name: 'unsigned', types: ['uint'] },
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
    attributes: [
      { name: 'at', types: ['timestamp_ns'] },
      { name: 'binary', types: ['binary'] },
      { name: 'mixed', types: ['string'] },
      { name: 'nothing', types: ['null'] },
      { name: 'unsigned', types: ['uint'] },
    ],
    contents: ['request'],
    mayIncludeShadowedFields: true,
  });
});
