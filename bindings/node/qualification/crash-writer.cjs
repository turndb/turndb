'use strict';

const { NativeStore } = require('..');
const { putRecord } = require('./record-adapter.cjs');

async function main() {
  const dir = process.argv[2];
  if (!dir) throw new Error('usage: crash-writer.cjs <store-dir>');
  const store = await NativeStore.open(dir);
  await store.write([
    putRecord({
      id: 'crash/0001/first',
      fields: [{ name: 'batch.key', type: 'string', value: 'atomic-1' }],
      contents: [{ name: 'payload', bytes: Buffer.from('first durable value') }],
    }),
    putRecord({
      id: 'crash/0002/second',
      fields: [{ name: 'batch.key', type: 'string', value: 'atomic-1' }],
      contents: [{ name: 'payload', bytes: Buffer.from('second durable value') }],
    }),
  ], true);
  // Intentionally bypass NativeStore.close(): the parent process reopens and exercises WAL replay.
  process.exit(0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
