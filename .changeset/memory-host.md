---
default: minor
---

# A memory host for the portable engine

`turndb/memory` runs the same `wasm32-wasip1` engine over an in-memory directory with a
purpose-built WASI Preview 1 host (`wasi-memfs.mjs`): the twenty-two imports the engine uses and no
others, so a browser Web Worker, a Cloudflare Worker, Bun, or Deno can write a store and read the
settled container bytes back out. `MemoryHost.create({ module })` takes a compiled module, a
`Response`, or the bytes; `open`, `openFile`, `read`, `write`, `list`, and `remove` are the door to
its directory, and several stores may be open on one host.

`index.mjs` is now the Node host over a shared `core.mjs`; its API and behaviour are unchanged
(the existing 67-test suite passes unmodified). The host states what it gives up: durability
barriers are no-ops over memory, and the store lives in the host's memory until the host is
dropped. `npm/turndb/test/memory-host.mjs` holds the WASI layer to POSIX semantics call by call and
proves an exported container is one the Node host opens and the native CLI verifies deep.
