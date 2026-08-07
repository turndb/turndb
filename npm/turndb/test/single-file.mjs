// Single-file reads through the portable package: produce with the CLI, consume with wasm.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import './_artifact.mjs';
import { open, openFile, singleFileKind, TurndbError } from '../index.mjs';

const CLI = new URL('../../../target/debug/turndb', import.meta.url).pathname;

test('reads a store held in one file, pack or container', async () => {
  const root = await mkdtemp(join(tmpdir(), 'turndb-wasm-file-'));
  const dir = join(root, 'store');
  const payload = JSON.stringify([{ role: 'user', content: 'one file' }]);
  await mkdir(dir, { recursive: true });
  const store = await open(dir);
  store.putBody('trace:1#input', payload, { model: 'm0' });
  store.sync();
  store.flush();
  store.close();

  assert.equal(await singleFileKind(dir), null, 'a directory carries neither magic');

  const container = join(root, 'store.turndb');
  const pack = join(root, 'store.pack');
  execFileSync(CLI, ['checkpoint', dir, container]);
  execFileSync(CLI, ['pack', dir, pack]);
  assert.equal(await singleFileKind(container), 'container');
  assert.equal(await singleFileKind(pack), 'pack');

  for (const [label, file] of [['container', container], ['pack', pack]]) {
    const ro = await openFile(file);
    assert.deepEqual(ro.scanIds(), ['trace:1#input'], `${label} pages the same ids`);
    assert.equal(new TextDecoder().decode(ro.get('trace:1#input')), payload, `${label} is byte-exact`);
    assert.equal(ro.scan({ limit: 10 }).rows.length, 1, `${label} scans`);
    // A single-file handle has no writer role, so every mutating verb must refuse it by name.
    assert.throws(
      () => ro.putBody('trace:2#input', 'nope'),
      (e) => e instanceof TurndbError && /read-only single-file/.test(e.message),
      `${label} must refuse writes`,
    );
    ro.close();
  }

  await rm(root, { recursive: true, force: true });
});
