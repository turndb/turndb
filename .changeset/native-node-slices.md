---
default: patch
---

# The native Node addon ships for Linux arm64 and both macOS architectures

`@turndb/native` selects a platform package by npm's `os`, `cpu` and `libc` metadata, and through
0.1.8 only Linux x86-64 glibc and Windows x86-64 existed; every other host received the load error
that names its platform and refuses to substitute the reduced WASI build. Three slices are added:
`@turndb/native-linux-arm64-gnu`, cross-built through the same napi-rs toolchain as the x86-64 slice
so both Linux addons carry the GLIBC 2.17 floor, and installed and exercised on arm64 hardware on
Node 22, 24, and 26; and `@turndb/native-darwin-x64` and `@turndb/native-darwin-arm64`, built on
`macos-15-intel` and `macos-15` and installed and exercised there on the same Node majors. The
native release workflow builds all five, installs each on its own hardware before the protected
publish step, and refuses to publish unless all five are present. The packages publish with the next
owner-approved release; the support policy states what is prepared and what remains open (Python
wheels for these hosts).
