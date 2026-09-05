# Trace mapping contract v1

Status: normative Phase 3 mapping for the first-party Node and Python OpenTelemetry exporters.
The storage engine remains domain-generic; this document belongs to the SDK layer.

One OpenTelemetry span maps to one TurnDB record. Its id is:

```text
span/<32-lower-hex-trace-id>/<20-digit-start-unix-ns>/<16-lower-hex-span-id>
```

That makes a trace a contiguous prefix range and time-orders its spans without an index. Exporters
reject malformed identifiers rather than create a second spelling of the same span.

## Attributes

The mapper writes `otel.trace_id`, `otel.span_id`, `otel.parent_span_id` (when present),
`otel.name`, `otel.kind`, `otel.start_time_unix_nano`, `otel.end_time_unix_nano`,
`otel.duration_ns`, `otel.status.code`, and `otel.status.message` (when present). Original OTel
attributes retain their names. Strings, booleans, signed integers, floats, bytes, and null keep
their scalar types. Attribute arrays become repeated attributes with the same name and original
order. Objects are not silently stringified.

The following semconv attributes are content, not metadata, because they carry repeated bulk JSON:

- `gen_ai.system_instructions`
- `gen_ai.input.messages`
- `gen_ai.output.messages`
- `gen_ai.tool.definitions`

Their UTF-8 bytes are stored under the same content name and the attribute is omitted. This is the
deduplication boundary: resent message-array elements can become shared pieces. Span events and
links are stable-key JSON under `otel.events` and `otel.links`; absent collections are absent
content, while an explicitly present empty collection is the exact bytes `[]`.

## Export cadence

Tier 2 owns policy. The defaults acknowledge every export batch durably, publish the pending change
set after 512 accepted spans or five seconds, and always perform synchronization and publication on
`forceFlush`/`shutdown` through `sync` and `flush`.
Changing cadence changes visibility and compression economics, never Tier-1 durability semantics.
An exporter serializes its calls through one queue; it does not issue concurrent store mutations.

## Thin client calls

Node `traceGenAiCall` and Python `trace_gen_ai_call`/`trace_gen_ai_call_async` instrument a provider
SDK closure without depending on that SDK. Each creates an OpenTelemetry CLIENT span with
`gen_ai.operation.name`, and optional `gen_ai.provider.name` and `gen_ai.request.model`. Input and
output messages are stable JSON on `gen_ai.input.messages` and `gen_ai.output.messages`, so the
exporters route them to named content. The wrapper returns the client's value unchanged, records
and rethrows its exception unchanged, and ends the span exactly once. Provider-specific adapters
therefore describe request and response extraction without defining another storage mapping.

The executable language-neutral vector is
[`conformance/v1/trace-mapping.json`](../conformance/v1/trace-mapping.json).
