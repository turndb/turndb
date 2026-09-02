---
default: patch
---

# Linux arm64 is built, tested and driven on arm64 hardware

The `@turndb/cli` Linux arm64 slice shipped at 0.1.8 cross-compiled on an x86-64 runner, and no job
in this repository had ever executed on arm64 hardware, so nothing proved that binary ran. GitHub's
hosted `ubuntu-22.04-arm` runner is real arm64 hardware, free on a public repository, and every
Linux arm64 claim now rests on it: the crate's clippy, debug, SQL-off, corruption and release-profile
suites, the crash sweeps under both durability models, and the reference store byte-compared in both
directions against the x86-64 Linux fixture are required gates on every push and pull request; the
CLI slice is built there, installed from its packed tarball and driven, in CI and in the release
install matrix. The support policy states what is qualified for Linux arm64 and, for the first time,
what is and is not proven for macOS, where the CLI slices are driven but the engine suite does not
run. No native Node or Python package is added for either.
