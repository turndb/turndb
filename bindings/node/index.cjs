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

let native;
for (const candidate of candidates) {
  if (fs.existsSync(candidate)) {
    native = load(candidate);
    break;
  }
}

if (!native) {
  throw new Error(
    `No TurnDB native addon was found for ${process.platform}-${process.arch}. ` +
      `Looked for: ${candidates.join(', ')}. ` +
      'This package does not silently fall back to the capability-reduced WASM build.'
  );
}

class TurnDbError extends Error {
  constructor(code, message, cause) {
    super(message, { cause });
    this.name = 'TurnDbError';
    this.code = code;
  }
}

function normalizeError(error) {
  if (error instanceof TurnDbError) return error;
  const reason = error && typeof error.message === 'string' ? error.message : String(error);
  const marker = /\[TURNDB_CODE:([A-Z_]+)\]/.exec(reason);
  const code = marker
    ? marker[1]
    : error && error.code === 'InvalidArg'
      ? 'INVALID_ARGUMENT'
      : 'INTERNAL';
  return new TurnDbError(code, reason.replace(/\[TURNDB_CODE:[A-Z_]+\]\s*/g, ''), error);
}

function guarded(fn, operation) {
  return function guardedNativeCall(...args) {
    if (['scan', 'explainScan'].includes(operation) && args[0]?.signal?.aborted) {
      return Promise.reject(new TurnDbError(
        'CANCELLED',
        `${operation} was cancelled before submission`,
      ));
    }
    if (operation === 'next' && args[0]?.signal?.aborted) {
      // Cancellation is terminal for a pull-based SQL stream. Close before rejecting so an already
      // aborted signal has the same ownership semantics as Rust winning the in-flight select.
      return Promise.resolve(this.close()).then(() => {
        throw new TurnDbError('CANCELLED', 'next was cancelled before submission');
      });
    }
    let lifecycleOptions;
    if ([
      'compact', 'compactBounded', 'estimateCompactionSpace', 'erase', 'backup',
      'recoverManifest',
    ].includes(operation)) {
      lifecycleOptions = args[1];
    } else if (['restoreBackup', 'querySql'].includes(operation)) {
      lifecycleOptions = args[2];
    } else if ([
      'sync', 'flush', 'verify', 'punch', 'refold', 'estimateRefoldSpace', 'spaceUsage',
      'formatMigrationStatus', 'estimateFormatMigrationSpace', 'migrateFormatStep',
      'partDistribution', 'contentLiveness',
    ].includes(operation)) {
      lifecycleOptions = args[0];
    }
    if (lifecycleOptions?.signal?.aborted) {
      return Promise.reject(new TurnDbError(
        'CANCELLED',
        `${operation} was cancelled before submission`,
      ));
    }
    try {
      return Promise.resolve(fn.apply(this, args)).catch((error) => {
        throw normalizeError(error);
      });
    } catch (error) {
      throw normalizeError(error);
    }
  };
}

for (const Class of [native.NativeStore, native.NativeSnapshot, native.NativeSqlQuery].filter(Boolean)) {
  for (const name of [
    'write', 'sync', 'flush', 'scan', 'explainScan', 'readContent', 'snapshot',
    'querySql', 'next', 'stats', 'compact', 'compactBounded', 'estimateCompactionSpace',
    'verify', 'erase', 'punch', 'refold', 'estimateRefoldSpace',
    'formatMigrationStatus', 'estimateFormatMigrationSpace', 'migrateFormatStep',
    'backup', 'health', 'metrics', 'lifecycleEvents', 'partDistribution', 'contentLiveness', 'spaceUsage',
    'schema', 'close',
  ]) {
    if (typeof Class.prototype[name] === 'function') {
      Class.prototype[name] = guarded(Class.prototype[name], name);
    }
  }
}

function guardFactories(Class) {
  function NativeFacade() {
    throw new TypeError('TurnDB handles are created with their static open methods');
  }
  // Instances created by the native factories still satisfy `instanceof` against the public
  // facade because both use the same native prototype.
  NativeFacade.prototype = Class.prototype;
  for (const name of ['open', 'openAt']) {
    if (typeof Class[name] === 'function') NativeFacade[name] = guarded(Class[name].bind(Class));
  }
  return NativeFacade;
}

module.exports = {
  ...native,
  NativeStore: guardFactories(native.NativeStore),
  NativeSnapshot: guardFactories(native.NativeSnapshot),
  ...(native.NativeSqlQuery && { NativeSqlQuery: guardFactories(native.NativeSqlQuery) }),
  retainedCommits: guarded(native.retainedCommits),
  restoreBackup: guarded(native.restoreBackup, 'restoreBackup'),
  recoverManifest: guarded(native.recoverManifest, 'recoverManifest'),
  TurnDbError,
};
