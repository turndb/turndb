// The deterministic close path is the contract; this exercises its GC fallback.
// Run with `node --expose-gc test/probes/abandoned-handle-gc.mjs`.
import { open } from '../../index.mjs';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

if (globalThis.gc == null) throw new Error('run with --expose-gc');
const dir = mkdtempSync(join(tmpdir(), 'abandoned-handle-'));
let store = await open(dir);
store = null;
for (let i = 0; i < 20; i++) {
  globalThis.gc();
  await new Promise(setImmediate);
}
const reopened = await open(dir);
reopened.close();
console.log('ABANDONED HANDLE COLLECTED; REOPEN COMPLETED');
