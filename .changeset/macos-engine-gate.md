---
default: patch
---

# macOS x86-64 and Apple silicon run the engine suite as required gates

Through 0.1.8 macOS proved only its `@turndb/cli` slices, built and driven on `macos-15-intel` and
`macos-15` on pull requests; the engine test suite, the crash sweeps and the cross-OS byte-compare
never ran there, so the macOS arms of `src/sys.rs` were exercised only through that drive. Both
runners now run clippy, the debug, SQL-off, corruption and release-profile suites, the crash sweeps
under both durability models, and the reference store byte-compared in both directions against the
Linux x86-64 fixture, as required gates on every push and pull request. The release path installs
and drives the CLI on both macOS runners before publishing. The support policy states what that
qualifies; native Node and Python packages for macOS remain open.
