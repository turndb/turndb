# TurnDB conformance corpus

`v1/` is the frozen language-neutral query/runner contract and its original capability contract.
`v2/` advances only the capability profile after the `seal` operation was retired in favor of
`backup`; query requests and runner messages remain v1. Package versions and on-disk format versions
are independent of both contract versions. Incompatible semantics add a versioned artifact instead
of rewriting an old one.

The corpus is independent input and expected data. A runner must replay `corpus.json.steps`, compare
point reads and scans with `v1/views` and `v1/queries`, and validate its public capability response
against `v2/capabilities.schema.json` and `v2/capabilities.json`. It must not create expected results
by asking a different binding at test time.

`v1/fixture.turndb.hex` is immutable evidence of the byte-exact published-v2 container, encoded as
lowercase hex so repository
patches and reviews remain textual. Runners decode whitespace-separated hex to obtain the actual
`.turndb` file. The Rust gate proves both directions: replaying the operations produces exactly those
bytes, and a reader opened on those bytes produces the published-v2 goldens. Read-only/browser
runners use this fixture without needing a writer.

`v2/fixture.turndb.hex` is the current revision-3 writer's byte-exact output for the same corpus.
Writer-layout changes add a new fixture; they never regenerate an older one in place. The current
fixture may be regenerated with `TURNDB_UPDATE_CONFORMANCE_FIXTURE=1 cargo test --test conformance
rust_store_replays_the_shared_query_corpus`, followed by the same command without the environment
variable.

## Runner protocol v1

Out-of-process binding runners use newline-delimited JSON on standard input and output. Each request
has a caller-chosen `id`; each response repeats it. Messages are processed in order, but an adapter
may execute operations asynchronously internally.

Requests:

```json
{"id":"1","op":"capabilities"}
{"id":"2","op":"openWriter","path":"/tmp/example.turndb"}
{"id":"3","op":"openSnapshot","path":"/tmp/example.turndb"}
{"id":"4","op":"apply","handle":"writer","puts":[],"deletes":[]}
{"id":"5","op":"sync","handle":"writer"}
{"id":"6","op":"flush","handle":"writer"}
{"id":"7","op":"scan","handle":"snapshot","request":{"contractVersion":1}}
{"id":"8","op":"get","handle":"snapshot","recordId":"alpha"}
{"id":"9","op":"readContent","handle":"snapshot","recordId":"alpha","name":"body"}
{"id":"10","op":"close","handle":"snapshot"}
```

A successful response is `{"id":"…","ok":true,"value":…}`. A refusal is
`{"id":"…","ok":false,"error":{"code":"INVALID_ARGUMENT"}}`; rendered messages may be added
for diagnostics but are not golden data. Handle names are runner-local strings. Scalar, record,
request, page, and exact-counter encodings are the definitions in `query.schema.json`.

The protocol is deliberately smaller than the public SDK. It is test plumbing, not a second API.
