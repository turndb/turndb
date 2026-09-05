// Single-file reads through the portable package: produce with the CLI, consume with wasm.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import './_artifact.mjs';
import { open, openFile, TurndbError } from '../index.mjs';

// npm/build.sh builds the CLI and exports this; it is the producer of the artifacts read below,
// because atomic no-replace publication is unavailable in WASI Preview1, so this capability is
// truthfully absent from the portable profile rather than weakened.
const CLI = process.env.TURNDB_CLI
  ?? new URL('../../../target/debug/turndb', import.meta.url).pathname;
if (!existsSync(CLI)) {
  throw new Error(
    `single-file tests need the turndb CLI to produce their fixtures; run \`bash npm/build.sh\`, `
    + `or set TURNDB_CLI. Looked at ${CLI}`,
  );
}

test('reads a live store and its backup from one file', async () => {
  const root = await mkdtemp(join(tmpdir(), 'turndb-wasm-file-'));
  const container = join(root, 'store.turndb');
  const payload = JSON.stringify([{ role: 'user', content: 'one file' }]);
  // The portable writer produces the single file directly; the native CLI installs a backup
  // because WASI Preview1 cannot provide the required atomic no-replace operation.
  const store = await open(container);
  store.putBody('trace:1#input', payload, { model: 'm0' });
  store.sync();
  store.flush();
  store.close();

  const backup = join(root, 'snapshot.turndb');
  execFileSync(CLI, ['backup', container, backup]);

  for (const [label, file] of [['container', container], ['backup', backup]]) {
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

test('the first call a new user makes reports its own failure', async () => {
  const root = await mkdtemp(join(tmpdir(), 'turndb-first-contact-'));

  // A store path whose parents do not exist yet is the overwhelmingly common first call. This
  // binding must preserve that creation contract even though WASI preopens the parent first.
  const fresh = join(root, 'deep', 'nested', 'store.turndb');
  const store = await open(fresh);
  store.putBody('x', 'y');
  store.sync();
  store.close();
  assert.deepEqual((await open(fresh)).scanIds(), ['x'], 'the created store must be reopenable');

  // A single file cannot be created by opening it, so that is a refusal — but a refusal that
  // names the path and carries a code, not a bare errno out of uvwasi_init.
  await assert.rejects(
    openFile(join(root, 'absent.turndb')),
    (e) => e instanceof TurndbError && e.code === 'NOT_FOUND' && /absent\.turndb/.test(e.message),
    'a missing single file must refuse by name',
  );
  await assert.rejects(
    openFile(root),
    (e) => e instanceof TurndbError && /not a regular file/.test(e.message),
    'a directory handed to openFile must refuse as one',
  );

  await rm(root, { recursive: true, force: true });
});
