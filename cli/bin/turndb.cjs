#!/usr/bin/env node
'use strict';

// Launcher for the turndb binary, which ships in a per-platform package that npm installs as an
// optional dependency and skips everywhere it does not apply.
//
// The binary is native and Unix-only by design — it needs positioned reads, flock, and (for
// `punch`) Linux hole punching — so there is deliberately no WASM fallback here. A platform this
// package has no build for must say so plainly rather than silently degrade into a different
// engine with different guarantees.

const { spawnSync } = require('node:child_process');

/// Platform packages are named for the target triple, matching the native addon's convention.
function platformPackage() {
  const { platform, arch } = process;
  if (platform === 'linux') {
    // The musl/glibc split is a real ABI boundary for a dynamically linked binary; report the one
    // that was looked for so a miss names the package to publish rather than "not supported".
    const libc = isMusl() ? 'musl' : 'gnu';
    if (arch === 'x64') return `@turndb/cli-linux-x64-${libc}`;
    if (arch === 'arm64') return `@turndb/cli-linux-arm64-${libc}`;
  }
  if (platform === 'darwin') {
    if (arch === 'x64') return '@turndb/cli-darwin-x64';
    if (arch === 'arm64') return '@turndb/cli-darwin-arm64';
  }
  return null;
}

function isMusl() {
  const report = typeof process.report?.getReport === 'function' ? process.report.getReport() : null;
  if (report && report.header && typeof report.header.glibcVersionRuntime === 'string') return false;
  if (report && Array.isArray(report.sharedObjects)) {
    return report.sharedObjects.some((o) => o.includes('musl'));
  }
  // No report to consult: assume glibc, which is the overwhelmingly common case, and let a
  // resolution failure name the package rather than guessing musl and failing more confusingly.
  return false;
}

function resolveBinary() {
  const pkg = platformPackage();
  if (pkg === null) {
    throw new Error(
      `turndb has no CLI build for ${process.platform}-${process.arch}. The binary is Unix-only `
        + 'by design (positioned reads, flock, hole punching); on Windows use WSL, or build from '
        + 'source with `cargo install turndb`.',
    );
  }
  try {
    return require.resolve(`${pkg}/turndb`);
  } catch (cause) {
    const error = new Error(
      `turndb's platform package ${pkg} is not installed. npm skips optional dependencies on `
        + 'install failure, so this usually means the install was run with --no-optional, behind a '
        + `registry that lacks ${pkg}, or on a platform it was not published for. Reinstall, or `
        + 'build from source with `cargo install turndb`.',
    );
    error.cause = cause;
    throw error;
  }
}

let binary;
try {
  binary = resolveBinary();
} catch (error) {
  console.error(`turndb: ${error.message}`);
  process.exit(1);
}

// stdio is inherited so the child owns the terminal: `turndb get` writes record bytes straight to
// stdout, and piping into `head` must reach the real process rather than a Node buffer.
const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  console.error(`turndb: could not run ${binary}: ${result.error.message}`);
  process.exit(1);
}
// A child killed by a signal has no exit code; report it the way a shell would rather than as 0.
process.exit(result.status === null ? 128 + (result.signal ? 1 : 0) : result.status);
