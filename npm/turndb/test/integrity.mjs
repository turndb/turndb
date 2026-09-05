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

function manifestStarts(buf) {
  const signature = Buffer.from('{"draft_epoch":1,"parts":');
  const offsets = [];
  for (let at = buf.indexOf(signature); at !== -1; at = buf.indexOf(signature, at + 1)) {
    offsets.push(at);
  }
  assert.ok(offsets.length >= 2, 'current and retained manifest payloads must both be present');
  return offsets;
}

// The N-th part member's aligned start, anchored by its footer magic. Valid while each part in
// the fixture is smaller than one alignment block, which these fixtures assert.
function partStart(buf, nth = 0) {
  const magic = Buffer.from('TDBPRT01');
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

test('verify reports exact current-manifest-revision evidence and health makes no integrity claim', async () => {
  const { dir, store } = await fixture('clean');
  try {
    assert.equal(store.get('missing/id'), null, 'absence remains null');
    const report = store.verify();
    assert.equal(report.scope, 'current_manifest_revision');
    assert.equal(report.state, 'valid');
    assert.deepEqual(report.retainedManifests, { state: 'verified', count: 2 });
    assert.equal(report.records, 3);
    assert.equal(report.contentValues, 4);
    assert.equal(report.contentBytes, 16n);
    assert.equal(report.contentIdentities, 4);
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

test('a mutated part is refused as corruption during store open', async () => {
  const { dir, store } = await fixture('part');
  store.close();
  try {
    // The first section starts at member offset zero. Current open validates its structural
    // semantics, so damage is refused before a handle can report a false absent record.
    const image = await readFile(dir);
    await flip(dir, partStart(image, 0) + 16);
    await assert.rejects(open(dir), corruption);
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
    assert.equal(image.subarray(2 * ALIGN, 2 * ALIGN + 8).toString(), 'TDBFLD01');
    await flip(dir, 2 * ALIGN + 52);
    await assert.rejects(open(dir), corruption);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('a mutated current manifest is typed store-open corruption', async () => {
  const { dir, store } = await fixture('live-manifest');
  store.close();
  try {
    const image = await readFile(dir);
    const starts = manifestStarts(image);
    await flip(dir, starts[starts.length - 1] + 40);
    await assert.rejects(open(dir), corruption);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('a damaged retained manifest is refused during writer open', async () => {
  const { dir, store } = await fixture('retained-manifest');
  store.close();
  try {
    // Each revision writes its retained copy before current MANIFEST. The final two canonical JSON
    // payloads are therefore the newest retained authority and the current authority.
    const image = await readFile(dir);
    const starts = manifestStarts(image);
    const retainedStart = starts[starts.length - 2];
    await flip(dir, retainedStart + 40);
    await assert.rejects(open(dir), corruption);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
