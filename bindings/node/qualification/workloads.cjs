'use strict';

const sharedTelemetryContext = Buffer.from(
  JSON.stringify([{ role: 'system', content: 'be exact' }, { role: 'user', content: 'status?' }]),
);
const sharedSourceManifest = Buffer.from('src/index.ts\npackage.json\n');

// The first fixture resembles linked application/AI telemetry but intentionally uses no OTel
// adapter or semantic-convention dependency. Its names are consumer data, not database API.
const linkedTelemetry = {
  name: 'linked telemetry',
  correlation: { name: 'run.key', type: 'string', value: 'run-7' },
  expectedCorrelatedIds: [
    'tenant-a/0001/activity',
    'tenant-a/0002/model-call',
    'tenant-a/0003/tool-call',
    'tenant-a/0004/provider-exchange',
    'tenant-a/0005/ingest-diagnostic',
  ],
  shared: [
    { id: 'tenant-a/0001/activity', content: 'context' },
    { id: 'tenant-a/0002/model-call', content: 'request' },
  ],
  selected: {
    id: 'tenant-a/0002/model-call',
    name: 'response',
    bytes: Buffer.from('{"status":"ok"}'),
  },
  records: [
    {
      id: 'tenant-a/0001/activity',
      fields: [
        { name: 'record.family', type: 'string', value: 'activity' },
        { name: 'run.key', type: 'string', value: 'run-7' },
        { name: 'occurred_at', type: 'timestamp_ns', value: 1000n },
      ],
      contents: [{ name: 'context', bytes: sharedTelemetryContext }],
    },
    {
      id: 'tenant-a/0002/model-call',
      fields: [
        { name: 'record.family', type: 'string', value: 'model-call' },
        { name: 'run.key', type: 'string', value: 'run-7' },
        { name: 'input.tokens', type: 'uint', value: 17n },
      ],
      contents: [
        { name: 'request', bytes: sharedTelemetryContext },
        { name: 'response', bytes: Buffer.from('{"status":"ok"}') },
      ],
    },
    {
      id: 'tenant-a/0003/tool-call',
      fields: [
        { name: 'record.family', type: 'string', value: 'tool-call' },
        { name: 'run.key', type: 'string', value: 'run-7' },
        { name: 'tool.success', type: 'bool', value: true },
      ],
      contents: [{ name: 'arguments', bytes: Buffer.from('{"path":"README.md"}') }],
    },
    {
      id: 'tenant-a/0004/provider-exchange',
      fields: [
        { name: 'record.family', type: 'string', value: 'provider-exchange' },
        { name: 'run.key', type: 'string', value: 'run-7' },
      ],
      contents: [{ name: 'raw', bytes: Buffer.from('HTTP/1.1 200 OK\r\n\r\n{}') }],
    },
    {
      id: 'tenant-a/0005/ingest-diagnostic',
      fields: [
        { name: 'record.family', type: 'string', value: 'ingest-diagnostic' },
        { name: 'run.key', type: 'string', value: 'run-7' },
        { name: 'warning', type: 'null' },
      ],
    },
  ],
};

// A deliberately non-telemetry workload uses the exact same adapter and assertions. These records
// describe a build pipeline; no trace-specific record family or correlation rule is required.
const buildPipeline = {
  name: 'build pipeline',
  correlation: { name: 'build.key', type: 'string', value: 'build-42' },
  expectedCorrelatedIds: [
    'project-x/0001/job',
    'project-x/0002/compile-step',
    'project-x/0003/artifact',
    'project-x/0004/diagnostic',
  ],
  shared: [
    { id: 'project-x/0001/job', content: 'inputs' },
    { id: 'project-x/0003/artifact', content: 'source-manifest' },
  ],
  selected: {
    id: 'project-x/0002/compile-step',
    name: 'stderr',
    bytes: Buffer.from('warning: unused variable\n'),
  },
  records: [
    {
      id: 'project-x/0001/job',
      fields: [
        { name: 'record.family', type: 'string', value: 'job' },
        { name: 'build.key', type: 'string', value: 'build-42' },
        { name: 'attempt', type: 'int', value: 2n },
      ],
      contents: [{ name: 'inputs', bytes: sharedSourceManifest }],
    },
    {
      id: 'project-x/0002/compile-step',
      fields: [
        { name: 'record.family', type: 'string', value: 'compile-step' },
        { name: 'build.key', type: 'string', value: 'build-42' },
        { name: 'duration_ms', type: 'float', value: 18.5 },
      ],
      contents: [{ name: 'stderr', bytes: Buffer.from('warning: unused variable\n') }],
    },
    {
      id: 'project-x/0003/artifact',
      fields: [
        { name: 'record.family', type: 'string', value: 'artifact' },
        { name: 'build.key', type: 'string', value: 'build-42' },
        { name: 'reproducible', type: 'bool', value: true },
      ],
      contents: [
        { name: 'source-manifest', bytes: sharedSourceManifest },
        { name: 'binary', bytes: Buffer.from([0, 1, 2, 3, 255]) },
      ],
    },
    {
      id: 'project-x/0004/diagnostic',
      fields: [
        { name: 'record.family', type: 'string', value: 'diagnostic' },
        { name: 'build.key', type: 'string', value: 'build-42' },
        { name: 'worker', type: 'binary', value: Buffer.from([0xde, 0xad]) },
      ],
    },
  ],
};

module.exports = { buildPipeline, linkedTelemetry };
