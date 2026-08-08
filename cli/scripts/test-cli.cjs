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

const CLI_DIR = path.resolve(__dirname, '..');
const DIST = process.argv[2] ? path.resolve(process.argv[2]) : path.join(CLI_DIR, 'dist');

const tarballs = fs.readdirSync(DIST).filter((f) => f.endsWith('.tgz'));
assert.ok(tarballs.length >= 2, `expected a selector and a platform tarball in ${DIST}`);

const work = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-cli-test-'));
try {
  execFileSync('npm', ['init', '-y'], { cwd: work, stdio: 'ignore' });
  execFileSync(
    'npm',
    ['install', '--no-audit', '--no-fund', ...tarballs.map((f) => path.join(DIST, f))],
    { cwd: work, stdio: 'inherit' },
  );

  const turndb = path.join(work, 'node_modules', '.bin', 'turndb');
  assert.ok(fs.existsSync(turndb), 'the selector must install a turndb bin');

  const cli = (args, opts = {}) => spawnSync(turndb, args, { encoding: 'utf8', cwd: work, ...opts });

  const help = cli(['help']);
  assert.equal(help.status, 0, `help exited ${help.status}: ${help.stderr}`);
  assert.match(help.stdout, /content-addressed columnar store/, 'help must be the real usage text');

  // A real store, so the install is exercised as a store tool rather than as a binary that runs.
  const jsonl = path.join(work, 'traces.jsonl');
  fs.writeFileSync(
    jsonl,
    [0, 1, 2]
      .map((i) => JSON.stringify({ body: JSON.stringify([{ role: 'user', content: `t${i}` }]), model: `m${i % 2}` }))
      .join('\n') + '\n',
  );
  const store = path.join(work, 'store');
  const imported = cli(['import', store, jsonl]);
  assert.equal(imported.status, 0, `import exited ${imported.status}: ${imported.stderr}`);

  const ids = cli(['ids', store]);
  assert.equal(ids.status, 0, `ids exited ${ids.status}: ${ids.stderr}`);
  assert.equal(ids.stdout.trim().split('\n').length, 3, 'every imported record must be listed');

  const verified = cli(['verify', store, '--deep']);
  assert.equal(verified.status, 0, `verify exited ${verified.status}: ${verified.stderr}`);
  assert.match(verified.stdout, /reconstruct byte-exact/, 'deep verify must report reconstruction');

  // The single-file forms the packages now read, produced by the same binary.
  const container = path.join(work, 'store.turndb');
  assert.equal(cli(['checkpoint', store, container]).status, 0, 'checkpoint must succeed');
  const inspected = cli(['inspect', container]);
  assert.equal(inspected.status, 0, `inspect exited ${inspected.status}: ${inspected.stderr}`);
  assert.match(inspected.stdout, /^container:/m, 'a container must be reported as one');

  // A refusal must be a refusal: nonzero status and a message on stderr, not a stack trace.
  const missing = cli(['inspect', path.join(work, 'nope')]);
  assert.notEqual(missing.status, 0, 'a missing store must exit nonzero');
  assert.match(missing.stderr, /^turndb: /m, 'errors must carry the CLI prefix');

  console.log('cli install: help, import, ids, verify --deep, checkpoint, inspect, refusal');
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
