import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

import { open, TurndbError } from '../index.mjs';

async function fixture(tag) {
  const dir = await mkdtemp(join(tmpdir(), `turndb-integrity-${tag}-`));
  const store = await open(dir);
  store.write(
    [
      {
        kind: 'put',
        id: 'member/0001',
        contents: [
          { name: 'request', bytes: Uint8Array.from([0, 255, 1]) },
          { name: 'response', bytes: 'answer' },
        ],
        attrs: [
          ['same', 'first'],
          ['same', 'second'],
        ],
      },
      {
        kind: 'put',
        id: 'member/0002',
        contents: [{ name: 'unknown/kind', bytes: 'xyz' }],
      },
    ],
    { durable: true },
  );
  store.flush();
  // A second commit ensures the retained chain has a predecessor as well as the newest copy.
  store.write(
    [
      {
        kind: 'put',
        id: 'member/0003',
        contents: [{ name: 'body', bytes: 'tail' }],
      },
    ],
    { durable: true },
  );
  store.flush();
  return { dir, store };
}

async function flip(path, offset = undefined) {
  const before = await readFile(path);
  assert.ok(before.length > 0, `${path} must contain bytes before mutation`);
  const at = offset ?? Math.floor(before.length / 2);
  assert.ok(at >= 0 && at < before.length, `mutation offset ${at} must be in ${path}`);
  const after = Buffer.from(before);
  after[at] ^= 0x40;
  await writeFile(path, after);
  const observed = await readFile(path);
  assert.notDeepEqual(observed, before, `mutation must change ${path}`);
  assert.equal(observed[at], after[at], `mutation byte must reach ${path}`);
}

async function files(dir, prefix) {
  return (await readdir(dir)).filter((name) => name.startsWith(prefix)).sort();
}

function corruption(error) {
  assert.ok(error instanceof TurndbError, `expected TurndbError, got ${error?.constructor?.name}`);
  assert.equal(error.code, 'CORRUPTION');
  return true;
}

test('verify reports exact committed-snapshot evidence and health makes no integrity claim', async () => {
  const { dir, store } = await fixture('clean');
  try {
    assert.equal(store.get('missing/id'), null, 'absence remains null');
    const report = store.verify();
    assert.equal(report.scope, 'committed_snapshot');
    assert.equal(report.state, 'valid');
    assert.deepEqual(report.retainedManifests, { state: 'verified', count: 2 });
    assert.equal(report.records, 3);
    assert.equal(report.contentValues, 4);
    assert.equal(report.contentBytes, 16n);
    assert.equal(report.contentIdentities, 4);
    assert.equal(report.unidentifiedContentValues, 0);
    assert.ok(report.parts > 0);
    assert.ok(report.partSections > 0);
    assert.ok(report.fold.blocks > 0);

    const health = store.health();
    assert.equal(health.state, 'available');
    assert.equal('valid' in health, false, 'cheap health must not imply integrity');
    assert.equal(health.commit, 2n);
    assert.equal(health.memtableEntries, 0);
  } finally {
    store.close();
    await rm(dir, { recursive: true, force: true });
  }
});

test('a mutated part is corruption, never an absent record', async () => {
  const { dir, store } = await fixture('part');
  store.close();
  try {
    const [part] = await files(dir, 'part-');
    assert.ok(part, 'fixture must create a part');
    // The first section starts at byte zero. Damage its payload rather than the footer/TOC so the
    // part remains openable and verification exercises the section checksum itself.
    await flip(join(dir, part), 16);
    const reopened = await open(dir);
    try {
      assert.throws(() => reopened.verify(), corruption);
      assert.throws(
        () => reopened.get('member/0001'),
        corruption,
        'a corrupt record must throw rather than arrive as absence',
      );
    } finally {
      reopened.close();
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('a mutated fold frame is typed store-open corruption', async () => {
  const { dir, store } = await fixture('fold');
  store.close();
  try {
    const generations = (await readdir(dir)).filter((name) => name === 'fold' || name.startsWith('fold-')).sort();
    assert.ok(generations.length > 0, 'fixture must create a fold generation');
    const segments = await readdir(join(dir, generations.at(-1)));
    const segment = segments.find((name) => name.startsWith('seg-') && name.endsWith('.fold'));
    assert.ok(segment, 'fixture must create a fold segment');
    await flip(join(dir, generations.at(-1), segment));
    await assert.rejects(open(dir), corruption);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('a mutated live manifest is typed store-open corruption', async () => {
  const { dir, store } = await fixture('live-manifest');
  store.close();
  try {
    await flip(join(dir, 'MANIFEST'));
    await assert.rejects(open(dir), corruption);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('the newest retained manifest is verified explicitly rather than absorbed by scope', async () => {
  const { dir, store } = await fixture('retained-manifest');
  store.close();
  try {
    const retained = await files(dir, 'MANIFEST.');
    assert.ok(retained.length >= 2, 'fixture must create a retained chain');
    await flip(join(dir, retained.at(-1)));
    const reopened = await open(dir);
    try {
      assert.throws(() => reopened.verify(), corruption);
    } finally {
      reopened.close();
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
