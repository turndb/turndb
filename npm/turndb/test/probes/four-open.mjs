// Independent reducer, written without reference to Seamus's script.
// Sequential open/close cycles, each against a FRESH directory, one ordinary Node process.
// Every step prints BEFORE the call, so the last line on a crash names the call that died.
import { open } from '../../index.mjs';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const N = Number(process.argv[2] ?? 6);
for (let i = 1; i <= N; i++) {
  const dir = mkdtempSync(join(tmpdir(), `probe-${i}-`));
  process.stdout.write(`open ${i}...`);
  const store = await open(dir);
  process.stdout.write(` opened.`);
  store.close();
  process.stdout.write(` closed ${i}\n`);
}
console.log(`ALL ${N} OPEN/CLOSE CYCLES COMPLETED`);
