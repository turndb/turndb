'use strict';

const child = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const cliArguments = process.argv.slice(2);
const checkOnly = cliArguments.includes('--check');
const paths = cliArguments.filter((argument) => argument !== '--check');
if (paths.length > 1) throw new Error('usage: publish-prebuild.cjs [dist] [--check]');
const dist = path.resolve(paths[0] || path.join(root, 'dist'));

if (!checkOnly &&
    (process.env.GITHUB_ACTIONS !== 'true' || process.env.TURNDB_RELEASE_APPROVED !== 'true')) {
  throw new Error('native publication is permitted only in the owner-approved GitHub release job');
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

function digest(file) {
  const bytes = fs.readFileSync(file);
  return {
    file: path.basename(file),
    bytes: bytes.length,
    sha256: crypto.createHash('sha256').update(bytes).digest('hex'),
  };
}

function packageJson(tarball) {
  return JSON.parse(
    child.execFileSync('tar', ['-xOf', tarball, 'package/package.json'], { encoding: 'utf8' }),
  );
}

const expectedTargets = ['linux-x64-gnu', 'win32-x64-msvc'];
const manifestNames = fs.readdirSync(dist)
  .filter((name) => /^prebuild-manifest-.+\.json$/.test(name))
  .sort();
const expectedManifestNames = expectedTargets.map((target) => `prebuild-manifest-${target}.json`);
if (JSON.stringify(manifestNames) !== JSON.stringify(expectedManifestNames)) {
  throw new Error(
    `native release manifest set differs: expected ${expectedManifestNames}; got ${manifestNames}`,
  );
}
const manifests = manifestNames.map((name) =>
  JSON.parse(fs.readFileSync(path.join(dist, name), 'utf8')));
for (const [index, manifest] of manifests.entries()) {
  if (manifest.schema !== 2 || manifest.publishable !== true ||
      manifest.npmTarget !== expectedTargets[index] || !manifest.tarballs?.length) {
    throw new Error(`unsupported or incomplete native manifest ${manifestNames[index]}`);
  }
}
const versions = [...new Set(manifests.map(({ version }) => version))];
const commits = [...new Set(manifests.map(({ sourceCommit }) => sourceCommit))];
if (versions.length !== 1 || commits.length !== 1) {
  throw new Error(`native slices disagree on version/commit: ${versions} / ${commits}`);
}
const version = versions[0];

const entriesByFile = new Map();
for (const entry of manifests.flatMap(({ tarballs }) => tarballs)) {
  const prior = entriesByFile.get(entry.file);
  if (prior && JSON.stringify(prior) !== JSON.stringify(entry)) {
    throw new Error(`native manifests disagree on duplicate tarball ${entry.file}`);
  }
  entriesByFile.set(entry.file, entry);
}
const entries = [...entriesByFile.values()];
for (const expected of entries) {
  const actual = digest(path.join(dist, expected.file));
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(`${expected.file} does not match the build digest manifest`);
  }
}

const selectorName = `turndb-native-${version}.tgz`;
const selectorEntry = entries.find(({ file }) => file === selectorName);
if (!selectorEntry) throw new Error(`native release is missing selector ${selectorName}`);
const selectorTarball = path.join(dist, selectorEntry.file);
const selector = packageJson(selectorTarball);
if (selector.name !== '@turndb/native' || selector.version !== version || selector.private === true) {
  throw new Error(`unexpected or private native selector ${selector.name}@${selector.version}`);
}

const platformTarballs = [];
for (const [packageName, pinned] of Object.entries(selector.optionalDependencies || {}).sort()) {
  if (pinned !== version) throw new Error(`selector pin ${packageName}@${pinned} differs from ${version}`);
  const npmTarget = packageName.replace('@turndb/native-', '');
  const filename = `turndb-native-${npmTarget}-${version}.tgz`;
  const entry = entries.find(({ file }) => file === filename);
  if (!entry) {
    throw new Error(`refusing before publication: required platform tarball is absent: ${filename}`);
  }
  const platform = packageJson(path.join(dist, filename));
  if (platform.name !== packageName || platform.version !== version || platform.private === true) {
    throw new Error(`unexpected or private platform package in ${filename}`);
  }
  platformTarballs.push(path.join(dist, filename));
}
if (platformTarballs.length !== expectedTargets.length) {
  throw new Error(`expected ${expectedTargets.length} native platform packages, got ${platformTarballs.length}`);
}

const npmVersion = child.execFileSync('npm', ['--version'], { encoding: 'utf8' }).trim();
if (compareVersions(npmVersion, '11.5.1') < 0) {
  throw new Error(`trusted publication requires npm >=11.5.1; found ${npmVersion}`);
}

const expectedTag = `v${version}`;
if (!checkOnly) {
  const actualTag = child.execFileSync(
    'git', ['describe', '--tags', '--exact-match', 'HEAD'],
    { cwd: root, encoding: 'utf8' },
  ).trim();
  if (actualTag !== expectedTag) {
    throw new Error(`release checkout must be exact tag ${expectedTag}; found ${actualTag || 'none'}`);
  }
  const tagType = child.execFileSync(
    'git', ['cat-file', '-t', expectedTag], { cwd: root, encoding: 'utf8' },
  ).trim();
  if (tagType !== 'tag') throw new Error(`${expectedTag} must be an annotated tag; found ${tagType}`);

  // A rerun must not reach the first registry write. npm's registry is immutable, but refusing
  // before any publish call makes the retry state explicit rather than partially replaying it.
  for (const packageName of [...Object.keys(selector.optionalDependencies), selector.name]) {
    const exists = child.spawnSync(
      'npm', ['view', `${packageName}@${version}`, 'version', '--json'],
      { cwd: root, encoding: 'utf8' },
    );
    if (exists.status === 0) {
      throw new Error(`refusing release rerun before publication: ${packageName}@${version} exists`);
    }
  }
}

if (checkOnly) {
  console.log(
    `verified publishable ${selector.name}@${version}, ${platformTarballs.length} platforms, `
      + `commit ${commits[0]}`,
  );
  process.exit(0);
}

// npm publication is not transactional: publish every platform before the selector that points at
// them. The complete-set and rerun preflights above execute before this first registry write.
for (const tarball of [...platformTarballs, selectorTarball]) {
  child.execFileSync(
    'npm', ['publish', tarball, '--access', 'public', '--provenance'],
    { cwd: root, stdio: 'inherit' },
  );
}
