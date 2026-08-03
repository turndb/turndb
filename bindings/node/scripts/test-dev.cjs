'use strict';

const child = require('node:child_process');
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

child.execFileSync(process.execPath, ['--test', 'test/native.test.cjs'], {
  cwd: path.resolve(__dirname, '..'),
  env: { ...process.env, TURNDB_NATIVE_PATH: library },
  stdio: 'inherit',
});
