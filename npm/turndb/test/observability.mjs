import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

import { capabilities, compiledCapabilities, open, TurndbError } from '../index.mjs';

async function withStore(tag, fn) {
  const root = await mkdtemp(join(tmpdir(), `turndb-observability-${tag}-`));
  const dir = join(root, 's.turndb');
  const store = await open(dir);
  try {
    return await fn(store, dir);
  } finally {
    try { store.close(); } catch {}
    await rm(dir, { recursive: true, force: true });
  }
}

test('compiled facts and callable binding operations are separate questions', async () => {
  const compiled = await compiledCapabilities();
  const reachable = await capabilities();
  assert.equal(compiled.portable_wasm, true);
  assert.equal('physical_erasure' in compiled, false, 'a build cannot predict an erase outcome');
  assert.equal(reachable.binding, 'wasi');
  assert.deepEqual(reachable.unavailable, {
    allocatedBytes: 'absent',
    atomicNoReplacePublication: 'absent',
    cancellationToken: 'absent',
  });
  assert.equal(reachable.limits.lifecycleEvents, 256);
  for (const operation of [
    'metrics', 'lifecycleEvents', 'contentLiveness', 'spaceUsage', 'eraseIds',
  ]) assert.ok(reachable.bindingOperations.includes(operation), operation);
  await withStore('reachability-complete', async (store) => {
    const callable = Object.getOwnPropertyNames(Object.getPrototypeOf(store))
      .filter((name) => name !== 'constructor'
        && typeof Object.getOwnPropertyDescriptor(Object.getPrototypeOf(store), name)?.value === 'function')
      .sort();
    assert.deepEqual([...reachable.bindingOperations].sort(), callable,
      'a new public method must be added to the reachability profile');
  });
});

test('metrics, lifecycle events, liveness, and space facts are callable and typed', async () => {
  await withStore('surfaces', async (store) => {
    store.write([{
      kind: 'put', id: 'same/id', contents: [{ name: 'body', bytes: 'old bytes' }], attrs: [],
    }], { durable: true });
    store.flush();
    store.write([{
      kind: 'put', id: 'same/id', contents: [{ name: 'body', bytes: 'new bytes' }], attrs: [],
    }], { durable: true });
    store.flush();
    const liveness = store.contentLiveness();
    assert.equal(liveness.livePieces, 1n);
    assert.ok(liveness.deadLogicalBytes > 0n, 'superseded content must be classified dead');

    const usage = store.spaceUsage();
    assert.ok(usage.total.logicalBytes > 0n);
    for (const amount of [usage.live, usage.retainedOnly, usage.unclassified, usage.total]) {
      assert.equal(amount.allocatedBytes.state, 'absent');
      assert.equal('bytes' in amount.allocatedBytes, false, 'absent is never encoded as zero');
    }

    store.verify();
    const metrics = store.metrics();
    assert.equal(metrics.verification.attempts, 1n);
    assert.equal(metrics.verification.succeeded, 1n);
    assert.ok(metrics.sync.succeeded >= 2n);
    const events = store.lifecycleEvents();
    assert.equal(events.capacity, 256);
    assert.ok(events.events.some((event) =>
      event.operation === 'verification' && event.outcome === 'succeeded'));
    const after = store.lifecycleEvents({ after: events.latestSequence, limit: 1 });
    assert.deepEqual(after.events, []);
  });
});

test('erasure returns measured versus not-applicable outcomes and journals the operation', async () => {
  await withStore('erase', async (store) => {
    store.write([{
      kind: 'put', id: 'erase/me', contents: [{ name: 'body', bytes: 'secret' }], attrs: [],
    }], { durable: true });
    store.flush();
    const erased = store.eraseIds(['erase/me']);
    assert.deepEqual(
      { requested: erased.requested, erased: erased.erased, absent: erased.absent },
      { requested: 1, erased: 1, absent: 0 },
    );
    assert.equal(erased.reclamation.state, 'measured');
    assert.ok(erased.reclamation.logicalBytes > 0n);
    assert.deepEqual(erased.reclamation.allocatedBytes, { state: 'absent' });
    assert.equal(store.get('erase/me'), null);

    const absent = store.eraseIds(['erase/me']);
    assert.deepEqual(absent.reclamation, { state: 'not_applicable' });
    const metrics = store.metrics();
    assert.equal(metrics.erase.attempts, 2n);
    assert.equal(metrics.erase.succeeded, 2n);
    assert.equal(
      store.lifecycleEvents().events.filter((event) => event.operation === 'erase').length,
      2,
    );
  });
});

test('a verification failure is callable through both the result and observability surfaces', async () => {
  await withStore('failure', async (store, dir) => {
    store.write([{
      kind: 'put', id: 'corrupt/me', contents: [{ name: 'body', bytes: 'intact' }], attrs: [],
    }], { durable: true });
    store.flush();
    // The part member's aligned start, anchored by its footer magic — its first section begins
    // at member offset zero, and the damage lands there so verification exercises the section
    // checksum rather than the footer.
    const before = await readFile(dir);
    const footer = before.indexOf(Buffer.from('TURNPART'));
    assert.ok(footer > 0, 'the flushed part must exist in the store file');
    const partStart = Math.floor(footer / 4096) * 4096;
    const after = Buffer.from(before);
    after[partStart + 16] ^= 0x40;
    await writeFile(dir, after);
    assert.notDeepEqual(await readFile(dir), before, 'mutation must reach the tested artifact');

    assert.throws(() => store.verify(), (error) => {
      assert.ok(error instanceof TurndbError);
      assert.equal(error.code, 'CORRUPTION');
      return true;
    });
    assert.equal(store.metrics().verificationCorruptionFailures, 1n);
    const event = store.lifecycleEvents().events.findLast((candidate) =>
      candidate.operation === 'verification');
    assert.equal(event.outcome, 'failed');
    assert.equal(event.errorCode, 'CORRUPTION');
  });
});

test('the binding exposes lifecycle capacity as a limit and reports cursor gaps', async () => {
  await withStore('journal-gap', async (store) => {
    for (let i = 0; i < 258; i++) store.verify();
    const batch = store.lifecycleEvents({ after: 0n, limit: 1 });
    assert.equal(batch.capacity, 256);
    assert.equal(batch.gap, true);
    assert.equal(batch.droppedEvents, 3n, 'open recovery plus 258 verifies exceeds capacity by three');
    assert.ok(batch.oldestAvailableSequence > 1n);
    assert.equal(batch.events.length, 1);
  });
});
