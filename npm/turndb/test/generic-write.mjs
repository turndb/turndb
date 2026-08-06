// Production-trace-shaped generic ingest through the portable binding. Completeness is the contract:
// every named content, duplicate attribute occurrence, and colliding-timestamp id must survive.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { WASI } from 'node:wasi';
import './_artifact.mjs';
import { open } from '../index.mjs';

async function tempStore(prefix = 'turndb-generic-write-') {
  const dir = await mkdtemp(join(tmpdir(), prefix));
  return { dir, store: await open(dir) };
}

test('durable write calls the WASI sync boundary before acknowledging', async () => {
  // This observes the OPERATION rather than trusting `{ durable: true }` to describe itself. It is
  // also the named test for the skip-sync mutation: remove Store::sync from tdb_write and the
  // durable call makes no fd_sync while still claiming an acknowledgement.
  const original = WASI.prototype.getImportObject;
  let syncCalls = 0;
  WASI.prototype.getImportObject = function (...args) {
    const imports = original.apply(this, args);
    const wasi = imports.wasi_snapshot_preview1;
    const fdSync = wasi.fd_sync;
    wasi.fd_sync = (...syncArgs) => {
      syncCalls++;
      return fdSync(...syncArgs);
    };
    return imports;
  };

  let fixture;
  try {
    fixture = await tempStore('turndb-durable-ack-');
    const before = syncCalls;
    assert.deepEqual(
      fixture.store.write(
        [{ kind: 'put', id: 'not-yet-durable', contents: [{ name: 'request', bytes: 'one' }] }],
        { durable: false },
      ),
      { applied: 1, durable: false },
    );
    assert.equal(syncCalls, before, 'a non-durable result must not imply a sync');

    const ack = fixture.store.write(
      [{ kind: 'put', id: 'durable', contents: [{ name: 'response', bytes: 'two' }] }],
      { durable: true },
    );
    assert.deepEqual(ack, { applied: 1, durable: true });
    assert.ok(syncCalls > before, 'durable acknowledgement must follow fd_sync');
  } finally {
    WASI.prototype.getImportObject = original;
    if (fixture) {
      fixture.store.close();
      await rm(fixture.dir, { recursive: true, force: true });
    }
  }
});

test('a durable production-trace-shaped batch reopens byte-exact and pages eight timestamp peers once', async () => {
  const { dir, store } = await tempStore();
  const timestamp = '1785910000000';
  const ids = Array.from(
    { length: 8 },
    (_, i) => `member/alice/${timestamp}/${String(i).padStart(2, '0')}`,
  );
  const expected = new Map();
  try {
    store.putBody('obsolete', 'must be deleted');
    const operations = ids.map((id, i) => {
      const request = Uint8Array.from([0, i, 255, 128]);
      const response = Uint8Array.from([255, 128, i, 0]);
      const contents = [
        { name: 'request', bytes: request },
        { name: 'response', bytes: response },
      ];
      // Content names are an open set. This kind did not exist when the trace layer was designed.
      if (i === 0) contents.push({ name: 'future/tool-transcript', bytes: Uint8Array.of(7, 8, 9) });
      expected.set(id, { request, response });
      return {
        kind: 'put',
        id,
        contents,
        attrs: [
          ['kind', i === 0 ? 'session_end' : 'llm_exchange'],
          ['tag', 'first'],
          ['tag', 'second'],
          ['timestamp_ns', { timestampNs: 1785910000000000000n }],
        ],
      };
    });
    operations.push({ kind: 'delete', id: 'obsolete' });

    assert.deepEqual(store.write(operations, { durable: true }), {
      applied: 9,
      durable: true,
    });
    store.close();

    const reopened = await open(dir);
    try {
      assert.equal(reopened.get('obsolete'), null);
      const rows = [];
      let cursor;
      do {
        const page = reopened.scan({
          prefix: `member/alice/${timestamp}/`,
          cursor,
          limit: 3, // boundaries after 3 and 6, both INSIDE the eight-id collision group
          attrs: ['kind', 'tag', 'timestamp_ns'],
          contents: [
            { name: 'request', mode: 'bytes' },
            { name: 'response', mode: 'bytes' },
            { name: 'future/tool-transcript', mode: 'bytes' },
          ],
        });
        rows.push(...page.rows);
        cursor = page.next;
      } while (cursor !== undefined);

      assert.deepEqual(rows.map((row) => row.id), ids, 'complete traversal, stable and exactly once');
      assert.equal(new Set(rows.map((row) => row.id)).size, 8);
      for (const row of rows) {
        assert.deepEqual(row.attrs.map(([name]) => name), ['kind', 'tag', 'tag', 'timestamp_ns']);
        assert.deepEqual(row.attrs.slice(1, 3), [
          ['tag', 'first'],
          ['tag', 'second'],
        ]);
        const byName = Object.fromEntries(row.contents.map((content) => [content.name, content]));
        assert.deepEqual(byName.request.bytes, Buffer.from(expected.get(row.id).request));
        assert.deepEqual(byName.response.bytes, Buffer.from(expected.get(row.id).response));
        assert.match(byName.request.identity, /^[0-9a-f]{64}$/);
        assert.match(byName.response.identity, /^[0-9a-f]{64}$/);
        assert.equal(byName['future/tool-transcript'].present, row.id === ids[0]);
        if (row.id === ids[0]) {
          assert.deepEqual(byName['future/tool-transcript'].bytes, Buffer.from([7, 8, 9]));
        }
      }
    } finally {
      reopened.close();
    }
  } finally {
    try {
      store.close();
    } catch {}
    await rm(dir, { recursive: true, force: true });
  }
});

test('a malformed later generic record applies none of the earlier valid batch', async () => {
  const { dir, store } = await tempStore('turndb-generic-atomic-');
  try {
    assert.throws(
      () =>
        store.write(
          [
            { kind: 'put', id: 'would-have-landed', contents: [{ name: 'request', bytes: 'valid' }] },
            {
              kind: 'put',
              id: 'duplicate-content',
              contents: [
                { name: 'request', bytes: 'one' },
                { name: 'request', bytes: 'two' },
              ],
            },
          ],
          { durable: true },
        ),
      /duplicate content name.*request/,
    );
    assert.deepEqual(
      store.scanIds({ prefix: 'would-have-landed' }),
      [],
      'a refused batch applies no valid prefix',
    );
    assert.deepEqual(store.scanIds({ prefix: 'duplicate-content' }), []);

    // Refusal must not widen onto the nearest valid input: two distinct, unknown names are valid.
    assert.deepEqual(
      store.write(
        [
          {
            kind: 'put',
            id: 'nearest-valid',
            contents: [
              { name: 'request', bytes: 'one' },
              { name: 'request.extra', bytes: 'two' },
            ],
          },
        ],
        { durable: true },
      ),
      { applied: 1, durable: true },
    );
  } finally {
    store.close();
    await rm(dir, { recursive: true, force: true });
  }
});
