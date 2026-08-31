---
default: patch
---

# Windows x64 packages install the qualified binaries

The native Node binding, the `@turndb/cli` command line, and CPython 3.9–3.13 now ship Windows
x86-64 packages. Release qualification installs the exact publish-shaped artifacts from closed
local registries, verifies their digests, and exercises the installed binaries — including punch
zeroing, reclaim, transient-name refusal and inventory, erasure by refold, and byte-identical
cross-OS opening in both directions.

Windows users need the Microsoft Visual C++ v14 x64 runtime; all three shipped binaries import
`VCRUNTIME140.dll`. The support policy records the qualification environment and each entrance's
actual surface rather than implying parity between the Node, CLI, and Python packages.

Single-file allocation accounting remains unavailable on every platform: `space_usage` reports a
structural zero for allocated bytes for that store shape, not a measurement. Logical byte counts
remain valid, and directory stores continue to use the platform allocation query (tracked in
[#153](https://github.com/turndb/turndb/issues/153)).
