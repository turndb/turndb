// The safe-by-default surface: the level default routes to the engine, and bounded compaction
// bounds what it merges. Timing is NOT asserted here — CI runners make timing assertions lie —
// the level tests assert plumbing through deterministic compressed output instead.
import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { mkdtempSync, readdirSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { open, TurndbError } from '../index.mjs';

function dirBytes(dir) {
  let total = 0;
  for (const e of readdirSync(dir, { recursive: true })) {
    const p = join(dir, e.toString());
    const st = statSync(p);
    if (st.isFile()) total += st.size;
  }
  return total;
}

// Identical structured-but-unique content for every store, generated once. Compressible enough
// that zstd levels 3 and 19 provably emit different bytes.
const BODIES = [];
{
  const envelope = JSON.stringify({ role: 'assistant', model: 'engine-test', tools: ['bash', 'edit'] });
  for (let i = 0; i < 400; i++) {
    BODIES.push(`${envelope} record ${i} ${String(i * 2654435761 % 1e9).repeat(40)} tail-${i}`);
  }
}

async function fillStore(opts) {
  const dir = mkdtempSync(join(tmpdir(), 'lvl-'));
  const store = await open(dir, opts);
  for (let i = 0; i < BODIES.length; i++) {
    store.putBody(`rec-${String(i).padStart(4, '0')}`, BODIES[i], { i });
  }
  store.sync();
  store.flush();
  store.close();
  return dir;
}

test('the level default is 3: omitted and explicit 3 produce identical bytes, 19 does not', async () => {
  const defaultDir = await fillStore({});
  const level3Dir = await fillStore({ level: 3 });
  const level19Dir = await fillStore({ level: 19 });
  const d = dirBytes(defaultDir);
  const l3 = dirBytes(level3Dir);
  const l19 = dirBytes(level19Dir);
  // Same input, same level, deterministic codec: byte-identical stores. A default of 19 (or of
  // engine-passthrough 0) fails the first assertion; a level option that isn't plumbed at all
  // fails the second.
  assert.equal(d, l3, `default (${d}B) must match explicit level 3 (${l3}B)`);
  assert.notEqual(l3, l19, `level 3 and 19 must differ on compressible content (both ${l3}B)`);
});

test('level 0 selects the engine default (19), documented escape hatch', async () => {
  const level0Dir = await fillStore({ level: 0 });
  const level19Dir = await fillStore({ level: 19 });
  assert.equal(dirBytes(level0Dir), dirBytes(level19Dir));
});

test('maybeCompact bounds the merge; autoCompact totals it; both keep every record', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'cmp-'));
  const store = await open(dir);
  // Nine flushes -> nine parts.
  for (let p = 0; p < 9; p++) {
    for (let i = 0; i < 20; i++) {
      store.putBody(`p${p}-r${i}`, `part ${p} record ${i} ${'y'.repeat(64)}`, { p, i });
    }
    store.sync();
    store.flush();
  }
  assert.equal(store.stats().parts, 9);

  // Below trigger: refuses to run rather than merging anyway.
  assert.equal(store.maybeCompact({ trigger: 20 }), false);
  assert.equal(store.stats().parts, 9);

  // Default dial: oldest 4 of 9 merge into 1 -> 6 live parts. Bounded, not total.
  assert.equal(store.maybeCompact(), true);
  assert.equal(store.stats().parts, 6);

  // Every record from every original part survives the bounded merge.
  for (let p = 0; p < 9; p++) {
    const got = store.getText(`p${p}-r7`);
    assert.equal(got, `part ${p} record 7 ${'y'.repeat(64)}`);
  }

  // Nonsense budgets refuse loudly instead of silently merging something else.
  assert.throws(() => store.maybeCompact({ run: 1 }), TurndbError);
  assert.throws(() => store.maybeCompact({ trigger: 0 }), TurndbError);

  // Total merge still available and still total.
  assert.equal(store.autoCompact(), false); // 6 parts < engine threshold of 8
  while (store.stats().parts > 1) {
    assert.equal(store.maybeCompact({ trigger: 2, run: store.stats().parts }), true);
  }
  assert.equal(store.stats().parts, 1);
  assert.equal(store.getText('p8-r19'), `part 8 record 19 ${'y'.repeat(64)}`);
  store.close();
});
