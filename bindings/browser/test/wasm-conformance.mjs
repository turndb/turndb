import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

import { BlobReadAt, BrowserDatabase, BufferReadAt, TurnDbError, verifySource } from '../index.mjs';
import { reproducibleCargoEnv } from '../cargo-env.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../../..');
const cargoEnv = reproducibleCargoEnv(root);
const scratch = await mkdtemp(join(tmpdir(), 'turndb-browser-conformance-'));

function projected(expected, request) {
  const attrs = new Set(request.attrs ?? []);
  return {
    id: expected.id,
    attrs: expected.attrs.filter((value) => attrs.has(value.name)),
    contents: (request.contents ?? []).map((selection) => {
      const content = expected.contents.find((value) => value.name === selection.name);
      if (!content) return { name: selection.name, present: false };
      return {
        name: selection.name,
        present: true,
        ...(selection.mode === 'bytes' ? { base64: content.base64 } : {}),
      };
    }),
  };
}

try {
  execFileSync('cargo', [
    'build', '-p', 'turndb-browser', '--target', 'wasm32-unknown-unknown',
    '--profile', 'wasm-release',
  ], { cwd: root, env: cargoEnv, stdio: 'inherit' });
  execFileSync('wasm-bindgen', [
    join(root, 'target/wasm32-unknown-unknown/wasm-release/turndb_browser.wasm'),
    '--target', 'nodejs', '--out-dir', scratch,
  ], { cwd: root, stdio: 'inherit' });
  const wasm = createRequire(import.meta.url)(join(scratch, 'turndb_browser.js'));
  const corpus = JSON.parse(await readFile(join(root, 'conformance/v1/corpus.json'), 'utf8'));
  const hex = await readFile(join(root, 'conformance/v1/fixture.turndb.hex'), 'utf8');
  const bytes = Buffer.from(hex.replaceAll(/\s/g, ''), 'hex');
  const database = await BrowserDatabase.open(wasm, new BufferReadAt(bytes, 'fixture.turndb'));
  const capabilities = database.capabilities();
  assert.equal(capabilities.contractVersion, 2);
  assert.equal(capabilities.profile, 'browser');
  assert.equal(capabilities.writerExclusion, 'read_only');
  assert(capabilities.operations.includes('scan'));
  assert(!capabilities.operations.includes('write'));

  const view = corpus.views.find((candidate) => candidate.source === 'snapshot-v2');
  const records = new Map(view.records.map((record) => [record.id, record]));
  for (const query of corpus.queries.filter((candidate) => candidate.source === 'snapshot-v2')) {
    const rows = [];
    const pages = [];
    let cursor;
    do {
      const request = { ...query.request, ...(cursor ? { cursor } : {}) };
      const page = await database.scan(request);
      pages.push(page);
      rows.push(...page.rows);
      cursor = query.paginate ? page.next : undefined;
    } while (cursor);
    assert.deepEqual(rows.map((row) => row.id), query.expectedIds, query.name);
    for (const row of rows) {
      const expected = projected(records.get(row.id), query.request);
      assert.equal(row.id, expected.id);
      assert.deepEqual(row.attrs, expected.attrs, `${query.name}: ${row.id} attrs`);
      assert.equal(row.contents.length, expected.contents.length);
      for (let index = 0; index < row.contents.length; index++) {
        assert.equal(row.contents[index].name, expected.contents[index].name);
        assert.equal(row.contents[index].present, expected.contents[index].present);
        if ('base64' in expected.contents[index]) {
          assert.equal(row.contents[index].base64, expected.contents[index].base64);
        } else {
          assert(!('base64' in row.contents[index]));
        }
      }
    }
    if (query.assertMetadataOnlyIo) {
      assert(pages.every((page) => page.stats.io.foldBlocksTouched === '0'));
      assert(pages.every((page) => page.stats.io.foldStoredBytesRead === '0'));
      assert(pages.every((page) => page.stats.reconstructedBytes === '0'));
    }
    if (query.name === 'content-budget-refuses-to-truncate') {
      assert(pages.every((page) => page.rows.length === 1));
    }
  }
  assert.deepEqual(
    (await database.scan({ contractVersion: 1, limit: 100 })).rows.map((row) => row.id),
    view.records.map((record) => record.id),
  );
  const cursorPage = await database.scan({
    contractVersion: 1,
    direction: 'reverse',
    limit: 1,
  });
  const damaged = `${cursorPage.next.slice(0, -1)}${cursorPage.next.endsWith('A') ? 'B' : 'A'}`;
  await assert.rejects(
    database.scan({ contractVersion: 1, direction: 'reverse', limit: 1, cursor: damaged }),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );
  database.close();
  database.close();
  assert(capabilities.operations.includes('verify'));

  // Verification in declared units: the same result over a whole buffer and over a Blob whose
  // cache holds two 4 KiB blocks, where a single-pass verification would restart on every miss.
  const whole = await verifySource(wasm, new BufferReadAt(bytes, 'fixture.turndb'));
  assert.equal(whole.report.state, 'valid');
  assert.equal(whole.report.records, view.records.length, 'every visible record is reconstructed');
  assert(whole.report.contentIdentities > 0 && Number(whole.report.contentBytes) > 0);
  assert(whole.units > 8, `a store this shape plans many units, got ${whole.units}`);
  const progress = [];
  const small = new BlobReadAt(new Blob([bytes]), { blockSize: 4096, maxBlocks: 2, label: 'fixture.turndb' });
  let fetches = 0;
  const fetchRange = small.fetchRange.bind(small);
  small.fetchRange = (start, end, signal) => {
    fetches++;
    return fetchRange(start, end, signal);
  };
  const ranged = await verifySource(wasm, small, { onProgress: (step) => progress.push(step) });
  assert.deepEqual(ranged.report, whole.report, 'a range source composes the same evidence');
  assert.equal(progress.length, ranged.units);
  assert(progress.every((step, index) => step.done === index + 1 && step.total === ranged.units));
  const kinds = new Set(progress.map((step) => step.unit.kind));
  for (const kind of ['memberChecksum', 'manifestChain', 'partDigest', 'partSection', 'partGrammar', 'partPieces', 'foldFrames', 'content']) {
    assert(kinds.has(kind), `the plan exercises ${kind}`);
  }
  // Footprints are fetched whole ahead of each unit, so requests scale with units, not with
  // misses: a bound of a few requests per unit plus the open's block reads holds, where a
  // miss-driven walk over a two-block cache would issue one request per 4 KiB touched.
  const requestBound = ranged.units * 4 + Math.ceil(bytes.length / 4096);
  assert(fetches > 0 && fetches <= requestBound, `verification issued ${fetches} range fetches over ${ranged.units} units; bound ${requestBound}`);
  assert.equal(small.transient.length, 0, 'footprints are released after their unit');
  // One flipped byte inside a member's checked bytes — located through the plan itself, so the
  // damage cannot land in alignment padding — is refused with the engine's stable class.
  const probeSource = new BufferReadAt(bytes, 'probe.turndb');
  const probe = wasm.BrowserVerifier.open(probeSource.readSync.bind(probeSource), probeSource.length, 'probe.turndb');
  const plan = probe.units();
  const first = plan.findIndex((unit) => unit.kind === 'memberChecksum' && BigInt(unit.stop) > BigInt(unit.start) + 8n);
  assert(first >= 0, 'the plan checks a member with bytes');
  const [range] = probe.footprint(first);
  probe.close();
  const flipped = Uint8Array.from(bytes);
  flipped[Number(range.offset) + 5] ^= 0xff;
  await assert.rejects(
    verifySource(wasm, new BufferReadAt(flipped, 'damaged.turndb')),
    (error) => error instanceof TurnDbError && error.code === 'CORRUPTION',
  );
  console.log(`browser wasm: verification composed over ${ranged.units} units with ${fetches} range fetches`);
  console.log('browser wasm: shared snapshot-v2 corpus passed');
} finally {
  await rm(scratch, { recursive: true, force: true });
}
