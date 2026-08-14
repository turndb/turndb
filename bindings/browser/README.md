# TurnDB in the browser

This package is the read-only contract-v1 core for `wasm32-unknown-unknown`. `BrowserDatabase`
opens a `Uint8Array`, `Blob`/`File`, or HTTP URL. Blob and HTTP use a bounded 64 KiB block cache;
the wasm engine reports an exact missing byte range and the JavaScript layer fills only that block.

HTTP servers must return `206` plus an exact `Content-Range` and allow `Range`/`Content-Range`
through CORS. A `200` whole-file fallback is refused.

Query failures are `TurnDbError` instances with the same stable engine `code` taxonomy as the
native SDKs. Transport failures use `IO`; an HTTP status or malformed `Content-Range` is never
silently turned into a full-file read.

Build the one-file viewer with:

```sh
node bindings/browser/build-viewer.mjs
```

The resulting `turndb-viewer.html` contains its JavaScript and engine wasm. It makes no external
request unless the reader supplies a store URL.
