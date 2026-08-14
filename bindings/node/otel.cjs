'use strict';

const { NativeStore } = require('./index.cjs');

const CONTENT_ATTRIBUTES = new Set([
  'gen_ai.system_instructions',
  'gen_ai.input.messages',
  'gen_ai.output.messages',
  'gen_ai.tool.definitions',
]);

const KIND = ['INTERNAL', 'SERVER', 'CLIENT', 'PRODUCER', 'CONSUMER'];
const STATUS = ['UNSET', 'OK', 'ERROR'];

function stable(value) {
  if (Array.isArray(value)) return value.map(stable);
  if (value && typeof value === 'object' && !Buffer.isBuffer(value) && !(value instanceof Uint8Array)) {
    return Object.fromEntries(Object.keys(value).sort().map((key) => [key, stable(value[key])]));
  }
  return value;
}

function stableJson(value) {
  return JSON.stringify(stable(value));
}

function timeNs(value) {
  if (typeof value === 'bigint') return value;
  if (typeof value === 'string') return BigInt(value);
  if (Array.isArray(value) && value.length === 2) return BigInt(value[0]) * 1_000_000_000n + BigInt(value[1]);
  if (typeof value === 'number' && Number.isSafeInteger(value)) return BigInt(value);
  throw new TypeError('span time must be bigint, decimal text, or [seconds,nanoseconds]');
}

function scalar(name, value) {
  if (typeof value === 'string') return { name, kind: 'string', stringValue: value };
  if (typeof value === 'boolean') return { name, kind: 'bool', boolValue: value };
  if (typeof value === 'bigint') return { name, kind: 'int', intValue: value };
  if (typeof value === 'number' && Number.isSafeInteger(value)) {
    return { name, kind: 'int', intValue: BigInt(value) };
  }
  if (typeof value === 'number') return { name, kind: 'float', floatValue: value };
  if (value instanceof Uint8Array) {
    return { name, kind: 'binary', binaryValue: Buffer.from(value) };
  }
  if (value === null) return { name, kind: 'null' };
  throw new TypeError(`OpenTelemetry attribute ${JSON.stringify(name)} is not a scalar`);
}

function normalizedAttributes(attributes) {
  if (Array.isArray(attributes)) return attributes;
  return Object.entries(attributes ?? {});
}

function assertHex(value, length, field) {
  if (typeof value !== 'string' || !new RegExp(`^[0-9a-f]{${length}}$`).test(value)) {
    throw new TypeError(`${field} must be ${length} lowercase hexadecimal digits`);
  }
  return value;
}

function mapNormalizedSpan(span) {
  const traceId = assertHex(span.traceId, 32, 'traceId');
  const spanId = assertHex(span.spanId, 16, 'spanId');
  const parentSpanId = span.parentSpanId == null || span.parentSpanId === ''
    ? null
    : assertHex(span.parentSpanId, 16, 'parentSpanId');
  const start = timeNs(span.startTimeUnixNano);
  const end = timeNs(span.endTimeUnixNano);
  if (start < 0n || end < start) throw new TypeError('span times must be non-negative and ordered');
  const attrs = [
    scalar('otel.trace_id', traceId),
    scalar('otel.span_id', spanId),
    ...(parentSpanId ? [scalar('otel.parent_span_id', parentSpanId)] : []),
    scalar('otel.name', String(span.name)),
    scalar('otel.kind', String(span.kind ?? 'INTERNAL')),
    { name: 'otel.start_time_unix_nano', kind: 'timestamp_ns', timestampNsValue: start },
    { name: 'otel.end_time_unix_nano', kind: 'timestamp_ns', timestampNsValue: end },
    { name: 'otel.duration_ns', kind: 'int', intValue: end - start },
    scalar('otel.status.code', String(span.status?.code ?? 'UNSET')),
    ...(span.status?.message ? [scalar('otel.status.message', String(span.status.message))] : []),
  ];
  const contents = [];
  for (const [name, value] of normalizedAttributes(span.attributes)) {
    if (CONTENT_ATTRIBUTES.has(name)) {
      const bytes = typeof value === 'string' ? Buffer.from(value) : Buffer.from(stableJson(value));
      contents.push({ name, bytes });
    } else if (Array.isArray(value)) {
      attrs.push(...value.map((item) => scalar(name, item)));
    } else {
      attrs.push(scalar(name, value));
    }
  }
  if (span.events !== undefined) contents.push({ name: 'otel.events', bytes: Buffer.from(stableJson(span.events)) });
  if (span.links !== undefined) contents.push({ name: 'otel.links', bytes: Buffer.from(stableJson(span.links)) });
  return {
    kind: 'put',
    id: `span/${traceId}/${start.toString().padStart(20, '0')}/${spanId}`,
    attrs,
    contents,
  };
}

function normalizeReadableSpan(span) {
  const context = span.spanContext();
  const parent = span.parentSpanContext?.() ?? span.parentSpanId;
  return {
    traceId: context.traceId,
    spanId: context.spanId,
    parentSpanId: typeof parent === 'string' ? parent : parent?.spanId,
    name: span.name,
    kind: typeof span.kind === 'number' ? KIND[span.kind] ?? `KIND_${span.kind}` : span.kind,
    startTimeUnixNano: span.startTime,
    endTimeUnixNano: span.endTime,
    status: {
      code: typeof span.status?.code === 'number' ? STATUS[span.status.code] ?? `STATUS_${span.status.code}` : span.status?.code,
      message: span.status?.message,
    },
    attributes: span.attributes,
    events: span.events,
    links: span.links,
  };
}

function mapReadableSpan(span) {
  return mapNormalizedSpan(normalizeReadableSpan(span));
}

function contentAttribute(value) {
  return typeof value === 'string' ? value : stableJson(value);
}

/**
 * Run one provider-client call inside a canonical gen_ai CLIENT span.
 *
 * This deliberately accepts an OpenTelemetry tracer and a zero-argument closure instead of taking
 * a dependency on one provider SDK. The closure retains its exact return value and exception; the
 * wrapper only describes the call in the vocabulary consumed by TurnDbSpanExporter.
 */
async function traceGenAiCall(tracer, options, call) {
  if (!tracer || typeof tracer.startActiveSpan !== 'function') {
    throw new TypeError('tracer must provide startActiveSpan');
  }
  if (!options || typeof options !== 'object') throw new TypeError('options must be an object');
  if (typeof call !== 'function') throw new TypeError('call must be a function');
  if (typeof options.operationName !== 'string' || options.operationName.length === 0) {
    throw new TypeError('operationName must be a non-empty string');
  }
  const attributes = {
    ...(options.attributes ?? {}),
    'gen_ai.operation.name': options.operationName,
  };
  if (options.providerName !== undefined) attributes['gen_ai.provider.name'] = options.providerName;
  if (options.model !== undefined) attributes['gen_ai.request.model'] = options.model;
  if (options.inputMessages !== undefined) {
    attributes['gen_ai.input.messages'] = contentAttribute(options.inputMessages);
  }
  const spanName = options.spanName
    ?? `${options.operationName}${options.model === undefined ? '' : ` ${options.model}`}`;
  return tracer.startActiveSpan(spanName, { kind: 2, attributes }, async (span) => {
    try {
      const result = await call();
      const selected = typeof options.outputMessages === 'function'
        ? await options.outputMessages(result)
        : options.outputMessages;
      if (selected !== undefined) {
        span.setAttribute('gen_ai.output.messages', contentAttribute(selected));
      }
      span.setStatus?.({ code: 1 });
      return result;
    } catch (error) {
      span.recordException?.(error);
      span.setStatus?.({ code: 2, message: error instanceof Error ? error.message : String(error) });
      throw error;
    } finally {
      span.end();
    }
  });
}

class TurnDbSpanExporter {
  constructor(pathOrStore, options = {}) {
    this.pathOrStore = pathOrStore;
    this.ownsStore = typeof pathOrStore === 'string';
    this.storePromise = this.ownsStore ? NativeStore.openFile(pathOrStore) : Promise.resolve(pathOrStore);
    this.durableEveryExport = options.durableEveryExport ?? true;
    this.flushEverySpans = options.flushEverySpans ?? 512;
    this.flushIntervalMs = options.flushIntervalMs ?? 5000;
    this.pendingSpans = 0;
    this.lastFlush = Date.now();
    this.tail = Promise.resolve();
    this.stopped = false;
  }

  export(spans, callback) {
    if (this.stopped) {
      callback({ code: 1, error: new Error('TurnDB exporter is shut down') });
      return;
    }
    this.tail = this.tail.then(async () => {
      const store = await this.storePromise;
      await store.write(spans.map(mapReadableSpan), this.durableEveryExport);
      this.pendingSpans += spans.length;
      const due = this.pendingSpans >= this.flushEverySpans
        || Date.now() - this.lastFlush >= this.flushIntervalMs;
      if (due) {
        if (!this.durableEveryExport) await store.sync();
        await store.flush();
        this.pendingSpans = 0;
        this.lastFlush = Date.now();
      }
    });
    this.tail.then(() => callback({ code: 0 }), (error) => callback({ code: 1, error }));
  }

  forceFlush() {
    this.tail = this.tail.then(async () => {
      const store = await this.storePromise;
      await store.sync();
      await store.flush();
      this.pendingSpans = 0;
      this.lastFlush = Date.now();
    });
    return this.tail;
  }

  shutdown() {
    if (this.stopped) return this.tail;
    this.stopped = true;
    this.tail = this.forceFlush().then(async () => {
      if (this.ownsStore) await (await this.storePromise).close(true);
    });
    return this.tail;
  }
}

module.exports = { TurnDbSpanExporter, mapNormalizedSpan, mapReadableSpan, traceGenAiCall };
