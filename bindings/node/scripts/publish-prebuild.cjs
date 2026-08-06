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
const manifest = JSON.parse(
  fs.readFileSync(path.join(dist, 'prebuild-manifest.json'), 'utf8'),
);

if (
  !checkOnly &&
  (process.env.GITHUB_ACTIONS !== 'true' || process.env.TURNDB_RELEASE_APPROVED !== 'true')
) {
  throw new Error('native publication is permitted only in the owner-approved GitHub release job');
}
if (manifest.schema !== 1 || manifest.publishable !== true) {
  throw new Error('prebuild manifest is not a publishable release artifact');
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
const npmVersion = child.execFileSync('npm', ['--version'], { encoding: 'utf8' }).trim();
if (compareVersions(npmVersion, '11.5.1') < 0) {
  throw new Error(`trusted publication requires npm >=11.5.1; found ${npmVersion}`);
}

const expectedTag = `v${manifest.version}`;
if (!checkOnly) {
  const actualTag = child.execFileSync(
    'git', ['describe', '--tags', '--exact-match', 'HEAD'],
    { cwd: root, encoding: 'utf8' },
  ).trim();
  if (actualTag !== expectedTag) {
    throw new Error(
      `release checkout must be exact tag ${expectedTag}; found ${actualTag || 'none'}`,
    );
  }
  const tagType = child.execFileSync('git', ['cat-file', '-t', expectedTag], {
    cwd: root,
    encoding: 'utf8',
  }).trim();
  if (tagType !== 'tag') {
    throw new Error(`${expectedTag} must be an annotated tag; found Git object type ${tagType}`);
  }
}

function digest(file) {
  const bytes = fs.readFileSync(file);
  return {
    bytes: bytes.length,
    sha256: crypto.createHash('sha256').update(bytes).digest('hex'),
  };
}

function packageJson(tarball) {
  return JSON.parse(
    child.execFileSync('tar', ['-xOf', tarball, 'package/package.json'], {
      encoding: 'utf8',
    }),
  );
}

for (const entry of manifest.tarballs) {
  const file = path.join(dist, entry.file);
  const actual = digest(file);
  if (actual.bytes !== entry.bytes || actual.sha256 !== entry.sha256) {
    throw new Error(`${entry.file} does not match the release manifest`);
  }
}

const rootEntry = manifest.tarballs.find(
  (entry) => entry.file === `turndb-native-${manifest.version}.tgz`,
);
const targetEntry = manifest.tarballs.find(
  (entry) => entry.file === `turndb-native-linux-x64-gnu-${manifest.version}.tgz`,
);
if (!rootEntry || !targetEntry) throw new Error('release is missing root or platform tarball');

const rootTarball = path.join(dist, rootEntry.file);
const targetTarball = path.join(dist, targetEntry.file);
const rootPackage = packageJson(rootTarball);
const targetPackage = packageJson(targetTarball);
for (const packageManifest of [rootPackage, targetPackage]) {
  if (packageManifest.version !== manifest.version || packageManifest.private === true) {
    throw new Error(`${packageManifest.name} is not a publishable ${manifest.version} package`);
  }
}
if (rootPackage.name !== '@turndb/native') {
  throw new Error(`unexpected root package ${rootPackage.name}`);
}
if (targetPackage.name !== '@turndb/native-linux-x64-gnu') {
  throw new Error(`unexpected platform package ${targetPackage.name}`);
}
if (rootPackage.optionalDependencies?.[targetPackage.name] !== manifest.version) {
  throw new Error('root package does not select this exact platform-package version');
}

if (checkOnly) {
  console.log(
    `verified publishable ${rootPackage.name}@${manifest.version} and ${targetPackage.name}`,
  );
  process.exit(0);
}

// npm registry publication is not transactional. Publish the dependency first so a visible root
// package never points at a platform package that does not exist. A root-package failure may leave
// an installable but undiscoverable platform package; rerunning at the same version is then refused
// by npm and requires an explicit owner decision rather than automated mutation.
for (const tarball of [targetTarball, rootTarball]) {
  child.execFileSync(
    'npm', ['publish', tarball, '--access', 'public', '--provenance'],
    { cwd: root, stdio: 'inherit' },
  );
}
