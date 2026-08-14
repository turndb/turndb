import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../..');
const generated = mkdtempSync(join(tmpdir(), 'turndb-viewer-'));
try {
  execFileSync('cargo', [
    'build', '-p', 'turndb-browser', '--target', 'wasm32-unknown-unknown', '--release',
  ], { cwd: root, stdio: 'inherit' });
  execFileSync('wasm-bindgen', [
    join(root, 'target/wasm32-unknown-unknown/release/turndb_browser.wasm'),
    '--target', 'web', '--out-dir', generated,
  ], { cwd: root, stdio: 'inherit' });
  const glue = readFileSync(join(generated, 'turndb_browser.js'), 'utf8');
  const wasm = readFileSync(join(generated, 'turndb_browser_bg.wasm')).toString('base64');
  const client = readFileSync(join(here, 'index.mjs'), 'utf8');
  const shell = readFileSync(join(here, 'viewer-shell.html'), 'utf8');
  const html = shell
    .replace('/*__TURNDB_WASM_GLUE__*/', glue)
    .replace('/*__TURNDB_BROWSER_CLIENT__*/', client)
    .replace('/*__TURNDB_WASM_BASE64__*/', wasm);
  writeFileSync(join(here, 'turndb-viewer.html'), html);
  console.log(`turndb-viewer.html: ${Buffer.byteLength(html)} bytes`);
} finally {
  rmSync(generated, { recursive: true, force: true });
}
