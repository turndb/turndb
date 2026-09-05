# turndb for Python

The Python SDK is a thin PyO3 shell over TurnDB's single-file engine. It uses the same capability
and structured-query contract as Rust and Node. One dedicated Rust actor owns each writer, so
Python threads never concurrently enter mutable engine state.

What this package does **not** carry, so you choose it knowing: the columnar/Arrow lens and SQL
(`columnar: false`, `arrowIpc: false`, `sql: false`) and cooperative cancellation of scans and
lifecycle operations (`cancellation: {scan: false, lifecycle: false}`). `turndb.capabilities()`
reports exactly these; a consumer that needs SQL or cancellation uses the Rust crate or the native
Node package. Wheels are Linux x86-64 (manylinux); other platforms build from the sdist.

```python
from turndb import Store

db = Store.open("agent.turndb")
db.write([{"kind": "put", "id": "trace/1", "attrs": [], "contents": []}], durable=True)
db.backup("agent-backup.turndb")
db.close()
```

The backup result's string-valued `commit` field is the public store-authority encoding: `"0"`
means the canonical origin and a positive decimal value means that numbered manifest revision. Zero
never denotes a manifest revision.

`close()` releases the handle without synchronizing or publishing by default. Pass
`close(durable=True)` to synchronize, publish the pending change set, leave the store settled, and
remove the WAL sidecar; otherwise call `sync()` or `flush()` first according to the guarantee you
need.

Attributes, writes, scan requests, and scan results use the canonical data shapes in
`conformance/v1/query.schema.json`. Content bytes in those serializable shapes are base64. The
direct `read_content()` convenience returns `bytes`.

OpenTelemetry is the Tier-2 entrance. With an SDK provider already configured, tracing to one local
file is two lines:

```python
exporter = TurnDbSpanExporter("agent.turndb")
provider.add_span_processor(BatchSpanProcessor(exporter))
```

The exporter durably acknowledges each export by default, publishes after 512 spans or five
seconds, and always synchronizes durability and publishes on `force_flush()` and `shutdown()`.

Provider SDKs remain optional. `trace_gen_ai_call()` and `trace_gen_ai_call_async()` wrap any
client closure in the canonical `gen_ai` CLIENT span, move input/output message arrays onto the
content-bearing attributes consumed by the exporter, and preserve the exact return value or
exception.
