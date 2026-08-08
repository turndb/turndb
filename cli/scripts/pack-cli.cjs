#!/usr/bin/env node
'use strict';

// Build the CLI binary for one target and stage it into its platform package, then pack both
// tarballs — the platform package that carries the bytes and the selector that finds them.
//
// The version is read from the platform package rather than passed in, because the lockstep check
// already holds every manifest to one version and a flag here would be a second source of truth.

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

// `--release` clears `private` in the packed manifests. Without it both tarballs stay private, so
// a stray `npm publish` refuses rather than shipping — the same posture the native prebuild takes.
const RELEASE = process.argv.includes('--release');
const CLI_DIR = path.resolve(__dirname, '..');
const ROOT = path.resolve(CLI_DIR, '..');
const TARGET = process.env.TURNDB_CLI_TARGET ?? 'x86_64-unknown-linux-gnu';
const SLICE = process.env.TURNDB_CLI_SLICE ?? 'linux-x64-gnu';
// Slices land in one directory across matrix jobs, so a job must not clear its siblings' output.
const DIST = path.join(CLI_DIR, 'dist');
const SELECTOR_ONLY = process.argv.includes('--selector-only');

function run(cmd, args, opts = {}) {
  execFileSync(cmd, args, { stdio: 'inherit', ...opts });
}

const platformDir = path.join(CLI_DIR, 'npm', SLICE);
if (!fs.existsSync(platformDir)) {
  throw new Error(`no platform package for slice ${SLICE} at ${platformDir}`);
}

// `native-release`, not `release`: the ordinary release profile keeps `debug = true` on purpose so
// developers can profile it, which here means a 1.7 GB binary packing to 335 MiB. The named
// distribution profile is the one whose size tradeoff is explicit — the same reasoning, and the
// same profile, the native addon prebuild uses.
run('cargo', ['build', '--profile', 'native-release', '--target', TARGET, '--bin', 'turndb'], {
  cwd: ROOT,
});
const built = path.join(ROOT, 'target', TARGET, 'native-release', 'turndb');
if (!fs.existsSync(built)) throw new Error(`cargo did not produce ${built}`);

// The binary is the package's whole payload; copying it in rather than symlinking keeps `npm pack`
// from following a link out of the package and shipping nothing.
fs.copyFileSync(built, path.join(platformDir, 'turndb'));
fs.chmodSync(path.join(platformDir, 'turndb'), 0o755);
for (const file of ['LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.html']) {
  fs.copyFileSync(path.join(ROOT, file), path.join(platformDir, file));
  fs.copyFileSync(path.join(ROOT, file), path.join(CLI_DIR, file));
}
fs.copyFileSync(path.join(CLI_DIR, 'README.md'), path.join(platformDir, 'README.md'));

const size = fs.statSync(path.join(platformDir, 'turndb')).size;
console.log(`${SLICE}: turndb binary ${size} bytes`);

fs.mkdirSync(DIST, { recursive: true });

// Toggle `private` around the pack and always put it back, so an interrupted release cannot leave
// a publishable manifest checked out in the tree.
const manifests = [path.join(CLI_DIR, 'package.json'), path.join(platformDir, 'package.json')];
const originals = manifests.map((file) => fs.readFileSync(file, 'utf8'));
try {
  if (RELEASE) {
    for (const file of manifests) {
      const json = JSON.parse(fs.readFileSync(file, 'utf8'));
      delete json.private;
      fs.writeFileSync(file, `${JSON.stringify(json, null, 2)}\n`);
    }
  }
  if (!SELECTOR_ONLY) {
    run('npm', ['pack', platformDir, '--pack-destination', DIST]);
  }
  // The selector is packed once, by whichever invocation is told to; a matrix job packing it
  // would race its siblings for the same filename.
  if (SELECTOR_ONLY || process.env.TURNDB_CLI_PACK_SELECTOR === '1') {
    run('npm', ['pack', CLI_DIR, '--pack-destination', DIST]);
  }
} finally {
  manifests.forEach((file, i) => fs.writeFileSync(file, originals[i]));
}

// Every pin the selector carries must name a package this repository can actually build, and at
// the same version. npm reports a missing OPTIONAL dependency as a successful install, so a pin
// that resolves to nothing produces a launcher that can never find its binary and says nothing.
const selector = JSON.parse(fs.readFileSync(path.join(CLI_DIR, 'package.json'), 'utf8'));
const platform = JSON.parse(fs.readFileSync(path.join(platformDir, 'package.json'), 'utf8'));
for (const [name, pinned] of Object.entries(selector.optionalDependencies ?? {})) {
  const slice = name.replace('@turndb/cli-', '');
  const manifestPath = path.join(CLI_DIR, 'npm', slice, 'package.json');
  if (!fs.existsSync(manifestPath)) {
    throw new Error(`selector pins ${name} but cli/npm/${slice} does not exist`);
  }
  const declared = JSON.parse(fs.readFileSync(manifestPath, 'utf8')).version;
  if (pinned !== declared) {
    throw new Error(
      `selector pins ${name}@${pinned} but that package declares ${declared}; `
        + 'the lockstep version sync did not cover one of them',
    );
  }
}
if (selector.optionalDependencies?.[platform.name] === undefined) {
  throw new Error(`the selector does not pin ${platform.name}, so this slice is unreachable`);
}
console.log(`packed ${fs.readdirSync(DIST).sort().join(', ')}`);
