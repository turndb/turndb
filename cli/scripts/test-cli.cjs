#!/usr/bin/env node
'use strict';

// Install the exact packed tarballs into a throwaway project and drive the CLI the way a consumer
// would — through `node_modules/.bin/turndb`, not the built binary. The launcher's resolution of
// its platform package is the part most likely to be wrong, and only an install exercises it.

const assert = require('node:assert/strict');
const { execFileSync, spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { npmCommand } = require('../../scripts/npm-command.cjs');

const CLI_DIR = path.resolve(__dirname, '..');
const DIST = process.argv[2] ? path.resolve(process.argv[2]) : path.join(CLI_DIR, 'dist');

const version = JSON.parse(fs.readFileSync(path.join(CLI_DIR, 'package.json'), 'utf8')).version;
const hostSlice = process.env.TURNDB_CLI_TEST_SLICE ?? (() => {
  if (process.platform === 'linux' && process.arch === 'x64') return 'linux-x64-gnu';
  if (process.platform === 'linux' && process.arch === 'arm64') return 'linux-arm64-gnu';
  if (process.platform === 'darwin' && process.arch === 'x64') return 'darwin-x64';
  if (process.platform === 'darwin' && process.arch === 'arm64') return 'darwin-arm64';
  if (process.platform === 'win32' && process.arch === 'x64') return 'win32-x64-msvc';
  throw new Error(`no CLI test slice for ${process.platform}-${process.arch}`);
})();
const tarballs = [
  `turndb-cli-${version}.tgz`,
  `turndb-cli-${hostSlice}-${version}.tgz`,
];
for (const tarball of tarballs) {
  assert.ok(fs.existsSync(path.join(DIST, tarball)), `expected ${tarball} in ${DIST}`);
}

const work = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-cli-test-'));
try {
  let npm = npmCommand(['init', '-y']);
  execFileSync(npm.file, npm.args, { cwd: work, stdio: 'ignore' });
  npm = npmCommand([
    'install', '--no-audit', '--no-fund', ...tarballs.map((f) => path.join(DIST, f)),
  ]);
  execFileSync(
    npm.file,
    npm.args,
    { cwd: work, stdio: 'inherit' },
  );

  const shim = path.join(
    work, 'node_modules', '.bin', process.platform === 'win32' ? 'turndb.cmd' : 'turndb',
  );
  const launcher = path.join(work, 'node_modules', '@turndb', 'cli', 'bin', 'turndb.cjs');
  assert.ok(fs.existsSync(shim), 'the selector must install a turndb bin');
  assert.ok(fs.existsSync(launcher), 'the selector must install its JS launcher');

  const detail = (result) => result.error?.stack || result.stderr || `signal ${result.signal}`;
  // Node 24 deliberately refuses to spawn Windows .cmd files without a shell. Exercise the shim
  // users type once with the fixed literal `help`; keep every path-bearing functional assertion
  // below on Node's argument-array boundary by invoking the installed JS launcher directly.
  const help = process.platform === 'win32'
    ? spawnSync(shim, ['help'], { encoding: 'utf8', cwd: work, shell: true })
    : spawnSync(shim, ['help'], { encoding: 'utf8', cwd: work });
  assert.equal(help.status, 0, `help exited ${help.status}: ${detail(help)}`);
  assert.match(help.stdout, /database for AI traces, in one file/, 'help must be the real usage text');

  const cli = (args, opts = {}) => spawnSync(
    process.execPath, [launcher, ...args], { encoding: 'utf8', cwd: work, ...opts },
  );

  // A real store, so the install is exercised as a store tool rather than as a binary that runs.
  const jsonl = path.join(work, 'traces.jsonl');
  fs.writeFileSync(
    jsonl,
    [0, 1, 2]
      .map((i) => JSON.stringify({ body: JSON.stringify([{ role: 'user', content: `t${i}` }]), model: `m${i % 2}` }))
      .join('\n') + '\n',
  );
  const store = path.join(work, 'store.turndb');
  const imported = cli(['import', store, jsonl]);
  assert.equal(imported.status, 0, `import exited ${imported.status}: ${detail(imported)}`);

  const ids = cli(['ids', store]);
  assert.equal(ids.status, 0, `ids exited ${ids.status}: ${detail(ids)}`);
  assert.equal(ids.stdout.trim().split('\n').length, 3, 'every imported record must be listed');

  const verified = cli(['verify', store, '--deep']);
  assert.equal(verified.status, 0, `verify exited ${verified.status}: ${detail(verified)}`);
  assert.match(verified.stdout, /reconstruct byte-exact/, 'deep verify must report reconstruction');

  // The store IS the single-file form; sealing ships its snapshot with the same binary.
  const sealedOut = path.join(work, 'snapshot.turndb');
  const sealed = cli(['seal', store, sealedOut]);
  assert.equal(sealed.status, 0, `seal exited ${sealed.status}: ${detail(sealed)}`);
  const inspected = cli(['inspect', sealedOut]);
  assert.equal(inspected.status, 0, `inspect exited ${inspected.status}: ${detail(inspected)}`);
  assert.match(inspected.stdout, /\(sealed\)/, 'a sealed snapshot must be reported as one');

  // A refusal must be a refusal: nonzero status and a message on stderr, not a stack trace.
  const missing = cli(['inspect', path.join(work, 'nope')]);
  assert.notEqual(missing.status, 0, 'a missing store must exit nonzero');
  assert.match(missing.stderr, /^turndb: /m, 'errors must carry the CLI prefix');

  console.log('cli install: help, import, ids, verify --deep, seal, inspect, refusal');
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
