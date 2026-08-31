'use strict';

const child = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const dist = path.resolve(process.argv[2] || path.join(root, 'dist'));
const hostTarget = (() => {
  if (process.platform === 'linux' && process.arch === 'x64') return 'linux-x64-gnu';
  if (process.platform === 'win32' && process.arch === 'x64') return 'win32-x64-msvc';
  throw new Error(`no native prebuild test target for ${process.platform}-${process.arch}`);
})();
const manifestPath = path.join(dist, `prebuild-manifest-${hostTarget}.json`);
const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));

if (
  manifest.schema !== 2 ||
  manifest.nodeApi !== 6 ||
  manifest.npmTarget !== hostTarget ||
  typeof manifest.publishable !== 'boolean'
) {
  throw new Error(`unsupported or inconsistent prebuild manifest at ${manifestPath}`);
}
const glibcRuntime = process.report?.getReport?.().header?.glibcVersionRuntime;
if (hostTarget === 'linux-x64-gnu' && !glibcRuntime) {
  throw new Error('linux-x64-gnu prebuild test requires a glibc runtime');
}

function compareVersions(left, right) {
  const a = left.split('.').map((part) => Number.parseInt(part, 10));
  const b = right.split('.').map((part) => Number.parseInt(part, 10));
  for (let index = 0; index < Math.max(a.length, b.length); index += 1) {
    const difference = (a[index] || 0) - (b[index] || 0);
    if (difference !== 0) return difference;
  }
  return 0;
}
if (hostTarget === 'linux-x64-gnu' && compareVersions(glibcRuntime, manifest.glibcRequired) < 0) {
  throw new Error(
    `prebuild requires glibc ${manifest.glibcRequired}, runtime provides only ${glibcRuntime}`,
  );
}

for (const entry of manifest.tarballs) {
  const file = path.join(dist, entry.file);
  const bytes = fs.readFileSync(file);
  const actual = crypto.createHash('sha256').update(bytes).digest('hex');
  if (bytes.length !== entry.bytes || actual !== entry.sha256) {
    throw new Error(`${entry.file} does not match the collected prebuild manifest`);
  }
}

const rootFilename = `turndb-native-${manifest.version}.tgz`;
const targetFilename = `turndb-native-${hostTarget}-${manifest.version}.tgz`;
const rootTarball = manifest.tarballs.find((entry) => entry.file === rootFilename);
const targetTarball = manifest.tarballs.find(
  (entry) => entry.file === targetFilename,
);
if (!rootTarball || !targetTarball) {
  throw new Error(`prebuild manifest does not contain both root and ${hostTarget} packages`);
}

const consumer = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-native-install-'));
try {
  fs.writeFileSync(
    path.join(consumer, 'package.json'),
    JSON.stringify({ name: 'turndb-prebuild-smoke', version: '0.0.0', private: true }),
  );
  child.execFileSync(
    'npm',
    [
      'install', '--ignore-scripts', '--offline', '--no-audit', '--no-fund',
      path.join(dist, targetTarball.file),
      path.join(dist, rootTarball.file),
    ],
    {
      cwd: consumer,
      env: { ...process.env, npm_config_cache: path.join(consumer, '.npm-cache') },
      stdio: 'inherit',
    },
  );

  const smoke = String.raw`
    const assert = require('node:assert/strict');
    const path = require('node:path');
    const addon = require('@turndb/native');
    assert.match(require.resolve('@turndb/native-${hostTarget}'), /\.node$/);
    assert.equal(addon.capabilities().napiVersion, 6);
    (async () => {
      const directory = path.join(process.cwd(), 'store');
      let store = await addon.NativeStore.open(directory);
      await store.write([
        {
          kind: 'put',
          id: 'member/alice/0001/activity',
          attrs: [
            { name: 'member.key', kind: 'string', stringValue: 'alice' },
            { name: 'record.family', kind: 'string', stringValue: 'activity' },
            { name: 'occurred_at', kind: 'timestamp_ns', timestampNsValue: 1000n },
          ],
        },
        {
          kind: 'put',
          id: 'member/alice/0002/generation',
          attrs: [
            { name: 'member.key', kind: 'string', stringValue: 'alice' },
            { name: 'record.family', kind: 'string', stringValue: 'generation' },
            { name: 'occurred_at', kind: 'timestamp_ns', timestampNsValue: 2000n },
          ],
          contents: [
            { name: 'request', bytes: Buffer.from('{"prompt":"status?"}') },
            { name: 'response', bytes: Buffer.from('{"status":"ok"}') },
          ],
        },
      ], true);

      const request = {
        from: 'member/alice/',
        to: 'member/alice0',
        attrs: ['record.family', 'occurred_at'],
        contents: [{ name: 'response', mode: 'metadata' }],
        predicates: [{
          kind: 'attr',
          op: 'eq',
          value: { name: 'member.key', kind: 'string', stringValue: 'alice' },
        }],
        limit: 1,
      };
      const first = await store.scan(request);
      const second = await store.scan({ ...request, cursor: first.next });
      assert.deepEqual(
        [...first.rows, ...second.rows].map(({ id }) => id),
        ['member/alice/0001/activity', 'member/alice/0002/generation'],
      );
      assert.equal(first.stats.io.foldBlocksTouched, 0n);
      assert.equal(second.rows[0].contents[0].present, true);
      assert.equal(second.rows[0].contents[0].bytes, undefined);

      // Reopen before flush: the durable acknowledgement is backed by WAL recovery, and content
      // remains addressable through the same consumer-selected name.
      await store.close(false);
      store = await addon.NativeStore.open(directory);
      assert.equal(
        (await store.readContent('member/alice/0002/generation', 'response')).toString(),
        '{"status":"ok"}',
      );
      await store.close(true);
    })().catch((error) => {
      console.error(error);
      process.exitCode = 1;
    });
  `;
  child.execFileSync(process.execPath, ['-e', smoke], { cwd: consumer, stdio: 'inherit' });
  child.execFileSync(
    process.execPath,
    [
      '--input-type=module',
      '-e',
      String.raw`
        import assert from 'node:assert/strict';
        import native, { capabilities, NativeStore, TurnDbError } from '@turndb/native';
        assert.equal(capabilities().napiVersion, 6);
        assert.equal(NativeStore, native.NativeStore);
        assert.equal(TurnDbError, native.TurnDbError);
      `,
    ],
    { cwd: consumer, stdio: 'inherit' },
  );
} finally {
  fs.rmSync(consumer, { recursive: true, force: true });
}

console.log(
    `installed and exercised ${manifest.package}@${manifest.version} on ` +
    `Node ${process.versions.node} (${hostTarget}` +
    `${glibcRuntime ? `, glibc ${glibcRuntime}` : ''})`,
);
