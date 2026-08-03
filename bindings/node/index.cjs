'use strict';

const fs = require('node:fs');
const path = require('node:path');

function load(filename) {
  // process.dlopen accepts an arbitrary filename, which lets the development test load Cargo's
  // `.so` directly. Published prebuilds use `.node` and take the ordinary require path.
  if (path.extname(filename) === '.node') return require(filename);
  const module = { exports: {} };
  process.dlopen(module, filename);
  return module.exports;
}

const explicit = process.env.TURNDB_NATIVE_PATH;
const candidates = explicit
  ? [path.resolve(explicit)]
  : [
      path.join(__dirname, `turndb.${process.platform}-${process.arch}.node`),
      path.join(__dirname, 'turndb.node'),
    ];

for (const candidate of candidates) {
  if (fs.existsSync(candidate)) {
    module.exports = load(candidate);
    return;
  }
}

throw new Error(
  `No TurnDB native addon was found for ${process.platform}-${process.arch}. ` +
    `Looked for: ${candidates.join(', ')}. ` +
    'This package does not silently fall back to the capability-reduced WASM build.'
);
