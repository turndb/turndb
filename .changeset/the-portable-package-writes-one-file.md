---
default: minor
---

# The portable package writes one file

`open(path)` in the `turndb` npm package now names a `.turndb` **file**, not a store directory:
the WASI preopen is the file's parent, the store grows inside the single file, and the `-wal`
sidecar lives beside it under the same mount. Parent directories are created exactly as the
directory open always created its own. Opening an existing directory refuses and names `convert`
as the retired layout's one door.

The whole portable suite — and the two-way native/WASI interoperability proof — now runs on
single-file stores, which also hardened the engine underneath: a native open now refuses a fold
segment whose committed bytes scan short ("the fold lost durable data"), the same claim the
directory layout made through its committed-tail check.
