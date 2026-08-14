# TurnDB conformance corpus

`v1/` is the language-neutral semantic gate for Phase 3. Package versions and on-disk format
versions can change without changing this directory; incompatible contract semantics require a new
versioned directory.

The corpus is independent input and expected data. A runner must replay `corpus.json.steps`, compare
point reads and scans with `views` and `queries`, and validate its public capability response against
`capabilities.schema.json` and `capabilities.json`. It must not create expected results by asking a
different binding at test time.

`fixture.turndb.hex` is the byte-exact published-v2 container encoded as lowercase hex so repository
patches and reviews remain textual. Runners decode whitespace-separated hex to obtain the actual
`.turndb` file. The Rust gate proves both directions: replaying the operations produces exactly those
bytes, and a reader opened on those bytes produces the published-v2 goldens. Read-only/browser
runners use this fixture without needing a writer.

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
