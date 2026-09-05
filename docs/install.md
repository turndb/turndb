# Install TurnDB

TurnDB ships three Windows x86-64 entrances. Choose the entrance whose surface you need; a package
existing for an operating system does not imply that every engine capability is exposed through it.

## Windows prerequisite

Install Microsoft's latest supported
[Visual C++ v14 Redistributable for x64](https://aka.ms/vc14/vc_redist.x64.exe) before using any of
the three packages. The shipped CLI, Node addon, and Python extension all import
`VCRUNTIME140.dll`. This requirement comes from their PE import tables, not from a successful run on
the hosted CI image: that image already had the runtime installed.

The prebuilt packages do not require a Rust or C/C++ compiler. The qualification jobs installed and
ran them with `cl` absent from `PATH`.

No Microsoft Defender exclusion is required or recommended. CI records the hosted image's Defender
posture as an environment limitation; it is not evidence about a consumer machine, so TurnDB makes
no antivirus-performance promise.

The Windows qualification image had long-path support enabled (`LongPathsEnabled=1`). TurnDB uses
extended-length (`\\?\`) paths for long filesystem operations, but the hosted result does not prove
that an otherwise unconfigured Windows host can ignore its own path policy. Keep Windows long-path
support enabled when stores may live under deep paths.

## Native Node addon

```powershell
npm install @turndb/native
```

The package selects `@turndb/native-win32-x64-msvc` through npm's `os` and `cpu` metadata. It
supports Node 22, 24, and 26 and exposes the native programmatic store, query, erasure, content-punch/refold,
and capability surfaces. A missing native slice is an error; the selector never silently substitutes
the reduced WASI implementation.

## Command line

```powershell
npm install --global @turndb/cli
turndb help
```

The selector installs `@turndb/cli-win32-x64-msvc`. This is the entrance for `import`, `inspect`,
`verify`, `query`, and maintenance commands such as `reclaim`. In particular, `inspect` is the
packaged surface that inventories transient names beside a store.

## Python

```powershell
py -m pip install turndb
```

Windows `win_amd64` wheels are built for CPython 3.9 through 3.13. Each exact wheel is installed
from a closed local index into a fresh environment and performs a real write/scan/close smoke test;
the full installed-capability and cross-OS contract is exercised on CPython 3.12. The wheel has no
required runtime dependencies. The optional `otel` extra is separate.

The Python entrance exposes its actor-owned store and structured-query API. It does not expose the
CLI's `inspect` or container `reclaim` commands, and it does not expose the Node addon's direct
`contentPunch()` operation.

## Space accounting

For a single-file store, `space_usage` reports allocated bytes as absent on every platform; it does
not fabricate a structural zero. Logical byte counts and filesystem availability remain valid. The
compiled `allocatedSpaceUsage` capability is therefore false. Tracking:
[#153](https://github.com/turndb/turndb/issues/153).
