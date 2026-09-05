'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { capabilities, NativeSnapshot, NativeStore, TurnDbError } = require('..');

const conformanceDir = path.resolve(__dirname, '../../../conformance/v1');
const capabilityConformanceDir = path.resolve(__dirname, '../../../conformance/v2');
const corpus = JSON.parse(fs.readFileSync(path.join(conformanceDir, 'corpus.json'), 'utf8'));
const capabilitySchema = JSON.parse(
  fs.readFileSync(path.join(capabilityConformanceDir, 'capabilities.schema.json'), 'utf8'),
);

function temporaryStore(t, name = 'fixture.turndb') {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-conformance-node-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  return path.join(root, name);
}

function f64Bits(value) {
  const bytes = Buffer.alloc(8);
  bytes.writeDoubleBE(value);
  return bytes.readBigUInt64BE().toString(16).padStart(16, '0');
}

function scalarToNative(name, scalar) {
  switch (scalar.type) {
    case 'string': return { name, kind: 'string', stringValue: scalar.value };
    case 'i64': return { name, kind: 'int', intValue: BigInt(scalar.decimal) };
    case 'f64': return { name, kind: 'float', floatBits: scalar.bitsHex };
    case 'bool': return { name, kind: 'bool', boolValue: scalar.value };
    case 'u64': return { name, kind: 'uint', uintValue: BigInt(scalar.decimal) };
    case 'binary': return { name, kind: 'binary', binaryValue: Buffer.from(scalar.base64, 'base64') };
    case 'timestampNs':
      return { name, kind: 'timestamp_ns', timestampNsValue: BigInt(scalar.decimal) };
    case 'null': return { name, kind: 'null' };
    default: throw new Error(`unknown contract scalar ${scalar.type}`);
  }
}

function nativeAttrToContract(attr) {
  switch (attr.kind) {
    case 'string': return { name: attr.name, value: { type: 'string', value: attr.stringValue } };
    case 'int': return { name: attr.name, value: { type: 'i64', decimal: String(attr.intValue) } };
    case 'float':
      return {
        name: attr.name,
        value: { type: 'f64', bitsHex: attr.floatBits ?? f64Bits(attr.floatValue) },
      };
    case 'bool': return { name: attr.name, value: { type: 'bool', value: attr.boolValue } };
    case 'uint': return { name: attr.name, value: { type: 'u64', decimal: String(attr.uintValue) } };
    case 'binary':
      return { name: attr.name, value: { type: 'binary', base64: attr.binaryValue.toString('base64') } };
    case 'timestamp_ns':
      return {
        name: attr.name,
        value: { type: 'timestampNs', decimal: String(attr.timestampNsValue) },
      };
    case 'null': return { name: attr.name, value: { type: 'null' } };
    default: throw new Error(`unknown native scalar ${attr.kind}`);
  }
}

function recordToWrite(record) {
  return {
    kind: 'put',
    id: record.id,
    attrs: record.attrs.map(({ name, value }) => scalarToNative(name, value)),
    contents: record.contents.map(({ name, base64 }) => ({
      name,
      bytes: Buffer.from(base64, 'base64'),
    })),
  };
}

function requestToNative(request) {
  assert.equal(request.contractVersion, 1);
  const native = { ...request };
  delete native.contractVersion;
  if (native.maxReconstructedBytes !== undefined) {
    native.maxReconstructedBytes = BigInt(native.maxReconstructedBytes);
  }
  native.predicates = (native.predicates ?? []).map((predicate) => {
    switch (predicate.kind) {
      case 'id':
        return { kind: 'id', op: predicate.op, idValue: predicate.value };
      case 'attr':
        return {
          kind: 'attr',
          op: predicate.op,
          value: scalarToNative(predicate.name, predicate.value),
        };
      case 'attrExists':
        return { kind: 'attr_exists', name: predicate.name, present: predicate.present };
      case 'contentExists':
        return { kind: 'content_exists', name: predicate.name, present: predicate.present };
      default: throw new Error(`unknown predicate ${predicate.kind}`);
    }
  });
  return native;
}

function viewFor(source) {
  const views = corpus.views.filter((view) => view.source === source);
  assert.equal(views.length, 1, `one view for ${source}`);
  return views[0];
}

async function assertView(handle, source) {
  const view = viewFor(source);
  const attrs = [...new Set(view.records.flatMap((record) => record.attrs.map((attr) => attr.name)))];
  const contentNames = [
    ...new Set(view.records.flatMap((record) => record.contents.map((content) => content.name))),
  ];
  const page = await handle.scan({
    limit: 100,
    attrs,
    contents: contentNames.map((name) => ({ name, mode: 'bytes' })),
  });
  assert.equal(page.next, undefined);
  assert.deepEqual(page.rows.map((row) => row.id), view.records.map((record) => record.id));

  for (const [row, expected] of page.rows.map((row) => [
    row,
    view.records.find((record) => record.id === row.id),
  ])) {
    assert.deepEqual(row.attrs.map(nativeAttrToContract), expected.attrs, `${source}/${row.id} attrs`);
    for (const projected of row.contents) {
      const content = expected.contents.find((candidate) => candidate.name === projected.name);
      assert.equal(projected.present, content !== undefined);
      if (content === undefined) {
        assert.equal(projected.len, undefined);
        assert.equal(projected.pieces, undefined);
        assert.equal(projected.identity, undefined);
        assert.equal(projected.bytes, undefined);
        assert.equal(await handle.readContent(row.id, projected.name), null);
      } else {
        const bytes = Buffer.from(content.base64, 'base64');
        assert.equal(projected.len, BigInt(bytes.length));
        assert.equal(projected.pieces, content.storage === 'piece' && bytes.length > 0 ? 1 : 0);
        assert.match(projected.identity, /^[0-9a-f]{64}$/);
        assert.deepEqual(projected.bytes, bytes);
        assert.deepEqual(await handle.readContent(row.id, projected.name), bytes);
      }
    }
  }
}

function assertProjectedRow(row, expected, request) {
  const attrs = new Set(request.attrs ?? []);
  assert.deepEqual(
    row.attrs.map(nativeAttrToContract),
    expected.attrs.filter((attr) => attrs.has(attr.name)),
    `${row.id} projected attrs`,
  );
  assert.equal(row.contents.length, (request.contents ?? []).length);
  for (const [projected, selected] of row.contents.map((content, index) => [
    content,
    request.contents[index],
  ])) {
    const expectedContent = expected.contents.find((content) => content.name === selected.name);
    assert.equal(projected.name, selected.name);
    assert.equal(projected.present, expectedContent !== undefined);
    if (expectedContent === undefined) {
      assert.equal(projected.len, undefined);
      assert.equal(projected.pieces, undefined);
      assert.equal(projected.identity, undefined);
      assert.equal(projected.bytes, undefined);
      continue;
    }
    const bytes = Buffer.from(expectedContent.base64, 'base64');
    assert.equal(projected.len, BigInt(bytes.length));
    assert.equal(projected.pieces, expectedContent.storage === 'piece' && bytes.length > 0 ? 1 : 0);
    assert.match(projected.identity, /^[0-9a-f]{64}$/);
    if (selected.mode === 'bytes') assert.deepEqual(projected.bytes, bytes);
    else assert.equal(projected.bytes, undefined);
  }
}

async function assertQuery(handle, source, query) {
  const request = requestToNative(query.request);
  const first = await handle.scan(request);
  if (query.assertCursorDamageRejected) {
    assert(first.next, `${query.name} must return a cursor`);
    const last = first.next.at(-1);
    const damaged = `${first.next.slice(0, -1)}${last === 'A' ? 'B' : 'A'}`;
    await assert.rejects(
      handle.scan({ ...request, cursor: damaged }),
      (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
    );
  }
  if (query.assertCursorMismatchRejected) {
    assert(first.next, `${query.name} must return a cursor`);
    await assert.rejects(
      handle.scan({
        ...request,
        cursor: first.next,
        predicates: [
          ...(request.predicates ?? []),
          { kind: 'id', op: 'ne', idValue: '__changed_after_cursor__' },
        ],
      }),
      (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
    );
  }

  const pages = [first];
  if (query.paginate) {
    for (let guard = 0; pages.at(-1).next && guard < 100; guard += 1) {
      pages.push(await handle.scan({ ...request, cursor: pages.at(-1).next }));
    }
    assert.equal(pages.at(-1).next, undefined, `${query.name} terminates`);
  } else {
    assert.equal(first.next, undefined, `${query.name} fits one page`);
  }

  const rows = pages.flatMap((page) => page.rows);
  assert.deepEqual(rows.map((row) => row.id), query.expectedIds, query.name);
  const view = viewFor(source);
  for (const row of rows) {
    assertProjectedRow(row, view.records.find((record) => record.id === row.id), query.request);
  }
  for (const page of pages) {
    assert.equal(page.stats.returned, page.rows.length);
    if (query.assertMetadataOnlyIo) {
      assert.equal(page.stats.contentValuesReconstructed, 0);
      assert.equal(page.stats.reconstructedBytes, 0n);
      assert.equal(page.stats.io.foldBlocksTouched, 0n);
      assert.equal(page.stats.io.foldBlockCacheHits, 0n);
      assert.equal(page.stats.io.foldBlockCacheMisses, 0n);
      assert.equal(page.stats.io.foldStoredBytesRead, 0n);
      assert.equal(page.stats.io.foldRawBytesDecoded, 0n);
    }
  }
  if (query.name === 'all-scalars-duplicates-and-content-shapes') {
    assert.equal(first.stats.duplicateAttrOccurrences, 1);
  }
  if (query.name === 'content-budget-refuses-to-truncate') {
    assert(pages.every((page) => page.rows.length === 1));
    assert(pages.slice(0, -1).every((page) => page.stats.reconstructionBudgetExhausted));
  }
}

async function assertSource(handle, source) {
  await assertView(handle, source);
  for (const query of corpus.queries.filter((candidate) => candidate.source === source)) {
    await assertQuery(handle, source, query);
  }
}

test('native capability response implements the current contract', () => {
  const profile = capabilities();
  assert.equal(profile.contractVersion, 2);
  assert.equal(profile.profile, 'native');
  for (const field of capabilitySchema.required) assert(Object.hasOwn(profile, field), field);
  assert.equal(profile.draftFormatEpoch, 1);
  assert.equal(new Set(profile.operations).size, profile.operations.length);
  assert(profile.operations.includes('scan'));
  assert.equal(profile.sql, profile.operations.includes('querySql'));
  assert(!profile.sql || (profile.columnar && profile.arrowIpc));
  assert.deepEqual(profile.cancellation, { scan: true, lifecycle: true });
});

test('native writer and snapshots replay the shared corpus', async (t) => {
  assert.equal(corpus.contractVersion, 1);
  const file = temporaryStore(t);
  const store = await NativeStore.openFile(file);
  const snapshots = new Map();
  t.after(async () => {
    for (const snapshot of snapshots.values()) {
      try { await snapshot.close(); } catch {}
    }
    try { await store.close(); } catch {}
  });

  await assert.rejects(
    store.write([{
      kind: 'put',
      id: 'invalid-uppercase-float-bits',
      attrs: [{ name: 'float', kind: 'float', floatBits: '7FF8000000000001' }],
    }]),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );
  await assert.rejects(
    store.write([{
      kind: 'put',
      id: 'invalid-disagreeing-float-lanes',
      attrs: [{
        name: 'float', kind: 'float', floatValue: 1, floatBits: '4000000000000000',
      }],
    }]),
    (error) => error instanceof TurnDbError && error.code === 'INVALID_ARGUMENT',
  );

  for (const step of corpus.steps) {
    switch (step.action) {
      case 'apply':
        await store.write([
          ...step.puts.map(recordToWrite),
          ...step.deletes.map((id) => ({ kind: 'delete', id })),
        ]);
        break;
      case 'sync': await store.sync(); break;
      case 'flush': await store.flush(); break;
      case 'captureSnapshot': {
        const snapshot = await NativeSnapshot.openFile(file);
        snapshots.set(step.name, snapshot);
        await assertSource(snapshot, step.name);
        break;
      }
      case 'assertWriter': await assertSource(store, step.name); break;
      default: throw new Error(`unknown corpus action ${step.action}`);
    }
  }
  for (const [source, snapshot] of snapshots) await assertSource(snapshot, source);
  const backupPath = `${file}.backup.turndb`;
  const backup = await store.backup(backupPath);
  assert(backup.bytes > 0n);
  const backupSnapshot = await NativeSnapshot.openFile(backupPath);
  try {
    await assertSource(backupSnapshot, 'snapshot-v2');
  } finally {
    await backupSnapshot.close();
  }
});

test('native read-only path opens the checked-in physical fixture', async (t) => {
  const target = temporaryStore(t);
  const hex = fs.readFileSync(path.join(conformanceDir, 'fixture.turndb.hex'), 'utf8');
  assert.match(hex, /^(?:[0-9a-f]{2}|\s)+$/);
  const compact = hex.replaceAll(/\s/g, '');
  assert.equal(compact.length % 2, 0);
  fs.writeFileSync(target, Buffer.from(compact, 'hex'));
  const snapshot = await NativeSnapshot.openFile(target);
  t.after(async () => {
    try { await snapshot.close(); } catch {}
  });
  await assertSource(snapshot, 'snapshot-v2');
});
