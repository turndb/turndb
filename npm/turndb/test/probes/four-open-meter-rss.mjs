// Meter variant tracking BOTH V8's external counter and actual RSS before each open.
// This is the probe that corrected the "leak on every version" claim: on Node 24,
// RSS stays flat (~55-58MB) across 12 cycles while ext climbs ~10MB/construction,
// and ext self-discharges (86MB -> 49MB) when an ordinary GC collects dead instances.
// No real memory is retained on any version; only the accounting rides up between GCs.
import { open } from '../../index.mjs';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const N = Number(process.argv[2] ?? 8);
for (let i = 1; i <= N; i++) {
  const dir = mkdtempSync(join(tmpdir(), `probe-${i}-`));
  const m = process.memoryUsage();
  const ext = Math.round(m.external / 1048576);
  const rss = Math.round(m.rss / 1048576);
  process.stdout.write(`before open ${i}: ext=${ext}MB rss=${rss}MB ...`);
  const store = await open(dir);
  process.stdout.write(` opened.`);
  store.close();
  process.stdout.write(` closed ${i}\n`);
}
console.log(`ALL ${N} COMPLETED`);
