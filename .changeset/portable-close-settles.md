---
default: patch
---

# The portable binding settles a writer on close

`close()` on the portable (`wasm32-wasip1`) `Store` now performs settlement when the handle has no
pending change set: the redundant write-ahead log is truncated and removed, so a clean close leaves
exactly the one container file `FORMAT.md` promises. Previously `tdb_close` dropped the handle
without settling, which left a zero-length `-wal` beside every store this binding ever closed —
observed on Node with `node:wasi` and on the memory host alike. A writer with accepted, unpublished
mutations keeps its WAL for replay; `close()` still publishes nothing the caller did not ask for.
