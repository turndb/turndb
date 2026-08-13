import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

import './_artifact.mjs';
import { open } from '../index.mjs';

async function snapshot(store) {
  const page = store.scan({
    limit: 1_000,
    contents: [{ name: 'body', mode: 'bytes' }],
  });
  return new Map(page.rows.map((row) => [row.id, {
    identity: row.contents[0].identity,
    bytes: Buffer.from(row.contents[0].bytes),
  }]));
}

test('retention reports query absence, reclamation, and audit evidence separately', async () => {
  const dir = join(await mkdtemp(join(tmpdir(), 'turndb-retention-cutoff-')), 's.turndb');
  const cutoff = 'trace/2026-08-05T09:00:00Z';
  const records = [
    ['trace/2026-08-05T08:00:00Z/a', 'dead-a'],
    ['trace/2026-08-05T08:30:00Z/b', 'dead-b'],
    ['trace/2026-08-05T09:00:00Z/c', 'live-c'],
    ['trace/2026-08-05T09:30:00Z/d', 'live-d'],
  ];
  let store = await open(dir);
  try {
    store.write(records.map(([id, bytes]) => ({
      kind: 'put', id, contents: [{ name: 'body', bytes }], attrs: [],
    })), { durable: true });
    store.flush();
    const before = await snapshot(store);
    assert.equal(before.size, 4);

    const targeted = [...before.keys()].filter((id) => id < cutoff);
    assert.deepEqual(targeted, records.slice(0, 2).map(([id]) => id));
    const result = store.eraseIds([...targeted, 'trace/2026-08-05T07:00:00Z/already-absent']);
    assert.deepEqual(
      { requested: result.requested, erased: result.erased, absent: result.absent, remaining: result.remaining },
      { requested: 3, erased: 2, absent: 1, remaining: 2 },
      'accounting must distinguish targeted records from ids already absent',
    );
    assert.equal(result.reclamation.state, 'measured');
    assert.ok(result.reclamation.logicalBytes > 0n);
    assert.deepEqual(result.reclamation.allocatedBytes, { state: 'absent' },
      'WASI must not fabricate allocated bytes or encode unavailable as zero');

    const after = await snapshot(store);
    assert.deepEqual([...after.keys()], records.slice(2).map(([id]) => id));
    for (const [id, expected] of after) {
      assert.equal(expected.identity, before.get(id).identity, `${id} identity changed`);
      assert.deepEqual(expected.bytes, before.get(id).bytes, `${id} bytes changed`);
    }
    for (const id of targeted) assert.equal(store.get(id), null, `${id} remained queryable`);

    const event = store.lifecycleEvents().events.findLast((candidate) => candidate.operation === 'erase');
    assert.equal(event?.outcome, 'succeeded', 'the erase must publish an auditable lifecycle event');

    store.close();
    store = await open(dir);
    const reopened = await snapshot(store);
    assert.deepEqual([...reopened.keys()], [...after.keys()]);
    for (const [id, expected] of after) {
      assert.equal(reopened.get(id).identity, expected.identity);
      assert.deepEqual(reopened.get(id).bytes, expected.bytes);
    }
  } finally {
    try { store.close(); } catch {}
    await rm(dir, { recursive: true, force: true });
  }
});

test('refold is explicitly preflighted and reports logical output without claiming media erasure', async () => {
  const dir = join(await mkdtemp(join(tmpdir(), 'turndb-retention-refold-')), 's.turndb');
  const store = await open(dir);
  try {
    store.write([{
      kind: 'put', id: 'same', contents: [{ name: 'body', bytes: Buffer.alloc(65_536, 0x41) }], attrs: [],
    }], { durable: true });
    store.flush();
    store.write([{
      kind: 'put', id: 'same', contents: [{ name: 'body', bytes: Buffer.alloc(65_536, 0x42) }], attrs: [],
    }], { durable: true });
    store.flush();

    const preflight = store.estimateRefoldSpace();
    assert.equal(preflight.estimateIsHardBound, false);
    assert.ok(preflight.estimatedStageBytes > 0n);
    assert.deepEqual(preflight.filesystemAvailableBytes, { state: 'absent' });

    const result = store.refold();
    assert.equal(result.piecesDropped, 1, 'dead content must not be carried into the new fold');
    assert.equal(result.reclamation.state, 'measured');
    assert.equal(
      result.reclamation.logicalBytes,
      result.foldLogicalBytesBefore - result.foldLogicalBytesAfter,
      'logical reclamation accounting must equal the exact output delta',
    );
    assert.deepEqual(result.reclamation.allocatedBytes, { state: 'absent' });
    assert.deepEqual(Buffer.from(store.get('same')), Buffer.alloc(65_536, 0x42),
      'live content must remain byte-exact after refold');
    const event = store.lifecycleEvents().events.findLast((candidate) => candidate.operation === 'refold');
    assert.equal(event?.outcome, 'succeeded');
  } finally {
    store.close();
    await rm(dir, { recursive: true, force: true });
  }
});
