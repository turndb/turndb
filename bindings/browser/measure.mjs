import { execFileSync } from 'node:child_process';
import { mkdtemp, open, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createRequire } from 'node:module';
import { createCipheriv } from 'node:crypto';

import { NativeStore } from '../node/index.mjs';
import { BrowserDatabase, HttpRangeReadAt } from './index.mjs';
import { reproducibleCargoEnv } from './cargo-env.mjs';

const root = resolve(import.meta.dirname, '../..');
const cargoEnv = reproducibleCargoEnv(root);
const scratch = await mkdtemp(join(tmpdir(), 'turndb-browser-measure-'));
const existingStore = process.env.TURNDB_MEASURE_EXISTING;
const storePath = existingStore ? resolve(existingStore) : join(scratch, 'large.turndb');
const generated = join(scratch, 'wasm');

function payload(seed, length) {
  // A one-element JSON array exercises the trace store's structural carve, while base64url of an
  // AES-CTR stream remains deterministic and realistically incompressible. Structural boundaries
  // keep the fold directory representative of large trace payloads instead of a worst-case binary
  // CDC corpus whose open metadata would itself dominate the point query.
  if (!Number.isSafeInteger(length) || length < 8 || (length - 4) % 4 !== 0) {
    throw new RangeError('measurement record bytes must be at least 8 and four bytes past a multiple of four');
  }
  const key = Buffer.alloc(32);
  const iv = Buffer.alloc(16);
  key.writeUInt32BE(seed >>> 0, 28);
  iv.writeUInt32BE(seed >>> 0, 12);
  const rawLength = ((length - 4) * 3) / 4;
  const encoded = createCipheriv('aes-256-ctr', key, iv)
    .update(Buffer.alloc(rawLength))
    .toString('base64url');
  return Buffer.from(`["${encoded}"]`);
}

try {
  const records = Number(process.env.TURNDB_MEASURE_RECORDS ?? 56);
  const bytesPerRecord = Number(process.env.TURNDB_MEASURE_RECORD_BYTES ?? (52 << 20));
  if (!Number.isSafeInteger(records) || records < 1) throw new RangeError('measurement records must be a positive safe integer');
  if (!existingStore) {
    const store = await NativeStore.openFile(storePath, {
      compressionLevel: 1,
      blockTargetBytes: 16n << 20n,
    });
    try {
      for (let index = 0; index < records; index++) {
        await store.write([{
          kind: 'put',
          id: `trace/${String(index).padStart(4, '0')}`,
          attrs: [{ name: 'index', kind: 'int', intValue: BigInt(index) }],
          contents: [{ name: 'body', bytes: payload(index + 1, bytesPerRecord) }],
        }]);
      }
      await store.sync();
      await store.flush();
    } finally {
      await store.close(true);
    }
  }

  execFileSync('cargo', [
    'build', '-p', 'turndb-browser', '--target', 'wasm32-unknown-unknown',
    '--profile', 'wasm-release',
  ], { cwd: root, env: cargoEnv, stdio: 'inherit' });
  execFileSync('wasm-bindgen', [
    join(root, 'target/wasm32-unknown-unknown/wasm-release/turndb_browser.wasm'),
    '--target', 'nodejs', '--out-dir', generated,
  ], { cwd: root, stdio: 'inherit' });
  const wasm = createRequire(import.meta.url)(join(generated, 'turndb_browser.js'));
  const storeBytes = (await stat(storePath)).size;
  const file = await open(storePath, 'r');
  let fetchStats;
  let blockBytes;
  try {
    const fetch = async (_url, options) => {
      const match = /^bytes=(\d+)-(\d+)$/.exec(options.headers.Range);
      const start = Number(match[1]);
      const end = Number(match[2]);
      const bytes = Buffer.allocUnsafe(end - start + 1);
      const { bytesRead } = await file.read(bytes, 0, bytes.length, start);
      return new Response(bytes.subarray(0, bytesRead), {
        status: 206,
        headers: { 'Content-Range': `bytes ${start}-${end}/${storeBytes}` },
      });
    };
    const source = await HttpRangeReadAt.open('https://fixture.invalid/large.turndb', { fetch });
    blockBytes = source.blockSize;
    const database = await BrowserDatabase.open(wasm, source);
    const selectedId = `trace/${String(records - 1).padStart(4, '0')}`;
    const page = await database.scan({
      contractVersion: 1,
      limit: 1,
      predicates: [{ kind: 'id', op: 'eq', value: selectedId }],
      attrs: ['index'],
      contents: [{ name: 'body', mode: 'metadata' }],
    });
    if (page.rows.length !== 1 || page.rows[0].id !== selectedId) {
      throw new Error(`point query returned ${JSON.stringify(page.rows)}`);
    }
    fetchStats = database.fetchStats();
    database.close();
  } finally {
    await file.close();
  }
  process.stdout.write(`${JSON.stringify({
    contractVersion: 1,
    workload: {
      records,
      bytesPerRecord,
      logicalContentBytes: records * bytesPerRecord,
      selectedId: `trace/${String(records - 1).padStart(4, '0')}`,
    },
    storeBytes,
    blockBytes,
    openAndPointQuery: fetchStats,
    fetchedFraction: fetchStats.networkBytes / storeBytes,
  }, null, 2)}\n`);
} finally {
  await rm(scratch, { recursive: true, force: true });
}
