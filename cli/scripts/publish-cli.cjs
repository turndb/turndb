'use strict';

const child = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..', '..');
const args = process.argv.slice(2);
const checkOnly = args.includes('--check');
const paths = args.filter((argument) => argument !== '--check');
if (paths.length > 1) throw new Error('usage: publish-cli.cjs [dist] [--check]');
const dist = path.resolve(paths[0] || path.join(root, 'cli', 'dist'));
if (!checkOnly &&
    (process.env.GITHUB_ACTIONS !== 'true' || process.env.TURNDB_RELEASE_APPROVED !== 'true')) {
  throw new Error('CLI publication is permitted only in the owner-approved GitHub release job');
}

function digest(file) {
  const bytes = fs.readFileSync(file);
  return { file: path.basename(file), bytes: bytes.length,
    sha256: crypto.createHash('sha256').update(bytes).digest('hex') };
}

function packageJson(tarball) {
  return JSON.parse(
    child.execFileSync('tar', ['-xOf', tarball, 'package/package.json'], { encoding: 'utf8' }),
  );
}

const slices = [
  'linux-x64-gnu', 'linux-arm64-gnu', 'darwin-x64', 'darwin-arm64', 'win32-x64-msvc',
];
const expectedManifests = slices.map((slice) => `cli-manifest-${slice}.json`).sort();
const actualManifests = fs.readdirSync(dist)
  .filter((name) => /^cli-manifest-.+\.json$/.test(name)).sort();
if (JSON.stringify(actualManifests) !== JSON.stringify(expectedManifests)) {
  throw new Error(`CLI manifest set differs: expected ${expectedManifests}; got ${actualManifests}`);
}
const manifests = actualManifests.map((name) =>
  JSON.parse(fs.readFileSync(path.join(dist, name), 'utf8')));
for (const [index, manifest] of manifests.entries()) {
  if (manifest.schema !== 1 || manifest.component !==
      `cli-${actualManifests[index].slice('cli-manifest-'.length, -'.json'.length)}` ||
      !manifest.files?.length) {
    throw new Error(`unsupported or incomplete CLI manifest ${actualManifests[index]}`);
  }
}
const versions = [...new Set(manifests.map(({ version }) => version))];
const commits = [...new Set(manifests.map(({ sourceCommit }) => sourceCommit))];
if (versions.length !== 1 || commits.length !== 1) {
  throw new Error(`CLI slices disagree on version/commit: ${versions} / ${commits}`);
}
const version = versions[0];
const entriesByFile = new Map();
for (const entry of manifests.flatMap(({ files }) => files)) {
  const prior = entriesByFile.get(entry.file);
  if (prior && JSON.stringify(prior) !== JSON.stringify(entry)) {
    throw new Error(`CLI manifests disagree on duplicate artifact ${entry.file}`);
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

const selectorName = `turndb-cli-${version}.tgz`;
const selectorEntry = entries.find(({ file }) => file === selectorName);
if (!selectorEntry) throw new Error(`CLI release is missing selector ${selectorName}`);
const selectorTarball = path.join(dist, selectorName);
const selector = packageJson(selectorTarball);
if (selector.name !== '@turndb/cli' || selector.version !== version || selector.private === true) {
  throw new Error(`unexpected or private CLI selector ${selector.name}@${selector.version}`);
}
const platformTarballs = [];
for (const [packageName, pinned] of Object.entries(selector.optionalDependencies || {}).sort()) {
  if (pinned !== version) throw new Error(`selector pin ${packageName}@${pinned} differs from ${version}`);
  const slice = packageName.replace('@turndb/cli-', '');
  const filename = `turndb-cli-${slice}-${version}.tgz`;
  if (!entries.some(({ file }) => file === filename)) {
    throw new Error(`refusing before publication: required platform tarball is absent: ${filename}`);
  }
  const platform = packageJson(path.join(dist, filename));
  if (platform.name !== packageName || platform.version !== version || platform.private === true) {
    throw new Error(`unexpected or private CLI platform package in ${filename}`);
  }
  platformTarballs.push(path.join(dist, filename));
}
if (platformTarballs.length !== slices.length) {
  throw new Error(`expected ${slices.length} CLI platforms, got ${platformTarballs.length}`);
}

if (!checkOnly) {
  const releaseRef = process.env.RELEASE_REF;
  if (releaseRef !== `v${version}`) throw new Error(`release ref ${releaseRef} differs from v${version}`);
  const actualTag = child.execFileSync(
    'git', ['describe', '--tags', '--exact-match', 'HEAD'], { cwd: root, encoding: 'utf8' },
  ).trim();
  const tagType = child.execFileSync(
    'git', ['cat-file', '-t', releaseRef], { cwd: root, encoding: 'utf8' },
  ).trim();
  if (actualTag !== releaseRef || tagType !== 'tag') {
    throw new Error(`CLI release requires exact annotated tag ${releaseRef}`);
  }
  for (const packageName of [...Object.keys(selector.optionalDependencies), selector.name]) {
    const exists = child.spawnSync(
      'npm', ['view', `${packageName}@${version}`, 'version', '--json'], { encoding: 'utf8' },
    );
    if (exists.status === 0) {
      throw new Error(`refusing release rerun before publication: ${packageName}@${version} exists`);
    }
    const outputs = [exists.stdout, exists.stderr].filter((output) => output?.trim());
    const definitelyAbsent = outputs.some((output) => {
      try { return JSON.parse(output).error?.code === 'E404'; } catch { return false; }
    });
    if (!definitelyAbsent) {
      throw new Error(
        `refusing publication: registry did not prove ${packageName}@${version} absent: `
          + (outputs.join('\n') || exists.error?.message || `npm exited ${exists.status}`),
      );
    }
  }
}
if (checkOnly) {
  console.log(`verified CLI ${version}, ${platformTarballs.length} platforms, commit ${commits[0]}`);
  process.exit(0);
}
for (const tarball of [...platformTarballs, selectorTarball]) {
  child.execFileSync(
    'npm', ['publish', tarball, '--access', 'public', '--provenance'],
    { cwd: root, stdio: 'inherit' },
  );
}
