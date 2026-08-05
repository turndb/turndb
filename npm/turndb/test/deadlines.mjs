import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { WASI } from 'node:wasi';

import './_artifact.mjs';
import { capabilities, open, TurndbError } from '../index.mjs';

const HEAVY_BYTES = 819_200;

async function directory(tag) {
  return mkdtemp(join(tmpdir(), `turndb-deadline-${tag}-`));
}

function cancelled(call) {
  assert.throws(call, (error) => {
    assert.ok(error instanceof TurndbError);
    assert.equal(error.code, 'CANCELLED');
    assert.match(error.message, /deadline exceeded/);
    return true;
  });
}

test('expired portable deadlines refuse at safe checkpoints and a generous deadline completes', async () => {
  const dir = await directory('surface');
  let store = await open(dir);
  try {
    store.write([{
      kind: 'put', id: 'still/live', contents: [{ name: 'body', bytes: 'before deadline' }], attrs: [],
    }, {
      kind: 'put', id: 'zz/heavy', contents: [{ name: 'body', bytes: Buffer.alloc(HEAVY_BYTES, 0x5a) }], attrs: [],
    }], { durable: true });
    store.flush();

    const profile = await capabilities();
    for (const operation of profile.controls.deadlineOperations) {
      assert.equal(typeof store[operation], 'function', `${operation} deadline must be callable`);
    }
    assert.equal(profile.unavailable.cancellationToken, 'absent');

    cancelled(() => store.sync({ timeoutMs: 0 }));
    cancelled(() => store.flush({ timeoutMs: 0 }));
    cancelled(() => store.autoCompact({ timeoutMs: 0 }));
    cancelled(() => store.maybeCompact({ timeoutMs: 0 }));
    cancelled(() => store.verify({ timeoutMs: 0 }));
    cancelled(() => store.contentLiveness({ timeoutMs: 0 }));
    cancelled(() => store.spaceUsage({ timeoutMs: 0 }));
    cancelled(() => store.estimateRefoldSpace({ timeoutMs: 0 }));
    cancelled(() => store.refold({ timeoutMs: 0 }));
    cancelled(() => store.scan({ timeoutMs: 0, contents: [{ name: 'body', mode: 'bytes' }] }));
    cancelled(() => store.scan({
      from: 'zz/', timeoutMs: 1, contents: [{ name: 'body', mode: 'bytes' }],
      maxReconstructedBytes: HEAVY_BYTES,
    }));
    cancelled(() => store.eraseIds(['still/live'], { timeoutMs: 0 }));

    const page = store.scan({
      timeoutMs: 3_600_000,
      contents: [{ name: 'body', mode: 'bytes' }],
    });
    assert.equal(page.rows.length, 2, 'the maximum valid timeout must not be rejected wholesale');
    assert.equal(Buffer.from(page.rows[0].contents[0].bytes).toString(), 'before deadline');

    store.close();
    store = await open(dir);
    assert.equal(Buffer.from(store.get('still/live')).toString(), 'before deadline',
      'expired erasure must stop before its atomic tombstone publication');
  } finally {
    try { store.close(); } catch {}
    await rm(dir, { recursive: true, force: true });
  }
});

test('freezing the guest clock makes the real artifact deadline control fail', async () => {
  const original = WASI.prototype.getImportObject;
  let clockCalls = 0;
  WASI.prototype.getImportObject = function (...args) {
    const owner = this;
    const imports = original.apply(owner, args);
    const wasi = imports.wasi_snapshot_preview1;
    const clock = wasi.clock_time_get;
    const instanceSymbol = Object.getOwnPropertySymbols(owner)
      .find((symbol) => symbol.description === 'kInstance');
    wasi.clock_time_get = (...clockArgs) => {
      const errno = clock(...clockArgs);
      const instance = owner[instanceSymbol];
      if (errno === 0 && instance) {
        new DataView(instance.exports.memory.buffer).setBigUint64(clockArgs[2], 123n, true);
        clockCalls++;
      }
      return errno;
    };
    return imports;
  };

  const dir = await directory('frozen');
  let store;
  try {
    store = await open(dir);
    const body = Buffer.alloc(HEAVY_BYTES, 0x5a);
    store.write([{
      kind: 'put', id: 'heavy', contents: [{ name: 'body', bytes: body }], attrs: [],
    }], { durable: true });
    store.flush();
    const page = store.scan({
      timeoutMs: 1,
      contents: [{ name: 'body', mode: 'bytes' }],
      maxReconstructedBytes: body.length,
    });
    assert.equal(page.rows[0].contents[0].bytes.length, body.length,
      'a frozen guest clock must prevent a positive deadline from advancing');
    assert.equal(page.stats.durationNs, 0n, 'the harness must detect the frozen clock itself');
    assert.ok(clockCalls > 1, 'the real artifact must read the wrapped guest clock repeatedly');
  } finally {
    WASI.prototype.getImportObject = original;
    try { store?.close(); } catch {}
    await rm(dir, { recursive: true, force: true });
  }
});
