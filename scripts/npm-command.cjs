'use strict';

const fs = require('node:fs');
const path = require('node:path');

// On Windows npm is exposed as a .cmd shim. Node >= 18.20.2 refuses to spawn batch files without
// a shell; enabling a shell would make package paths subject to cmd.exe quoting. Run npm's own JS
// entry point with this Node process instead, retaining the argument-array boundary. Fail loudly
// if a future Node distribution moves it rather than falling back to a shell.
function npmCommand(args) {
  if (process.platform !== 'win32') return { file: 'npm', args };
  const cli = path.join(path.dirname(process.execPath), 'node_modules', 'npm', 'bin', 'npm-cli.js');
  if (!fs.statSync(cli, { throwIfNoEntry: false })?.isFile()) {
    throw new Error(`npm CLI entry point is absent beside Node: ${cli}`);
  }
  return { file: process.execPath, args: [cli, ...args] };
}

module.exports = { npmCommand };
