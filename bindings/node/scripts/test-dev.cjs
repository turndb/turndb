'use strict';

const child = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const workspace = path.resolve(__dirname, '../../..');
child.execFileSync('cargo', ['build', '-p', 'turndb-node'], {
  cwd: workspace,
  stdio: 'inherit',
});

const library = path.join(
  workspace,
  'target',
  'debug',
  process.platform === 'darwin'
    ? 'libturndb_node.dylib'
    : process.platform === 'win32'
      ? 'turndb_node.dll'
      : 'libturndb_node.so'
);

const testDir = path.resolve(__dirname, '../test');
const tests = fs.readdirSync(testDir)
  .filter((name) => name.endsWith('.test.cjs'))
  .sort()
  .map((name) => path.join('test', name));

child.execFileSync(process.execPath, ['--test', ...tests], {
  cwd: path.resolve(__dirname, '..'),
  env: { ...process.env, TURNDB_NATIVE_PATH: library },
  stdio: 'inherit',
});
