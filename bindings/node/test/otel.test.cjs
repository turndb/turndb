'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');
const { NativeSnapshot } = require('..');
const { TurnDbSpanExporter, mapNormalizedSpan, traceGenAiCall } = require('../otel.cjs');

const vector = JSON.parse(fs.readFileSync(
  path.resolve(__dirname, '../../../conformance/v1/trace-mapping.json'),
  'utf8',
));

test('Node trace mapper implements the shared vector', () => {
  const record = mapNormalizedSpan(vector.span);
  assert.equal(record.id, vector.expected.id);
  assert.deepEqual(record.attrs.map((attribute) => attribute.name), vector.expected.attributeNames);
  assert.deepEqual(
    Object.fromEntries(record.contents.map((content) => [content.name, content.bytes.toString('base64')])),
    vector.expected.contents,
  );
});

test('trace mapper rejects alternate id spellings and unordered times', () => {
  assert.throws(() => mapNormalizedSpan({ ...vector.span, traceId: vector.span.traceId.toUpperCase() }));
  assert.throws(() => mapNormalizedSpan({
    ...vector.span,
    endTimeUnixNano: (BigInt(vector.span.startTimeUnixNano) - 1n).toString(),
  }));
});

test('thin gen_ai wrapper records canonical content and preserves client behavior', async () => {
  const observed = {};
  const span = {
    setAttribute(name, value) { (observed.output ??= {})[name] = value; },
    setStatus(status) { observed.status = status; },
    recordException(error) { observed.exception = error; },
    end() { observed.ended = (observed.ended ?? 0) + 1; },
  };
  const tracer = {
    startActiveSpan(name, options, callback) {
      observed.name = name;
      observed.options = options;
      return callback(span);
    },
  };
  const response = { output: [{ role: 'assistant', content: 'hello' }] };
  assert.equal(await traceGenAiCall(tracer, {
    operationName: 'chat',
    providerName: 'test',
    model: 'small',
    inputMessages: [{ role: 'user', content: 'hi' }],
    outputMessages: (value) => value.output,
  }, async () => response), response);
  assert.equal(observed.name, 'chat small');
  assert.equal(observed.options.kind, 2);
  assert.equal(observed.options.attributes['gen_ai.provider.name'], 'test');
  assert.equal(
    observed.options.attributes['gen_ai.input.messages'],
    '[{"content":"hi","role":"user"}]',
  );
  assert.equal(observed.output['gen_ai.output.messages'], '[{"content":"hello","role":"assistant"}]');
  assert.deepEqual(observed.status, { code: 1 });
  assert.equal(observed.ended, 1);

  const failure = new Error('provider refused');
  await assert.rejects(traceGenAiCall(tracer, { operationName: 'chat' }, async () => {
    throw failure;
  }), (error) => error === failure);
  assert.equal(observed.exception, failure);
  assert.deepEqual(observed.status, { code: 2, message: 'provider refused' });
  assert.equal(observed.ended, 2);
});

test('Node OpenTelemetry exporter writes a reader-visible local file', async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'turndb-node-otel-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const file = path.join(root, 'agent.turndb');
  const span = {
    spanContext: () => ({ traceId: vector.span.traceId, spanId: vector.span.spanId }),
    parentSpanContext: () => undefined,
    name: vector.span.name,
    kind: 0,
    startTime: vector.span.startTimeUnixNano,
    endTime: vector.span.endTimeUnixNano,
    status: { code: 1 },
    attributes: { 'agent.framework': 'test' },
    events: [],
    links: [],
  };
  const exporter = new TurnDbSpanExporter(file, { flushEverySpans: 1 });
  const result = await new Promise((resolve) => exporter.export([span], resolve));
  assert.equal(result.code, 0);
  await exporter.shutdown();
  const snapshot = await NativeSnapshot.openFile(file);
  try {
    const page = await snapshot.scan({ limit: 10, attrs: ['otel.name'] });
    assert.deepEqual(page.rows.map((row) => row.id), [vector.expected.id]);
  } finally {
    await snapshot.close();
  }
});
