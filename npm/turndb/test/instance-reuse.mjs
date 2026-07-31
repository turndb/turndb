import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, mkdtemp, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import './_artifact.mjs';
import { open, TurndbError } from '../index.mjs';

test('sequential opens reuse one WASI instance without external-memory accumulation', async () => {
  const root = await mkdtemp(join(tmpdir(), 'turndb-reuse-'));
  try {
    const firstDir = join(root, 'store-0');
    await mkdir(firstDir);
    const first = await open(firstDir);
    first.close();
    const oneInstance = process.memoryUsage().external;
    let peak = oneInstance;
    const fdBaseline = process.platform === 'linux' ? (await readdir('/proc/self/fd')).length : null;

    // This is intentionally well past the old fourth-open pressure boundary. The count is evidence,
    // not a budget: every iteration must reuse the same instance, so increasing it cannot create
    // another step in external-memory accounting.
    for (let i = 0; i < 64; i++) {
      const dir = join(root, `store-${i + 1}`);
      await mkdir(dir);
      const store = await open(dir);
      store.putBody(`record/${i}`, `body/${i}`);
      store.sync();
      store.close();
      peak = Math.max(peak, process.memoryUsage().external);
    }

    assert.ok(
      peak - oneInstance < 1 << 20,
      `sequential opens added ${peak - oneInstance} external bytes after the first instance`,
    );
    if (fdBaseline != null) {
      const fdFinal = (await readdir('/proc/self/fd')).length;
      assert.ok(
        fdFinal <= fdBaseline + 1,
        `sequential opens retained ${fdFinal - fdBaseline} file descriptors`,
      );
    }

    const firstReopened = await open(join(root, 'store-1'));
    assert.equal(firstReopened.getText('record/0'), 'body/0');
    assert.equal(firstReopened.get('record/63'), null, 'switching capabilities must not expose another store');
    firstReopened.close();

    const lastReopened = await open(join(root, 'store-64'));
    assert.equal(lastReopened.getText('record/63'), 'body/63');
    lastReopened.close();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('a live handle prevents a second handle in the process', async () => {
  const root = await mkdtemp(join(tmpdir(), 'turndb-one-handle-'));
  try {
    const firstDir = join(root, 'first');
    const secondDir = join(root, 'second');
    await mkdir(firstDir);
    await mkdir(secondDir);
    const store = await open(firstDir);
    await assert.rejects(open(secondDir), (e) => {
      assert.ok(e instanceof TurndbError);
      assert.match(e.message, /already has a store open/);
      return true;
    });
    store.close();

    const reopened = await open(secondDir);
    reopened.close();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('concurrent opens cannot switch the directory capability under a live handle', async () => {
  const root = await mkdtemp(join(tmpdir(), 'turndb-concurrent-open-'));
  try {
    const dirs = [join(root, 'first'), join(root, 'second')];
    await Promise.all(dirs.map((dir) => mkdir(dir)));
    const results = await Promise.allSettled(dirs.map((dir) => open(dir)));
    assert.equal(results.filter((r) => r.status === 'fulfilled').length, 1);
    assert.equal(results.filter((r) => r.status === 'rejected').length, 1);

    const liveIndex = results.findIndex((r) => r.status === 'fulfilled');
    const live = results[liveIndex].value;
    live.putBody('owner', String(liveIndex));
    live.sync();
    live.close();

    const reopened = await open(dirs[liveIndex]);
    assert.equal(reopened.getText('owner'), String(liveIndex));
    reopened.close();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('failed opens release their directory capability', async () => {
  const root = await mkdtemp(join(tmpdir(), 'turndb-failed-open-'));
  try {
    const badDir = join(root, 'bad');
    const goodDir = join(root, 'good');
    await mkdir(badDir);
    await mkdir(goodDir);
    await writeFile(join(badDir, 'MANIFEST'), 'not a manifest');
    const fdBaseline = process.platform === 'linux' ? (await readdir('/proc/self/fd')).length : null;

    for (let i = 0; i < 16; i++) {
      await assert.rejects(open(badDir), TurndbError);
    }
    if (fdBaseline != null) {
      const fdFinal = (await readdir('/proc/self/fd')).length;
      assert.ok(
        fdFinal <= fdBaseline + 1,
        `failed opens retained ${fdFinal - fdBaseline} file descriptors`,
      );
    }

    const store = await open(goodDir);
    store.close();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
