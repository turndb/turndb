---
default: patch
---

# The packaged CLI covers five targets and closes writer sessions cleanly

`@turndb/cli` now packages Linux x86-64 and arm64 GNU, macOS x86-64 and arm64, and Windows x86-64
MSVC binaries. `turndb --version`, `turndb version`, and `turndb -V` report the crate version compiled
into the selected platform binary.

Writer verbs now close the store they open before returning. A successful `compact`, `refold`,
`punch`, `erase`, or `seal` therefore removes its empty WAL sidecar and leaves the documented
single-file store shape instead of a stray zero-byte `<store>-wal`.
