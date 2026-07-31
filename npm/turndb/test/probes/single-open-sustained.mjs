// Lea's discriminator: ONE open(), then sustained filesystem work through that single store.
// If the defect is per-instance-construction, this survives indefinitely.
// If it is cumulative external allocation, this dies without ever opening twice.
// Result on Node 22.23.2: 50,000 rounds completed, ext flat ~20MB. Construction is the
// only accumulator; work through a live instance contributes nothing.
import { open } from '../../index.mjs';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const ROUNDS = Number(process.argv[2] ?? 50000);
const dir = mkdtempSync(join(tmpdir(), 'sustained-'));
process.stdout.write('open 1...');
const store = await open(dir);
console.log(' opened. driving sustained work through the single handle.');

const body = 'x'.repeat(1024);
for (let i = 1; i <= ROUNDS; i++) {
  store.putBody(`rec-${String(i).padStart(8, '0')}`, body, { round: i, kind: 'probe' });
  if (i % 100 === 0) store.sync();
  if (i % 500 === 0) store.flush();
  if (i % 1000 === 0) {
    const ids = store.scanIds({ limit: 50 });
    const rec = store.getRecord(ids[0]);
    if (!rec) throw new Error('lost a record');
    process.stdout.write(
      `round ${i}: ok (${ids.length} ids paged, mem rss=${Math.round(process.memoryUsage().rss / 1048576)}MB ext=${Math.round(process.memoryUsage().external / 1048576)}MB)\n`,
    );
  }
}
store.sync();
store.close();
console.log(`SINGLE-OPEN SUSTAINED: ${ROUNDS} rounds completed, never opened twice`);
