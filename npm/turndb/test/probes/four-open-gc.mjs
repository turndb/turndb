// Variant: identical loop, but force a full GC at a safe point after each close.
// Discriminates "GC from inside the WASI fast callback crashes" from "GC over wasm frames
// crashes regardless of initiation point". Run with --expose-gc.
import { open } from '../../index.mjs';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const N = Number(process.argv[2] ?? 20);
for (let i = 1; i <= N; i++) {
  const dir = mkdtempSync(join(tmpdir(), `probe-${i}-`));
  process.stdout.write(`open ${i}...`);
  const store = await open(dir);
  process.stdout.write(` opened.`);
  store.close();
  globalThis.gc();
  process.stdout.write(` closed+gc ${i}\n`);
}
console.log(`ALL ${N} OPEN/CLOSE+GC CYCLES COMPLETED`);
