'use strict';

const child = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const cliArguments = new Set(process.argv.slice(2));
for (const argument of cliArguments) {
  if (argument !== '--release') throw new Error(`unknown argument: ${argument}`);
}
const release = cliArguments.has('--release');

const root = path.resolve(__dirname, '..');
const workspace = path.resolve(root, '..', '..');
const target = 'linux-x64-gnu';
const targetDir = path.join(root, 'npm', target);
const artifactName = `turndb.${target}.node`;
const artifact = path.join(targetDir, artifactName);
const dist = path.join(root, 'dist');

if (!fs.statSync(artifact, { throwIfNoEntry: false })?.isFile()) {
  throw new Error(
    `${artifact} is absent; build and collect the configured prebuild before packing`,
  );
}

fs.rmSync(dist, { recursive: true, force: true });
fs.mkdirSync(dist, { recursive: true });
const npmCache = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-native-pack-cache-'));
const packWorkspace = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-native-pack-'));

function stagePackage(sourceDir, destination, files) {
  fs.mkdirSync(destination, { recursive: true });
  for (const file of files) {
    fs.copyFileSync(path.join(sourceDir, file), path.join(destination, file));
  }
  fs.copyFileSync(path.join(workspace, 'LICENSE'), path.join(destination, 'LICENSE'));
  fs.copyFileSync(path.join(workspace, 'NOTICE'), path.join(destination, 'NOTICE'));
  fs.copyFileSync(
    path.join(workspace, 'THIRD_PARTY_LICENSES.html'),
    path.join(destination, 'THIRD_PARTY_LICENSES.html'),
  );

  // napi-rs owns the generated platform metadata and may rewrite its `files` list. Make the legal
  // payload an invariant of the actual staging directory instead of relying on generated metadata
  // retaining local additions.
  const packagePath = path.join(destination, 'package.json');
  const packageJson = JSON.parse(fs.readFileSync(packagePath, 'utf8'));
  packageJson.files = [
    ...new Set([
      ...(packageJson.files || []),
      'LICENSE',
      'NOTICE',
      'THIRD_PARTY_LICENSES.html',
    ]),
  ];
  packageJson.private = !release;
  fs.writeFileSync(packagePath, `${JSON.stringify(packageJson, null, 2)}\n`);
}

const rootStage = path.join(packWorkspace, 'root');
const targetStage = path.join(packWorkspace, target);

function pack(packageDir) {
  const result = child.spawnSync(
    'npm',
    ['pack', '--ignore-scripts', '--json', '--pack-destination', dist, '.'],
    {
      // Packing an external directory argument goes through npm's package-spec resolver. Run in the
      // isolated staging directory so optional platform dependencies remain metadata, not registry
      // lookups, and the report describes only the bytes we staged.
      cwd: packageDir,
      encoding: 'utf8',
      env: { ...process.env, npm_config_cache: npmCache },
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );
  if (result.error || result.status !== 0) {
    throw new Error(
      `npm pack failed for ${packageDir} (${result.status}): ${result.stderr || result.error}`,
    );
  }
  if (!result.stdout.trim()) {
    throw new Error(`npm pack returned no report for ${packageDir}: ${result.stderr}`);
  }
  const reports = JSON.parse(result.stdout);
  if (!Array.isArray(reports) || reports.length !== 1) {
    throw new Error(`npm pack returned ${reports.length} reports for ${packageDir}`);
  }
  return reports[0];
}

let targetPack;
let rootPack;
try {
  stagePackage(
    root,
    rootStage,
    ['package.json', 'index.cjs', 'index.mjs', 'index.d.ts', 'README.md'],
  );
  stagePackage(targetDir, targetStage, ['package.json', 'README.md', artifactName]);
  targetPack = pack(targetStage);
  rootPack = pack(rootStage);
} finally {
  fs.rmSync(npmCache, { recursive: true, force: true });
  fs.rmSync(packWorkspace, { recursive: true, force: true });
}
const targetNodes = targetPack.files.filter((file) => file.path.endsWith('.node'));
if (targetNodes.length !== 1 || targetNodes[0].path !== artifactName) {
  throw new Error(`platform tarball must contain exactly ${artifactName}`);
}
if (rootPack.files.some((file) => file.path.endsWith('.node'))) {
  throw new Error('root tarball must stay platform-neutral; native bytes belong in optional packages');
}
for (const required of ['index.cjs', 'index.mjs', 'index.d.ts', 'README.md', 'package.json']) {
  if (!rootPack.files.some((file) => file.path === required)) {
    throw new Error(`root tarball is missing ${required}`);
  }
}
for (const report of [rootPack, targetPack]) {
  for (const required of ['LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.html']) {
    if (!report.files.some((file) => file.path === required)) {
      throw new Error(`${report.name} tarball is missing ${required}`);
    }
  }
}

function digest(file) {
  const bytes = fs.readFileSync(file);
  return {
    file: path.basename(file),
    bytes: bytes.length,
    sha256: crypto.createHash('sha256').update(bytes).digest('hex'),
  };
}

function versionParts(version) {
  return version.split('.').map((part) => Number.parseInt(part, 10));
}

function compareVersions(left, right) {
  const a = versionParts(left);
  const b = versionParts(right);
  for (let index = 0; index < Math.max(a.length, b.length); index += 1) {
    const difference = (a[index] || 0) - (b[index] || 0);
    if (difference !== 0) return difference;
  }
  return 0;
}

const versionInfo = child.execFileSync('readelf', ['--version-info', artifact], {
  encoding: 'utf8',
});
const glibcVersions = [...versionInfo.matchAll(/\bGLIBC_(\d+(?:\.\d+)+)\b/g)]
  .map((match) => match[1])
  .sort(compareVersions);
const glibcRequired = glibcVersions.at(-1);
if (!glibcRequired) {
  throw new Error(`${artifactName} exposes no readable GLIBC symbol-version requirement`);
}
const allowedGlibc = process.env.TURNDB_MAX_GLIBC;
if (allowedGlibc && compareVersions(glibcRequired, allowedGlibc) > 0) {
  throw new Error(
    `${artifactName} requires GLIBC_${glibcRequired}, above the declared GLIBC_${allowedGlibc} floor`,
  );
}

const packageJson = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
const manifest = {
  schema: 1,
  package: packageJson.name,
  version: packageJson.version,
  nodeApi: 6,
  rustTarget: 'x86_64-unknown-linux-gnu',
  npmTarget: target,
  publishable: release,
  glibcRequired,
  binary: digest(artifact),
  tarballs: [
    digest(path.join(dist, rootPack.filename)),
    digest(path.join(dist, targetPack.filename)),
  ],
};
fs.writeFileSync(path.join(dist, 'prebuild-manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);

console.log(
  `packed ${manifest.package}@${manifest.version} (${release ? 'release' : 'private'}): ` +
    `${manifest.binary.bytes} native bytes, ${manifest.tarballs.length} tarballs`,
);
