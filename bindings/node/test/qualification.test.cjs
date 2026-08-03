'use strict';

const assert = require('node:assert/strict');
const child = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { NativeStore, retainedCommits, restoreBackup } = require('..');
const { putRecord } = require('../qualification/record-adapter.cjs');
const { runSoak } = require('../qualification/soak.cjs');
const { buildPipeline, linkedTelemetry } = require('../qualification/workloads.cjs');

function temporaryRoot(t, prefix = 'turndb-qualification-') {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  return root;
}

function attrPredicate(field) {
  const value = putRecord({ id: 'predicate', fields: [field] }).attrs[0];
  return { kind: 'attr', op: 'eq', value };
}

function materializeHexFixture(source, destination) {
  const hex = fs.readFileSync(source, 'utf8').replaceAll(/\s/g, '');
  assert.match(hex, /^(?:[0-9a-f]{2})+$/);
  const bytes = Buffer.from(hex, 'hex');
  assert.equal(bytes.toString('hex'), hex, 'fixture decoding must not truncate invalid hex');
  fs.writeFileSync(destination, bytes);
}

async function qualifyWorkload(t, fixture) {
  const dir = path.join(temporaryRoot(t), 'store');
  let store = await NativeStore.open(dir);
  t.after(async () => {
    try { await store.close(); } catch {}
  });

  // One call is one ordered atomic source batch. `durable` is the acknowledgement boundary.
  await store.write(fixture.records.map(putRecord), true);
  assert.equal(await store.flush(), true);

  const schema = await store.schema();
  const expectedAttributeTypes = new Map();
  for (const record of fixture.records) {
    for (const field of record.fields ?? []) {
      if (!expectedAttributeTypes.has(field.name)) expectedAttributeTypes.set(field.name, new Set());
      expectedAttributeTypes.get(field.name).add(field.type);
    }
  }
  const canonicalAttributes = (attributes) => attributes
    .map(({ name, types }) => ({ name, types: [...types].sort() }))
    .sort((left, right) => left.name.localeCompare(right.name));
  assert.deepEqual(
    canonicalAttributes(schema.attributes),
    canonicalAttributes([...expectedAttributeTypes].map(([name, types]) => ({ name, types }))),
  );
  const expectedContentNames = [...new Set(fixture.records.flatMap(
    (record) => (record.contents ?? []).map(({ name }) => name),
  ))].sort();
  assert.deepEqual(schema.contents, expectedContentNames);

  const attributeNames = [...expectedAttributeTypes.keys()].sort();
  const completeMetadata = await store.scan({ attrs: attributeNames });
  const expectedRecords = [...fixture.records].sort((left, right) => left.id.localeCompare(right.id));
  assert.deepEqual(
    completeMetadata.rows.map(({ id, attrs }) => ({ id, attrs })),
    expectedRecords.map((record) => {
      const { id, attrs } = putRecord(record);
      return { id, attrs };
    }),
  );
  assert.equal(completeMetadata.stats.io.foldBlocksTouched, 0n);

  const correlated = await store.scan({
    attrs: ['record.family', fixture.correlation.name],
    predicates: [attrPredicate(fixture.correlation)],
  });
  assert.deepEqual(correlated.rows.map(({ id }) => id), fixture.expectedCorrelatedIds);
  assert.equal(correlated.stats.io.foldBlocksTouched, 0n);
  assert(correlated.rows.every(({ contents }) => contents.length === 0));

  const sharedRows = [];
  for (const shared of fixture.shared) {
    const page = await store.scan({
      from: shared.id,
      to: `${shared.id}\0`,
      contents: [{ name: shared.content, mode: 'metadata' }],
    });
    assert.equal(page.stats.io.foldBlocksTouched, 0n);
    assert.equal(page.rows.length, 1);
    assert.equal(page.rows[0].contents.length, 1);
    assert.equal(page.rows[0].contents[0].present, true);
    assert.equal(page.rows[0].contents[0].bytes, undefined);
    sharedRows.push(page.rows[0].contents[0]);
  }
  assert.match(sharedRows[0].identity, /^[0-9a-f]{64}$/);
  assert.equal(sharedRows[1].identity, sharedRows[0].identity);
  assert.equal(sharedRows[1].len, sharedRows[0].len);

  const selected = await store.scan({
    from: fixture.selected.id,
    to: `${fixture.selected.id}\0`,
    contents: [{ name: fixture.selected.name, mode: 'bytes' }],
  });
  assert.equal(selected.rows.length, 1);
  assert.equal(selected.rows[0].contents.length, 1);
  assert.deepEqual(selected.rows[0].contents[0].bytes, fixture.selected.bytes);

  const expectedIds = expectedRecords.map(({ id }) => id);
  await store.close(false);
  store = await NativeStore.open(dir);
  assert.deepEqual((await store.scan()).rows.map(({ id }) => id), expectedIds);
}

for (const fixture of [linkedTelemetry, buildPipeline]) {
  test(`qualifies a self-described ${fixture.name} consumer`, async (t) => {
    await qualifyWorkload(t, fixture);
  });
}

test('defines live cursor and late-arrival behavior without a trace-specific timeline API', async (t) => {
  const dir = path.join(temporaryRoot(t), 'store');
  const store = await NativeStore.open(dir);
  t.after(async () => {
    try { await store.close(); } catch {}
  });
  const record = (id, occurredAt) => putRecord({
    id,
    fields: [
      { name: 'stream.key', type: 'string', value: 'stream-a' },
      { name: 'occurred_at', type: 'timestamp_ns', value: occurredAt },
    ],
  });
  await store.write([
    record('stream-a/0001', 1000n),
    record('stream-a/0003', 3000n),
    record('stream-a/0005', 5000n),
  ], true);

  const request = { from: 'stream-a/', to: 'stream-a0', limit: 1, attrs: ['occurred_at'] };
  const first = await store.scan(request);
  assert.deepEqual(first.rows.map(({ id }) => id), ['stream-a/0001']);

  // A live cursor is a checked keyset continuation, not a snapshot: new keys after it are visible;
  // new keys before it are not replayed. Consumers wanting a stable cut use snapshot().
  await store.write([
    record('stream-a/0000-late', 500n),
    record('stream-a/0002-late', 750n),
    record('stream-a/0004-late', 250n),
  ], true);
  const ids = [...first.rows.map(({ id }) => id)];
  let cursor = first.next;
  while (cursor) {
    const page = await store.scan({ ...request, cursor });
    ids.push(...page.rows.map(({ id }) => id));
    cursor = page.next;
  }
  assert.deepEqual(ids, [
    'stream-a/0001',
    'stream-a/0002-late',
    'stream-a/0003',
    'stream-a/0004-late',
    'stream-a/0005',
  ]);

  const complete = await store.scan({ ...request, limit: 100 });
  assert.deepEqual(complete.rows.map(({ id }) => id), [
    'stream-a/0000-late',
    'stream-a/0001',
    'stream-a/0002-late',
    'stream-a/0003',
    'stream-a/0004-late',
    'stream-a/0005',
  ]);
  assert.equal(complete.rows[0].attrs[0].timestampNsValue, 500n);
  assert.equal(complete.rows[4].attrs[0].timestampNsValue, 250n);
});

test('recovers an atomically acknowledged consumer batch after process exit', async (t) => {
  const dir = path.join(temporaryRoot(t, 'turndb-qualification-crash-'), 'store');
  const writer = path.resolve(__dirname, '../qualification/crash-writer.cjs');
  const exited = child.spawnSync(process.execPath, [writer, dir], {
    env: process.env,
    encoding: 'utf8',
  });
  assert.equal(exited.status, 0, exited.stderr);

  const store = await NativeStore.open(dir);
  t.after(async () => {
    try { await store.close(); } catch {}
  });
  const page = await store.scan({
    attrs: ['batch.key'],
    contents: [{ name: 'payload', mode: 'bytes' }],
  });
  assert.deepEqual(page.rows.map(({ id }) => id), ['crash/0001/first', 'crash/0002/second']);
  assert(page.rows.every(({ attrs }) => attrs[0].stringValue === 'atomic-1'));
  assert.deepEqual(
    page.rows.map(({ contents }) => contents[0].bytes.toString()),
    ['first durable value', 'second durable value'],
  );
});

test('qualifies retention, compaction, backup, restore, and physical erasure as one workflow', async (t) => {
  const root = temporaryRoot(t, 'turndb-qualification-maintenance-');
  const dir = path.join(root, 'store');
  const artifact = path.join(root, 'before-erasure.turndb');
  const restoredDir = path.join(root, 'restored');
  const shared = Buffer.from('shared immutable source context');
  const erasedPayload = Buffer.alloc(64 << 10, 0xa5);
  const store = await NativeStore.open(dir, {
    blockTargetBytes: 8n << 10n,
    compressionLevel: 3,
  });
  let restoredStore;
  t.after(async () => {
    try { await restoredStore?.close(); } catch {}
    try { await store.close(); } catch {}
  });

  const expectedIds = [];
  for (let cycle = 0; cycle < 8; cycle++) {
    const id = `retention/${cycle.toString().padStart(4, '0')}`;
    expectedIds.push(id);
    await store.write([putRecord({
      id,
      fields: [
        { name: 'retention.group', type: 'string', value: 'group-a' },
        { name: 'retention.cycle', type: 'uint', value: BigInt(cycle) },
      ],
      contents: [
        { name: 'shared', bytes: shared },
        { name: 'payload', bytes: cycle === 3 ? erasedPayload : Buffer.from(`payload-${cycle}`) },
      ],
    })], true);
    assert.equal(await store.flush(), true);
  }

  assert.equal((await store.health()).parts, 8n);
  const retainedBeforeCompaction = await retainedCommits(dir);
  assert.equal(retainedBeforeCompaction.length, 4);
  assert(retainedBeforeCompaction.every((commit, index, commits) => (
    index === 0 || commits[index - 1] < commit
  )));

  const compacted = await store.compact(true);
  assert.equal(compacted.partsBefore, 8n);
  assert.equal(compacted.partsAfter, 1n);
  assert.equal(compacted.merge.inputs, 8n);
  assert.equal(compacted.merge.recordsOut, 8n);
  assert.equal(compacted.merge.foldBytesTouched, 0n);
  const distribution = await store.partDistribution();
  assert.equal(distribution.parts, 1n);
  assert.equal(distribution.totalRows, 8n);
  assert.equal((await store.verify()).parts, 1n);

  const backup = await store.backup(artifact);
  assert.equal(backup.bytes, BigInt(fs.statSync(artifact).size));
  assert.equal(backup.commit, (await store.health()).commit);
  assert.deepEqual((await store.scan()).rows.map(({ id }) => id), expectedIds);

  const restored = await restoreBackup(artifact, restoredDir);
  assert.deepEqual(restored, backup);
  restoredStore = await NativeStore.open(restoredDir);
  assert.deepEqual((await restoredStore.scan()).rows.map(({ id }) => id), expectedIds);
  assert.deepEqual(await restoredStore.readContent('retention/0003', 'payload'), erasedPayload);
  await restoredStore.write([putRecord({
    id: 'retention/restored-only',
    fields: [{ name: 'retention.group', type: 'string', value: 'group-b' }],
  })], true);
  assert.deepEqual(
    (await restoredStore.scan()).rows.map(({ id }) => id),
    [...expectedIds, 'retention/restored-only'],
  );

  const erased = await store.erase(['retention/0003']);
  assert.equal(erased.requested, 1n);
  assert.equal(erased.tombstoned, 1n);
  assert.equal(erased.absent, 0n);
  assert(erased.refold);
  assert.equal(erased.refold.recordsKept, 7n);
  assert.equal(await store.readContent('retention/0003', 'payload'), null);
  assert.deepEqual(
    (await store.scan()).rows.map(({ id }) => id),
    expectedIds.filter((id) => id !== 'retention/0003'),
  );
  const liveCommit = (await store.health()).commit;
  assert.deepEqual(await retainedCommits(dir), [liveCommit]);
  const liveness = await store.contentLiveness();
  assert.equal(liveness.deadLogicalBytes, 0n);
  assert.equal(liveness.reclaimableBlocks.rawBytes, 0n);
  assert.equal((await store.verify()).parts, 1n);

  // Erasure is scoped to this store. A backup made before it is an external copy by definition.
  assert.deepEqual(await restoredStore.readContent('retention/0003', 'payload'), erasedPayload);
});

test('upgrades a checked revision-three consumer artifact one restartable part at a time', async (t) => {
  const root = temporaryRoot(t, 'turndb-qualification-upgrade-');
  const artifact = path.join(root, 'revision-three.turndb');
  const dir = path.join(root, 'store');
  materializeHexFixture(
    path.resolve(__dirname, '../qualification/fixtures/revision-three.turndb.hex'),
    artifact,
  );
  await restoreBackup(artifact, dir);
  let store = await NativeStore.open(dir);
  t.after(async () => {
    try { await store.close(); } catch {}
  });

  const expected = new Map([
    ['legacy/0001', Buffer.from('revision three request')],
    ['legacy/0002', Buffer.from('revision three response')],
  ]);
  const contentState = async () => {
    const page = await store.scan({ contents: [{ name: 'payload', mode: 'metadata' }] });
    assert.equal(page.stats.io.foldBlocksTouched, 0n);
    return new Map(page.rows.map(({ id, contents }) => {
      assert.equal(contents.length, 1);
      assert.equal(contents[0].present, true);
      assert.match(contents[0].identity, /^[0-9a-f]{64}$/);
      return [id, { identity: contents[0].identity, len: contents[0].len }];
    }));
  };
  const beforeContent = await contentState();
  for (const [id, bytes] of expected) {
    assert.deepEqual(await store.readContent(id, 'payload'), bytes);
    assert.equal(beforeContent.get(id).len, BigInt(bytes.length));
  }
  const before = await store.formatMigrationStatus();
  assert.equal(before.targetPartVersion, 4);
  assert.equal(before.liveParts, 2n);
  assert.equal(before.currentParts, 0n);
  assert.equal(before.legacyParts, 2n);
  assert.equal(before.legacyRows, 2n);

  const preflight = await store.estimateFormatMigrationSpace();
  assert.equal(preflight.flushed, false);
  assert.equal(preflight.status.legacyParts, 2n);
  assert.equal(preflight.estimate.sourcePartVersion, 3);
  assert.equal(preflight.estimate.inputRows, 1n);
  assert.equal(preflight.estimate.estimateIsHardBound, false);
  const first = await store.migrateFormatStep();
  assert.equal(first.flushed, false);
  assert.equal(first.step.plan.sourcePartVersion, 3);
  assert.equal(first.step.remainingLegacyParts, 1n);
  assert.equal(first.step.rewrite.inputs, 1n);

  await store.close(false);
  store = await NativeStore.open(dir);
  const midway = await store.formatMigrationStatus();
  assert.equal(midway.currentParts, 1n);
  assert.equal(midway.legacyParts, 1n);
  // Packs contain one committed snapshot, not its source store's retained history. The first
  // migration therefore has no retained-only legacy input yet.
  assert.equal(midway.retainedLegacyParts, 0n);
  const second = await store.migrateFormatStep();
  assert.equal(second.step.remainingLegacyParts, 0n);
  assert.equal((await store.migrateFormatStep()).step, undefined);

  const after = await store.formatMigrationStatus();
  assert.equal(after.currentParts, 2n);
  assert.equal(after.legacyParts, 0n);
  assert.equal(after.retainedLegacyParts, 1n);
  const afterContent = await contentState();
  assert.deepEqual(afterContent, beforeContent);
  for (const [id, bytes] of expected) {
    assert.deepEqual(await store.readContent(id, 'payload'), bytes);
  }
  const verified = await store.verify();
  assert.equal(verified.parts, 2n);
  assert.equal(verified.trailingUncommittedBytes, 0n);
});

test('runs the bounded sustained-ingestion qualification profile', async (t) => {
  const dir = path.join(temporaryRoot(t, 'turndb-qualification-soak-'), 'store');
  const report = await runSoak({
    dir,
    cycles: 64,
    recordsPerCycle: 8,
    payloadBytes: 2048,
    compactEvery: 8,
    restartEvery: 16,
  });
  assert.equal(report.cycles, 64);
  assert.equal(report.acceptedOps, 702);
  assert.equal(report.liveRecords, 522);
  assert.equal(report.boundedCompactions, 15);
  assert.equal(report.restarts, 3);
  assert(BigInt(report.partHighWater) <= 9n);
  assert(BigInt(report.deadBytesBeforeRefold) > 0n);
  assert(BigInt(report.foldBytesAfterRefold) < BigInt(report.foldBytesBeforeRefold));
  assert(Number.isFinite(report.durationMs));
  assert(report.durationMs > 0);
});
