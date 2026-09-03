---
default: patch
---

# Python wheels for Linux arm64 and both macOS architectures

Through 0.1.8 the `turndb` wheel existed for manylinux x86-64 and Windows x86-64; every other host
built from the sdist. The release workflow now also builds manylinux2014 aarch64 wheels, in the
container on arm64 hardware rather than under emulation, and macOS x86-64 and Apple-silicon wheels,
one per interpreter on the hardware it runs on, for CPython 3.9–3.13 — and its install matrix
installs and exercises every exact wheel on the hardware it claims before the protected PyPI
publish. On every push, the binding is built, conformance-tested and wheel-installed on Linux arm64
and both macOS runners as required gates. The wheels publish with the next owner-approved release.
