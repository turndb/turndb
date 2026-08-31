'use strict';

const assert = require('node:assert/strict');
const child = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { createRequire } = require('node:module');

function run(file, args, options = {}) {
  const result = child.spawnSync(file, args, { encoding: 'utf8', ...options });
  if (result.status !== 0) {
    throw new Error(`${file} ${args.join(' ')} failed (${result.status})\n${result.stdout}\n${result.stderr}`);
  }
  return `${result.stdout}${result.stderr}`;
}

function sha256(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

function decodeHex(file) {
  return Buffer.from(fs.readFileSync(file, 'utf8').replace(/\s/g, ''), 'hex');
}

async function main() {
  assert.equal(process.platform, 'win32', 'the installed Windows contract must run on Windows');
  const [installedFile, fixtureHex, evidenceArg] = process.argv.slice(2);
  assert(installedFile && fixtureHex && evidenceArg, 'usage: test-installed-windows.cjs INSTALLED FIXTURE EVIDENCE');
  const installed = JSON.parse(fs.readFileSync(installedFile, 'utf8').replace(/^\uFEFF/, ''));
  const evidence = path.resolve(evidenceArg);
  fs.mkdirSync(evidence, { recursive: true });

  const consumerRequire = createRequire(path.join(installed.NodeConsumer, 'package.json'));
  const native = consumerRequire('@turndb/native');
  const nativeMeta = consumerRequire('@turndb/native/package.json');
  const nativeSliceMeta = consumerRequire('@turndb/native-win32-x64-msvc/package.json');
  const cliMeta = consumerRequire('@turndb/cli/package.json');
  const cliSliceMeta = consumerRequire('@turndb/cli-win32-x64-msvc/package.json');
  const cli = path.join(path.dirname(consumerRequire.resolve('@turndb/cli-win32-x64-msvc/package.json')), 'turndb.exe');
  const versions = new Set([nativeMeta.version, nativeSliceMeta.version, cliMeta.version, cliSliceMeta.version]);
  assert.deepEqual([...versions], [installed.Version]);
  assert.equal(run(cli, ['--version']).trim(), `turndb ${installed.Version}`);
  assert.equal(run(installed.Python, ['-c', 'import importlib.metadata; print(importlib.metadata.version("turndb"))']).trim(), installed.Version);

  const caps = native.capabilities();
  assert.equal(caps.profile, 'native');
  assert.equal(caps.writerExclusion, 'os_enforced');
  assert.equal(caps.positionedIo, true);
  assert.equal(caps.allocatedSpaceUsage, true);
  assert.equal(caps.reclamation, 'punch_or_refold');

  const fixture = path.join(evidence, 'linux-reference.turndb');
  fs.writeFileSync(fixture, decodeHex(fixtureHex));
  const expectedIds = [];
  for (let round = 0; round < 3; round += 1) {
    for (let i = 0; i < 8; i += 1) {
      const id = `r${round}:${String(i).padStart(2, '0')}`;
      if (id !== 'r0:00' && id !== 'r1:03') expectedIds.push(id);
    }
  }
  const snapshot = await native.NativeSnapshot.openFile(fixture);
  const page = await snapshot.scan({ contractVersion: 1, limit: 100 });
  assert.deepEqual(page.rows.map(({ id }) => id), expectedIds);
  await snapshot.close();
  assert.match(run(cli, ['verify', fixture, '--deep']), /ok/i);
  run(installed.Python, ['-c', [
    'import turndb, sys',
    's=turndb.Snapshot.open(sys.argv[1])',
    'r=s.scan({"contractVersion":1,"limit":100})',
    'assert len(r["rows"]) == 22',
    's.close()',
  ].join(';'), fixture]);

  const debrisStore = path.join(evidence, 'debris.turndb');
  let store = await native.NativeStore.openFile(debrisStore);
  await store.write([{ kind: 'put', id: 'live', contents: [{ name: 'body', bytes: Buffer.from('live') }] }], true);
  await store.close();
  const deadPending = `${debrisStore}.publish-1-1`;
  fs.writeFileSync(deadPending, 'dead');
  store = await native.NativeStore.openFile(debrisStore);
  await store.close();
  assert.equal(fs.existsSync(deadPending), false, 'writer open removes dead pending publish beside a store');

  const absentStore = path.join(evidence, 'absent.turndb');
  const pending = `${absentStore}.publish-2-3`;
  fs.writeFileSync(pending, 'unpublished');
  const inspected = child.spawnSync(cli, ['inspect', absentStore], { encoding: 'utf8' });
  assert.notEqual(inspected.status, 0);
  assert.match(`${inspected.stdout}${inspected.stderr}`, /PendingPublish/);
  assert.match(`${inspected.stdout}${inspected.stderr}`, /publish-2-3/);
  await assert.rejects(native.NativeStore.openFile(absentStore), /publish-2-3/);
  assert.equal(fs.existsSync(pending), true, 'absent-store debris is reported, never removed');

  const legacyStore = path.join(evidence, 'legacy.turndb');
  fs.mkdirSync(`${legacyStore}-hot`);
  await assert.rejects(native.NativeStore.openFile(legacyStore), /-hot/);
  assert.equal(fs.existsSync(`${legacyStore}-hot`), true, 'legacy acknowledged-write directory is never removed');

  // Every component stays below NTFS's 255-character limit while the total path exceeds MAX_PATH.
  let deep = path.join(evidence, 'long-path');
  for (let i = 0; i < 14; i += 1) deep = path.join(deep, `segment-${String(i).padStart(2, '0')}-${'x'.repeat(18)}`);
  fs.mkdirSync(deep, { recursive: true });
  const deepStore = path.join(deep, 'store.turndb');
  store = await native.NativeStore.openFile(deepStore);
  await store.write([{ kind: 'put', id: 'deep' }], true);
  await store.close();
  assert(fs.statSync(deepStore).isFile());

  // Exercise Windows zero-data punch through the installed addon. Allocation is evidence, not an
  // assertion: NTFS is allowed to retain physical clusters below its sparse granularity.
  const punchStore = path.join(evidence, 'punch.turndb');
  store = await native.NativeStore.openFile(punchStore, { blockTargetBytes: 65536n, segmentMaxBytes: 1n << 20n });
  for (let round = 0; round < 8; round += 1) {
    const bytes = crypto.randomBytes(192 * 1024);
    await store.write([{ kind: 'put', id: 'replace-me', contents: [{ name: 'body', bytes }] }]);
    await store.flush();
  }
  await store.erase(['replace-me']);
  await store.flush();
  const before = fs.readFileSync(punchStore);
  const beforeSpace = await store.spaceUsage();
  const punched = await store.punch();
  const afterSpace = await store.spaceUsage();
  const after = fs.readFileSync(punchStore);
  assert(punched.blocksPunched > 0n, 'installed Windows addon must exercise zero-data punch');
  let zeroed = 0;
  for (let i = 0; i < Math.min(before.length, after.length); i += 1) {
    if (before[i] !== after[i]) {
      assert.equal(after[i], 0, `punch changed old byte ${i} to a nonzero value`);
      zeroed += 1;
    }
  }
  assert(zeroed > 0, 'punch reported blocks but no old nonzero byte became zero');
  await store.close();

  const importFile = path.join(evidence, 'reference.jsonl');
  fs.writeFileSync(importFile, '{"body":"alpha","platform":"windows"}\n{"body":"beta","platform":"windows"}\n');
  const windowsStore = path.join(evidence, 'windows-cli.turndb');
  run(cli, ['import', windowsStore, importFile]);
  run(cli, ['verify', windowsStore, '--deep']);

  const addonPath = consumerRequire.resolve('@turndb/native-win32-x64-msvc');
  const pythonCode = 'import turndb._native; print(turndb._native.__file__)';
  const pythonExtension = run(installed.Python, ['-c', pythonCode]).trim();
  const imports = [];
  for (const binary of [cli, addonPath, pythonExtension]) {
    const listing = run('llvm-readobj', ['--coff-imports', binary]);
    imports.push(`## ${binary}\nsha256 ${sha256(binary)}\n${listing}`);
  }
  fs.writeFileSync(path.join(evidence, 'pe-imports.txt'), `${imports.join('\n')}\n`);
  fs.writeFileSync(path.join(evidence, 'installed-contract.json'), `${JSON.stringify({
    imageOS: process.env.ImageOS,
    imageVersion: process.env.ImageVersion,
    windowsVersion: require('node:os').version(),
    longPathsEnabled: run('powershell.exe', ['-NoProfile', '-Command', "(Get-ItemProperty 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\FileSystem').LongPathsEnabled"]).trim(),
    versions: [...versions],
    capabilities: caps,
    punch: {
      blocksExamined: punched.blocksExamined.toString(),
      blocksPunched: punched.blocksPunched.toString(),
      zeroedBytesObserved: zeroed,
      allocatedBefore: beforeSpace.allocatedBytes?.toString(),
      allocatedAfter: afterSpace.allocatedBytes?.toString(),
    },
    files: Object.fromEntries([fixture, windowsStore, importFile].map((file) => [path.basename(file), sha256(file)])),
  }, (_, value) => typeof value === 'bigint' ? value.toString() : value, 2)}\n`);
  console.log(`installed Windows artifacts exercised at ${installed.Version}; evidence: ${evidence}`);
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
