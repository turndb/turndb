import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

import { open, TurndbError } from '../index.mjs';

async function fixture(tag) {
  const root = await mkdtemp(join(tmpdir(), `turndb-integrity-${tag}-`));
  const dir = join(root, 's.turndb');
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

// Member surgery on the single file, guided by the format's fixed offsets: the live slot is the
// one whose seq (u64 LE at slot+8) is higher; dir_off is u64 LE at slot+16; members start on
// 4096-byte boundaries; the directory is written unaligned immediately after the MANIFEST member
// each commit restages. Geometry assumptions are asserted, so a fixture change fails loudly here
// rather than mutating the wrong artifact.
const ALIGN = 4096;

function liveDirOff(buf) {
  const seq0 = buf.readBigUInt64LE(8);
  const seq1 = buf.readBigUInt64LE(4096 + 8);
  const slot = seq1 > seq0 ? 4096 : 0;
  return Number(buf.readBigUInt64LE(slot + 16));
}

// The N-th part member's aligned start, anchored by its footer magic. Valid while each part in
// the fixture is smaller than one alignment block, which these fixtures assert.
function partStart(buf, nth = 0) {
  const magic = Buffer.from('TURNPART');
  let at = -1;
  for (let i = 0; i <= nth; i += 1) {
    at = buf.indexOf(magic, at + 1);
    assert.ok(at > 0, `part footer ${i} must exist in the store file`);
  }
  const start = Math.floor(at / ALIGN) * ALIGN;
  assert.ok(at - start < ALIGN, 'fixture parts must fit one alignment block');
  assert.ok(start >= 2 * ALIGN, 'a part member lives in the region');
  return start;
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
    // The first section starts at member offset zero. Damage its payload rather than the
    // footer/TOC so the part remains openable and verification exercises the section checksum.
    const image = await readFile(dir);
    await flip(dir, partStart(image, 0) + 16);
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
    // The virgin segment member is the first member in the region; its frames follow the
    // 48-byte header. Prove the geometry before cutting.
    const image = await readFile(dir);
    assert.equal(image.subarray(2 * ALIGN, 2 * ALIGN + 8).toString(), 'TURNFOLD');
    await flip(dir, 2 * ALIGN + 52);
    await assert.rejects(open(dir), corruption);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('a mutated live manifest is typed store-open corruption', async () => {
  const { dir, store } = await fixture('live-manifest');
  store.close();
  try {
    const image = await readFile(dir);
    await flip(dir, liveDirOff(image) - 2);
    await assert.rejects(open(dir), corruption);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('the newest retained manifest is verified explicitly rather than absorbed by scope', async () => {
  const { dir, store } = await fixture('retained-manifest');
  store.close();
  try {
    // The newest retained copy is the member staged immediately before the MANIFEST restage:
    // one alignment block above the manifest, which sits one block above the live directory.
    const image = await readFile(dir);
    const manifestStart = Math.floor((liveDirOff(image) - 1) / ALIGN) * ALIGN;
    const retainedStart = manifestStart - ALIGN;
    assert.equal(image[retainedStart], '{'.charCodeAt(0), 'the retained member must be JSON');
    await flip(dir, retainedStart + 40);
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
