// Meter variant: print V8's accounted external memory BEFORE each open.
// Prediction under the threshold hypothesis: ~20MB climb per fresh instance,
// death on the open that would cross V8's 64MB external-allocation soft limit.
// (Measured reality: ~10MB per construction, not 20 — see the objective thread.)
import { open } from '../../index.mjs';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const N = Number(process.argv[2] ?? 8);
for (let i = 1; i <= N; i++) {
  const dir = mkdtempSync(join(tmpdir(), `probe-${i}-`));
  const ext = Math.round(process.memoryUsage().external / 1048576);
  process.stdout.write(`before open ${i}: ext=${ext}MB ...`);
  const store = await open(dir);
  process.stdout.write(` opened.`);
  store.close();
  process.stdout.write(` closed ${i}\n`);
}
console.log(`ALL ${N} COMPLETED`);
