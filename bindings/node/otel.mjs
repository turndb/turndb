import otel from './otel.cjs';

export const TurnDbSpanExporter = otel.TurnDbSpanExporter;
export const mapNormalizedSpan = otel.mapNormalizedSpan;
export const mapReadableSpan = otel.mapReadableSpan;
export const traceGenAiCall = otel.traceGenAiCall;
export default otel;
