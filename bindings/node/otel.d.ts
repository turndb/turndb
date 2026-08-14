import type { NativeStore, WriteOp } from './index';

export interface NormalizedSpan {
  traceId: string;
  spanId: string;
  parentSpanId?: string;
  name: string;
  kind?: string;
  startTimeUnixNano: string | bigint | [number, number];
  endTimeUnixNano: string | bigint | [number, number];
  status?: { code?: string; message?: string };
  attributes?: Record<string, unknown> | Array<[string, unknown]>;
  events?: unknown[];
  links?: unknown[];
}

export interface ExportResult { code: 0 | 1; error?: Error }
export interface ExporterOptions {
  durableEveryExport?: boolean;
  flushEverySpans?: number;
  flushIntervalMs?: number;
}

export declare function mapNormalizedSpan(span: NormalizedSpan): WriteOp;
export declare function mapReadableSpan(span: object): WriteOp;

export interface GenAiCallOptions {
  operationName: string;
  providerName?: string;
  model?: string;
  spanName?: string;
  inputMessages?: unknown;
  outputMessages?: unknown | ((result: unknown) => unknown | Promise<unknown>);
  attributes?: Record<string, string | number | boolean>;
}

/** Trace one provider-client call without changing its return value or exception. */
export declare function traceGenAiCall<T>(
  tracer: object,
  options: GenAiCallOptions & {
    outputMessages?: unknown | ((result: T) => unknown | Promise<unknown>);
  },
  call: () => T | Promise<T>,
): Promise<T>;

export declare class TurnDbSpanExporter {
  constructor(pathOrStore: string | NativeStore, options?: ExporterOptions);
  export(spans: object[], callback: (result: ExportResult) => void): void;
  forceFlush(): Promise<void>;
  shutdown(): Promise<void>;
}
