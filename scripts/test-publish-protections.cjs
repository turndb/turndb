'use strict';

const assert = require('node:assert/strict');
const child = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {
  readPrebuildManifestSet,
  selectInstallTarballs,
} = require('../bindings/node/scripts/prebuild-manifest-set.cjs');

const root = path.resolve(__dirname, '..');
const version = JSON.parse(fs.readFileSync(path.join(root, 'bindings/node/package.json'))).version;
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-publish-protection-'));

function digest(file) {
  const bytes = fs.readFileSync(file);
  return { file: path.basename(file), bytes: bytes.length,
    sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
}

function pack(dist, name, extra, payload) {
  const source = fs.mkdtempSync(path.join(work, 'package-'));
  fs.writeFileSync(path.join(source, 'package.json'), JSON.stringify({
    name, version, files: [payload], ...extra,
  }));
  fs.writeFileSync(path.join(source, payload), 'published-shaped fixture');
  const report = JSON.parse(child.execFileSync(
    'npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', dist, '.'],
    { cwd: source, encoding: 'utf8' },
  ))[0];
  return path.join(dist, report.filename);
}

function writeJson(file, value) {
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

const NATIVE_SLICES = [
  { npmTarget: 'darwin-arm64', rustTarget: 'aarch64-apple-darwin', os: 'darwin', cpu: 'arm64' },
  { npmTarget: 'darwin-x64', rustTarget: 'x86_64-apple-darwin', os: 'darwin', cpu: 'x64' },
  { npmTarget: 'linux-arm64-gnu', rustTarget: 'aarch64-unknown-linux-gnu', os: 'linux', cpu: 'arm64',
    libc: 'glibc', glibcRequired: '2.17' },
  { npmTarget: 'linux-x64-gnu', rustTarget: 'x86_64-unknown-linux-gnu', os: 'linux', cpu: 'x64',
    libc: 'glibc', glibcRequired: '2.17' },
  { npmTarget: 'win32-x64-msvc', rustTarget: 'x86_64-pc-windows-msvc', os: 'win32', cpu: 'x64' },
];

function nativeFixture() {
  const dist = path.join(work, 'native');
  fs.mkdirSync(dist);
  const optionalDependencies = Object.fromEntries(
    NATIVE_SLICES.map(({ npmTarget }) => [`@turndb/native-${npmTarget}`, version]),
  );
  const selector = pack(dist, '@turndb/native', { optionalDependencies }, 'index.cjs');
  const common = { schema: 2, package: '@turndb/native', version, sourceCommit: 'fixture',
    nodeApi: 6, publishable: true };
  for (const slice of NATIVE_SLICES) {
    const platform = pack(
      dist, `@turndb/native-${slice.npmTarget}`,
      { os: [slice.os], cpu: [slice.cpu], ...(slice.libc ? { libc: [slice.libc] } : {}) },
      `turndb.${slice.npmTarget}.node`,
    );
    // The selector rides in exactly one slice's manifest — the reference slice's — as the release
    // build packs it; the manifest-set reader's union is what makes every other host installable.
    writeJson(path.join(dist, `prebuild-manifest-${slice.npmTarget}.json`), {
      ...common, rustTarget: slice.rustTarget, npmTarget: slice.npmTarget,
      glibcRequired: slice.glibcRequired ?? null,
      tarballs: [digest(platform), ...(slice.npmTarget === 'linux-x64-gnu' ? [digest(selector)] : [])],
    });
  }
  return dist;
}

function cliFixture() {
  const dist = path.join(work, 'cli');
  fs.mkdirSync(dist);
  const slices = [
    'linux-x64-gnu', 'linux-arm64-gnu', 'darwin-x64', 'darwin-arm64', 'win32-x64-msvc',
  ];
  const optionalDependencies = Object.fromEntries(
    slices.map((slice) => [`@turndb/cli-${slice}`, version]),
  );
  const selector = pack(dist, '@turndb/cli', { optionalDependencies }, 'turndb.cjs');
  for (const slice of slices) {
    const executable = slice.startsWith('win32') ? 'turndb.exe' : 'turndb';
    const platform = pack(
      dist, `@turndb/cli-${slice}`,
      { os: [slice.startsWith('win32') ? 'win32' : slice.startsWith('darwin') ? 'darwin' : 'linux'] },
      executable,
    );
    writeJson(path.join(dist, `cli-manifest-${slice}.json`), {
      schema: 1, component: `cli-${slice}`, version, sourceCommit: 'fixture',
      files: [digest(platform), ...(slice === 'linux-x64-gnu' ? [digest(selector)] : [])],
    });
  }
  return dist;
}

function fakeTools() {
  const bin = path.join(work, 'bin');
  fs.mkdirSync(bin);
  fs.writeFileSync(path.join(bin, 'git'), `#!/bin/sh
if [ "$1" = describe ]; then
  [ "$TURNDB_TEST_UNTAGGED" = 1 ] && echo v0.0.0 || echo v${version}
elif [ "$1" = cat-file ]; then
  [ "$TURNDB_TEST_LIGHTWEIGHT" = 1 ] && echo commit || echo tag
else exit 2
fi
`);
  fs.writeFileSync(path.join(bin, 'npm'), `#!/bin/sh
echo "$*" >> "$TURNDB_TEST_CALLS"
if [ "$1" = --version ]; then echo 11.5.1; exit 0; fi
if [ "$1" = view ]; then
  [ "$TURNDB_TEST_EXISTS" = 1 ] && echo ${version} && exit 0
  [ "$TURNDB_TEST_INCONCLUSIVE" = 1 ] && echo 'registry unavailable' >&2 && exit 1
  echo '{"error":{"code":"E404"}}' >&2
  exit 1
fi
if [ "$1" = publish ]; then exit 0; fi
exit 2
`);
  fs.chmodSync(path.join(bin, 'git'), 0o755);
  fs.chmodSync(path.join(bin, 'npm'), 0o755);
  return bin;
}

function invoke(script, dist, mode = {}) {
  const calls = path.join(work, `calls-${crypto.randomUUID()}`);
  fs.writeFileSync(calls, '');
  const result = child.spawnSync(process.execPath, [script, dist], {
    cwd: root,
    encoding: 'utf8',
    env: {
      ...process.env,
      PATH: `${tools}${path.delimiter}${process.env.PATH}`,
      GITHUB_ACTIONS: 'true',
      TURNDB_RELEASE_APPROVED: 'true',
      RELEASE_REF: `v${version}`,
      TURNDB_TEST_CALLS: calls,
      ...mode,
    },
  });
  return { result, calls: fs.readFileSync(calls, 'utf8').trim().split('\n').filter(Boolean) };
}

function copyDirectory(source, tag) {
  const destination = path.join(work, tag);
  fs.cpSync(source, destination, { recursive: true });
  return destination;
}

const tools = fakeTools();
try {
  const splitNative = nativeFixture();
  const splitSet = readPrebuildManifestSet(splitNative);
  assert.equal(splitSet.manifests.get('win32-x64-msvc').tarballs.length, 1,
    'fixture must keep the selector in the other slice manifest');
  const windowsOnlyEntries = new Map(
    splitSet.manifests.get('win32-x64-msvc').tarballs.map((entry) => [entry.file, entry]),
  );
  assert.throws(
    () => selectInstallTarballs(windowsOnlyEntries, 'win32-x64-msvc', version),
    /prebuild manifest does not contain both root and win32-x64-msvc packages/,
  );
  const selected = selectInstallTarballs(splitSet.entries, 'win32-x64-msvc', version);
  assert.equal(selected.rootTarball.file, `turndb-native-${version}.tgz`);
  assert.equal(selected.targetTarball.file, `turndb-native-win32-x64-msvc-${version}.tgz`);

  const conflictingNative = copyDirectory(splitNative, 'native-conflicting-manifest');
  const windowsManifestPath = path.join(
    conflictingNative, 'prebuild-manifest-win32-x64-msvc.json');
  const windowsManifest = JSON.parse(fs.readFileSync(windowsManifestPath));
  windowsManifest.tarballs.push({
    ...splitSet.entries.get(`turndb-native-${version}.tgz`),
    sha256: '0'.repeat(64),
  });
  writeJson(windowsManifestPath, windowsManifest);
  assert.throws(
    () => readPrebuildManifestSet(conflictingNative),
    /disagree on duplicate tarball/,
  );

  const mismatchedNative = copyDirectory(splitNative, 'native-mismatched-manifest');
  const mismatchedManifestPath = path.join(
    mismatchedNative, 'prebuild-manifest-win32-x64-msvc.json');
  const mismatchedManifest = JSON.parse(fs.readFileSync(mismatchedManifestPath));
  mismatchedManifest.sourceCommit = 'different-build';
  writeJson(mismatchedManifestPath, mismatchedManifest);
  assert.throws(
    () => readPrebuildManifestSet(mismatchedNative),
    /disagree on version\/source commit/,
  );

  for (const [name, script, fixture, selectorPrefix] of [
    ['native', path.join(root, 'bindings/node/scripts/publish-prebuild.cjs'),
      splitNative, 'turndb-native-'],
    ['cli', path.join(root, 'cli/scripts/publish-cli.cjs'), cliFixture(), 'turndb-cli-'],
  ]) {
    const valid = invoke(script, fixture);
    assert.equal(valid.result.status, 0, valid.result.stderr);
    const publishes = valid.calls.filter((call) => call.startsWith('publish '));
    assert(publishes.length >= 3, `${name}: valid fixture never reached the fake publisher`);
    assert.match(publishes.at(-1), new RegExp(`${selectorPrefix}${version}\\.tgz`));

    const missing = copyDirectory(fixture, `${name}-missing`);
    const platform = fs.readdirSync(missing)
      .find((file) => file.startsWith(selectorPrefix) && file.includes('win32') && file.endsWith('.tgz'));
    fs.rmSync(path.join(missing, platform));
    const refusedMissing = invoke(script, missing);
    assert.notEqual(refusedMissing.result.status, 0);
    assert.equal(refusedMissing.calls.filter((call) => call.startsWith('publish ')).length, 0);

    for (const mode of [
      { TURNDB_TEST_EXISTS: '1' },
      { TURNDB_TEST_INCONCLUSIVE: '1' },
      { TURNDB_TEST_LIGHTWEIGHT: '1' },
      { TURNDB_TEST_UNTAGGED: '1' },
    ]) {
      const refusal = invoke(script, fixture, mode);
      assert.notEqual(refusal.result.status, 0, `${name}: protection mode unexpectedly passed`);
      assert.equal(refusal.calls.filter((call) => call.startsWith('publish ')).length, 0);
    }
  }
  console.log('publish protections: complete set publishes platform-first; missing, rerun, inconclusive registry, lightweight and untagged refuse before publish');
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
