import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';

import { capabilities, open, openFile, TurndbError } from '../index.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const conformanceDir = resolve(here, '../../../conformance/v1');
const capabilityConformanceDir = resolve(here, '../../../conformance/v2');
const corpus = JSON.parse(await readFile(join(conformanceDir, 'corpus.json'), 'utf8'));
const capabilitySchema = JSON.parse(
  await readFile(join(capabilityConformanceDir, 'capabilities.schema.json'), 'utf8'),
);

function scalarToPortable(scalar) {
  switch (scalar.type) {
    case 'string': return scalar.value;
    case 'i64': return { i: BigInt(scalar.decimal) };
    case 'f64': return { fBits: scalar.bitsHex };
    case 'bool': return scalar.value;
    case 'u64': return { u: BigInt(scalar.decimal) };
    case 'binary': return Buffer.from(scalar.base64, 'base64');
    case 'timestampNs': return { timestampNs: BigInt(scalar.decimal) };
    case 'null': return null;
    default: throw new Error(`unknown contract scalar ${scalar.type}`);
  }
}

function f64Bits(value) {
  const bytes = Buffer.alloc(8);
  bytes.writeDoubleBE(value);
  return bytes.readBigUInt64BE().toString(16).padStart(16, '0');
}

function portableAttrToContract([name, value]) {
  if (typeof value === 'string') return { name, value: { type: 'string', value } };
  if (typeof value === 'bigint') return { name, value: { type: 'i64', decimal: String(value) } };
  if (typeof value === 'number') return { name, value: { type: 'f64', bitsHex: f64Bits(value) } };
  if (typeof value === 'boolean') return { name, value: { type: 'bool', value } };
  if (value === null) return { name, value: { type: 'null' } };
  if (value instanceof Uint8Array) {
    return { name, value: { type: 'binary', base64: Buffer.from(value).toString('base64') } };
  }
  if ('u' in value) return { name, value: { type: 'u64', decimal: String(value.u) } };
  if ('timestampNs' in value) {
    return { name, value: { type: 'timestampNs', decimal: String(value.timestampNs) } };
  }
  if ('fBits' in value) return { name, value: { type: 'f64', bitsHex: value.fBits } };
  throw new Error(`unknown portable scalar ${String(value)}`);
}

function recordToWrite(record) {
  return {
    kind: 'put',
    id: record.id,
    attrs: record.attrs.map(({ name, value }) => [name, scalarToPortable(value)]),
    contents: record.contents.map(({ name, base64 }) => ({
      name,
      bytes: Buffer.from(base64, 'base64'),
    })),
  };
}

function requestToPortable(request) {
  assert.equal(request.contractVersion, 1);
  const portable = { ...request };
  delete portable.contractVersion;
  if (portable.maxReconstructedBytes !== undefined) {
    portable.maxReconstructedBytes = BigInt(portable.maxReconstructedBytes);
  }
  portable.predicates = (portable.predicates ?? []).map((predicate) => {
    switch (predicate.kind) {
      case 'id': return predicate;
      case 'attr': return { ...predicate, value: scalarToPortable(predicate.value) };
      case 'attrExists': return { ...predicate, kind: 'attr_exists' };
      case 'contentExists': return { ...predicate, kind: 'content_exists' };
      default: throw new Error(`unknown predicate ${predicate.kind}`);
    }
  });
  return portable;
}

function viewFor(source) {
  const views = corpus.views.filter((view) => view.source === source);
  assert.equal(views.length, 1, `one view for ${source}`);
  return views[0];
}

function assertProjectedRow(row, expected, request) {
  const attrs = new Set(request.attrs ?? []);
  assert.deepEqual(
    row.attrs.map(portableAttrToContract),
    expected.attrs.filter((attr) => attrs.has(attr.name)),
    `${row.id} projected attrs`,
  );
  assert.equal(row.contents.length, (request.contents ?? []).length);
  for (let i = 0; i < row.contents.length; i++) {
    const projected = row.contents[i];
    const selected = request.contents[i];
    const content = expected.contents.find((candidate) => candidate.name === selected.name);
    assert.equal(projected.name, selected.name);
    assert.equal(projected.present, content !== undefined);
    if (content === undefined) {
      assert.equal(projected.len, undefined);
      assert.equal(projected.bytes, undefined);
      continue;
    }
    const bytes = Buffer.from(content.base64, 'base64');
    assert.equal(projected.len, BigInt(bytes.length));
    assert.equal(projected.pieces, content.storage === 'piece' && bytes.length > 0 ? 1 : 0);
    assert.match(projected.identity, /^[0-9a-f]{64}$/);
    if (selected.mode === 'bytes') assert.deepEqual(projected.bytes, bytes);
    else assert.equal(projected.bytes, undefined);
  }
}

function assertView(handle, source) {
  const view = viewFor(source);
  const attrs = [...new Set(view.records.flatMap((record) => record.attrs.map((attr) => attr.name)))];
  const contentNames = [
    ...new Set(view.records.flatMap((record) => record.contents.map((content) => content.name))),
  ];
  const page = handle.scan({
    limit: 100,
    attrs,
    contents: contentNames.map((name) => ({ name, mode: 'bytes' })),
  });
  assert.equal(page.next, undefined);
  assert.deepEqual(page.rows.map((row) => row.id), view.records.map((record) => record.id));
  for (const row of page.rows) {
    const expected = view.records.find((record) => record.id === row.id);
    assert.deepEqual(row.attrs.map(portableAttrToContract), expected.attrs, `${source}/${row.id}`);
    assertProjectedRow(row, expected, { attrs, contents: contentNames.map((name) => ({ name, mode: 'bytes' })) });
  }
}

function assertQuery(handle, source, query) {
  const request = requestToPortable(query.request);
  const pages = [];
  const ids = [];
  let cursor;
  do {
    const page = handle.scan({ ...request, ...(cursor === undefined ? {} : { cursor }) });
    pages.push(page);
    ids.push(...page.rows.map((row) => row.id));
    for (const row of page.rows) {
      assertProjectedRow(
        row,
        viewFor(source).records.find((record) => record.id === row.id),
        query.request,
      );
    }
    cursor = query.paginate ? page.next : undefined;
  } while (cursor !== undefined);
  assert.deepEqual(ids, query.expectedIds, query.name);

  if (query.assertMetadataOnlyIo) {
    for (const page of pages) {
      assert.equal(page.stats.io.foldBlocksTouched, 0n);
      assert.equal(page.stats.io.foldStoredBytesRead, 0n);
      assert.equal(page.stats.reconstructedBytes, 0n);
    }
  }
  if (query.name === 'content-budget-refuses-to-truncate') {
    assert(pages.every((page) => page.rows.length === 1));
    assert(pages.slice(0, -1).every((page) => page.stats.reconstructionBudgetExhausted));
  }
  if (query.assertCursorDamageRejected) {
    const first = handle.scan(request);
    assert(first.next);
    const damaged = `${first.next.slice(0, -1)}${first.next.endsWith('A') ? 'B' : 'A'}`;
    assert.throws(
      () => handle.scan({ ...request, cursor: damaged }),
      (error) => error instanceof TurndbError && error.code === 'INVALID_ARGUMENT',
    );
  }
  if (query.assertCursorMismatchRejected) {
    const first = handle.scan(request);
    assert(first.next);
    assert.throws(
      () => handle.scan({ ...request, direction: 'forward', cursor: first.next }),
      (error) => error instanceof TurndbError && error.code === 'INVALID_ARGUMENT',
    );
  }
}

function assertSource(handle, source) {
  assertView(handle, source);
  for (const query of corpus.queries.filter((candidate) => candidate.source === source)) {
    assertQuery(handle, source, query);
  }
}

async function temporaryPath(t) {
  const root = await mkdtemp(join(tmpdir(), 'turndb-conformance-wasi-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  return join(root, 'fixture.turndb');
}

test('portable capability response implements contract v2', async () => {
  const profile = await capabilities();
  for (const field of capabilitySchema.required) assert(Object.hasOwn(profile, field), field);
  assert.equal(profile.contractVersion, 2);
  assert.equal(profile.profile, 'wasi');
  assert.equal(profile.writerExclusion, 'embedder_enforced');
  assert.equal(profile.threads, false);
  assert.equal(profile.sql, false);
  assert.equal(profile.arrowIpc, false);
  assert.equal(new Set(profile.operations).size, profile.operations.length);
  assert(profile.operations.includes('scan'));
  assert.deepEqual(profile.cancellation, { scan: true, lifecycle: true });
});

test('portable writer and read-only handles replay the shared corpus', async (t) => {
  assert.equal(corpus.contractVersion, 1);
  const file = await temporaryPath(t);
  const apply = (store, step) => store.write([
    ...step.puts.map(recordToWrite),
    ...step.deletes.map((id) => ({ kind: 'delete', id })),
  ]);

  let store = await open(file);
  try {
    assert.throws(
      () => store.write([{
        kind: 'put', id: 'bad-bits', attrs: [['float', { fBits: '7FF8000000000001' }]], contents: [],
      }]),
      /sixteen lowercase hexadecimal digits/,
    );
    apply(store, corpus.steps.find((step) => step.name === 'published-v1'));
    store.sync();
    store.flush();
  } finally {
    store.close();
  }

  store = await openFile(file);
  try { assertSource(store, 'snapshot-v1'); } finally { store.close(); }

  store = await open(file);
  try {
    apply(store, corpus.steps.find((step) => step.name === 'overlay-v2'));
    assertSource(store, 'writer-overlay');
    store.sync();
    store.flush();
  } finally {
    store.close();
  }

  store = await openFile(file);
  try { assertSource(store, 'snapshot-v2'); } finally { store.close(); }
});

test('portable read-only path opens the checked-in physical fixture', async (t) => {
  const target = await temporaryPath(t);
  const hex = await readFile(join(conformanceDir, 'fixture.turndb.hex'), 'utf8');
  assert.match(hex, /^(?:[0-9a-f]{2}|\s)+$/);
  await writeFile(target, Buffer.from(hex.replaceAll(/\s/g, ''), 'hex'));
  const snapshot = await openFile(target);
  try { assertSource(snapshot, 'snapshot-v2'); } finally { snapshot.close(); }
});
