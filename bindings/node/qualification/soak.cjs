'use strict';

const assert = require('node:assert/strict');
const { performance } = require('node:perf_hooks');
const { NativeStore, retainedCommits } = require('..');
const { putRecord } = require('./record-adapter.cjs');

function cyclePayload(cycle, bytes) {
  const payload = Buffer.alloc(bytes, (cycle * 31) & 0xff);
  payload.writeUInt32LE(cycle, 0);
  return payload;
}

async function allIds(store) {
  const ids = [];
  let cursor;
  do {
    const page = await store.scan({ limit: 37, maxExamined: 53, cursor });
    ids.push(...page.rows.map(({ id }) => id));
    cursor = page.next;
  } while (cursor);
  return ids;
}

async function runSoak({
  dir,
  cycles,
  recordsPerCycle = 8,
  payloadBytes = 2048,
  compactEvery = 8,
  restartEvery = 16,
}) {
  if (!dir || !Number.isInteger(cycles) || cycles < 2) {
    throw new TypeError('runSoak needs a directory and at least two cycles');
  }
  const shared = Buffer.from('qualification-shared-context-v1');
  const expected = new Set();
  const started = performance.now();
  let store = await NativeStore.open(dir, {
    blockTargetBytes: 64n << 10n,
    compressionLevel: 3,
  });
  let restarts = 0;
  let boundedCompactions = 0;
  let partHighWater = 0n;
  let acceptedOps = 0;
  try {
    for (let cycle = 0; cycle < cycles; cycle++) {
      const ops = [];
      for (let record = 0; record < recordsPerCycle; record++) {
        const id = `soak/record/${cycle.toString().padStart(6, '0')}/${record.toString().padStart(3, '0')}`;
        expected.add(id);
        ops.push(putRecord({
          id,
          fields: [
            { name: 'soak.cycle', type: 'uint', value: BigInt(cycle) },
            { name: 'soak.partition', type: 'int', value: BigInt(record % 4) },
            { name: 'soak.active', type: 'bool', value: true },
          ],
          contents: [
            { name: 'shared', bytes: shared },
            { name: 'payload', bytes: Buffer.alloc(payloadBytes, record & 3) },
          ],
        }));
      }

      const anchor = `soak/anchor/${(cycle % 8).toString().padStart(2, '0')}`;
      expected.add(anchor);
      ops.push(putRecord({
        id: anchor,
        fields: [
          { name: 'soak.cycle', type: 'uint', value: BigInt(cycle) },
          { name: 'soak.role', type: 'string', value: 'anchor' },
        ],
        contents: [{ name: 'payload', bytes: cyclePayload(cycle, payloadBytes) }],
      }));

      const ephemeral = `soak/ephemeral/${cycle.toString().padStart(6, '0')}`;
      expected.add(ephemeral);
      ops.push(putRecord({
        id: ephemeral,
        fields: [{ name: 'soak.role', type: 'string', value: 'ephemeral' }],
        contents: [{ name: 'payload', bytes: cyclePayload(cycle + cycles, payloadBytes) }],
      }));
      if (cycle >= 2) {
        const expired = `soak/ephemeral/${(cycle - 2).toString().padStart(6, '0')}`;
        expected.delete(expired);
        ops.push({ kind: 'delete', id: expired });
      }

      await store.write(ops, true);
      acceptedOps += ops.length;
      assert.equal(await store.flush(), true);
      const health = await store.health();
      if (health.parts > partHighWater) partHighWater = health.parts;

      if ((cycle + 1) % compactEvery === 0 && health.parts >= 2n) {
        // Producing eight parts and merging one eight-part unit has a +1 part/interval equilibrium:
        // the merge output itself is a part. Drain through as many bounded units as necessary so
        // the consumer policy, rather than an unbounded engine operation, keeps up with ingestion.
        let remaining = health.parts;
        while (remaining >= 2n) {
          const compacted = await store.compactBounded({
            maxInputParts: 8,
            maxInputRows: 1_000_000n,
            maxInputBytes: 1n << 40n,
          });
          assert(compacted.merge, 'a two-or-more-part backlog must yield a bounded work unit');
          assert(compacted.plan.inputParts <= 8n);
          assert.equal(compacted.merge.foldBytesTouched, 0n);
          boundedCompactions += 1;
          remaining = compacted.partsAfter;
        }
      }

      if ((cycle + 1) % restartEvery === 0 && cycle + 1 < cycles) {
        await store.close(false);
        store = await NativeStore.open(dir, {
          blockTargetBytes: 64n << 10n,
          compressionLevel: 3,
        });
        restarts += 1;
      }
    }

    const expectedIds = [...expected].sort();
    assert.deepEqual(await allIds(store), expectedIds);
    assert.deepEqual(
      await store.readContent('soak/record/000000/000', 'shared'),
      shared,
    );
    const retainedBeforeRefold = await retainedCommits(dir);
    assert.equal(retainedBeforeRefold.length, 4);

    const compacted = await store.compact(true);
    assert.equal(compacted.partsAfter, 1n);
    if (compacted.merge) assert.equal(compacted.merge.foldBytesTouched, 0n);
    const distribution = await store.partDistribution();
    assert.equal(distribution.parts, 1n);
    assert.equal(distribution.totalRows, BigInt(expectedIds.length));
    const beforeRefold = await store.contentLiveness();
    assert(beforeRefold.deadLogicalBytes > 0n);
    const preflight = await store.estimateRefoldSpace();
    assert(preflight.estimate);
    assert.equal(preflight.estimate.estimateIsHardBound, false);
    const refolded = await store.refold();
    assert.equal(refolded.recordsKept, BigInt(expectedIds.length));
    assert(refolded.piecesDropped > 0n);
    const afterRefold = await store.contentLiveness();
    assert.equal(afterRefold.deadLogicalBytes, 0n);
    assert.equal(afterRefold.reclaimableBlocks.rawBytes, 0n);
    const verified = await store.verify();
    assert.equal(verified.parts, 1n);
    assert.equal(verified.trailingUncommittedBytes, 0n);
    assert.deepEqual(await allIds(store), expectedIds);
    const usage = await store.spaceUsage();

    return {
      cycles,
      recordsPerCycle,
      payloadBytes,
      acceptedOps,
      liveRecords: expectedIds.length,
      boundedCompactions,
      restarts,
      partHighWater: partHighWater.toString(),
      deadBytesBeforeRefold: beforeRefold.deadLogicalBytes.toString(),
      foldBytesBeforeRefold: refolded.foldBytesBefore.toString(),
      foldBytesAfterRefold: refolded.foldBytesAfter.toString(),
      finalLogicalStoreBytes: usage.total.logicalBytes.toString(),
      durationMs: Math.round((performance.now() - started) * 100) / 100,
    };
  } finally {
    try { await store.close(); } catch {}
  }
}

if (require.main === module) {
  const dir = process.argv[2];
  const cycles = Number.parseInt(process.argv[3] ?? '512', 10);
  runSoak({ dir, cycles }).then(
    (report) => console.log(JSON.stringify(report, null, 2)),
    (error) => {
      console.error(error);
      process.exitCode = 1;
    },
  );
}

module.exports = { runSoak };
