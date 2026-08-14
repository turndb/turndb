# Support and compatibility policy

This document separates four promises that are easy to conflate: platform support, package API
compatibility, on-disk compatibility, and compiled capabilities. It describes the 0.x development
line. Publication of a crate or package remains a separate owner-approved action.

## Evidence tiers

| Surface | Current evidence | Status |
|---|---|---|
| Rust core, default and SQL-off | pinned stable Rust on GitHub's Linux x86-64 runner; debug tests, clippy, rustdoc, and the corruption suite run hosted. The release-profile suite is verified on the local qualified platform; its hosted job runs on the larger public-repository runner class (the private free-tier runner could not link it — SIGBUS/disk, documented in the CI workflow) | qualified development platform; release-profile evidence is local until the hosted job reports green |
| Rust crash model | nightly deterministic simulation on Linux x86-64 | qualified durability model when the scheduled gate is green |
| Portable npm/WASI | `wasm32-wasip1` rebuilt from source; required CI matrix is Node 22, 24, and 26 | support candidate once the complete matrix is green and the package is published |
| Native Node | source-built addon plus one cross-built Linux x86-64 glibc candidate installed from the same tarballs on Node 22, 24, and 26 | release candidate after both matrices are green; tracked manifests remain private and registry status is owner-approved |
| Python SDK | PyO3 actor binding built and conformance-tested on CPython 3.12/Linux; release workflow builds manylinux wheels for CPython 3.9–3.13 and installs each exact wheel | Linux release candidate after CI and the owner-approved PyPI workflow are green |
| Browser viewer | `wasm32-unknown-unknown` structured reader plus local-file and HTTP-range viewer tests in stock Chromium and Firefox | qualified read-only browser artifact when both browser jobs are green |
| Other Unix systems and architectures | code paths exist but no CI or packaged artifacts prove them | unqualified; no support claim |
| Native Windows | the native core requires Unix positioned I/O and writer locking | unsupported |

Node ranges are deliberately closed at the next untested major: both manifests declare
`>=22 <27`. Node 22 and 24 are maintained LTS lines and Node 26 is the Current line as of
2026-08-03; the repository follows the [official Node release status](https://nodejs.org/en/about/previous-releases),
not historical popularity. A repository test keeps both manifests and CI's exact majors in sync.
Adding a newly released major requires a green matrix before widening the range. EOL majors are not
retained merely because N-API can load on them.

The 2026-08-03 policy change was locally exercised on Node 24. Node 22, 24, and 26 are exercised by
the required CI matrix on `main` (`.github/workflows/ci.yml`). A release is blocked until every
declared major passes.

N-API 6 decouples the addon from a particular V8 ABI. It does not prove OS/architecture prebuild
availability or runtime correctness on every later Node release. Only the matrix above is evidence.
The first package target is exactly Linux x86-64 glibc 2.17 or newer. macOS, musl, other
architectures, and other native Unix systems remain unqualified even if a source build happens to
work. See the [native prebuild contract](native-prebuilds.md).

## Capabilities are runtime facts

`turndb::capabilities()` and every SDK's `capabilities()` describe the compiled implementation.
Consumers should branch on them rather than the host OS or package name. In particular:

- WASI reports embedder-enforced writer exclusion, no threads, and refold-only reclamation even on a
  Linux host;
- native Linux reports OS-enforced writer exclusion, threads, and punch-or-refold reclamation;
- Python reports the mechanisms in its native, actor-owned build;
- the browser reports a read-only, single-threaded structured-query profile and no reclamation;
- Rust features decide whether the columnar lens and SQL exist;
- the native package refuses to load when its addon is absent and never silently falls back to the
  reduced WASM profile.

Capability objects are extensible: consumers must ignore unknown keys. Existing key names, types,
and meanings follow the API version rules below. A capability changing from true to false for the
same documented build target is breaking; a different target reporting a different value is the
purpose of the profile.

## Package and API versions

The Rust crate, portable npm package, native Node package, and Python distribution move in lockstep,
but their versions describe API artifacts, not the part-format byte. Until 1.0:

- a patch release fixes defects and may add diagnostic context, but does not intentionally remove or
  rename documented APIs, change a stable error code for the same typed condition, raise a supported
  runtime floor, or advance the writer format;
- a minor release may make a documented breaking API, feature-default, runtime-support, or writer-
  format change. Release notes must name the break and its migration path;
- additions normally use a minor release. Optional input fields and new methods remain preferable to
  changing existing meanings;
- unqualified examples, benchmarks, `bindings/node/qualification/`, and private test helpers are not
  package APIs.

At 1.0, incompatible documented API changes require a major version. Rust's public structs are
exhaustive unless explicitly marked otherwise, so adding a public field can itself be a breaking Rust
change. Removing a Cargo feature or changing what a default feature enables is also an API change.
The SQL-off storage profile remains tested independently.

For Node, the `.d.ts` declarations and runtime property names/types form one contract. Numeric values
declared as `bigint` never narrow through JavaScript `number`. Capability/result objects may gain
fields in a compatible release, so consumers should select fields rather than require exact object
equality. Existing fields are not silently repurposed.

The portable package's JavaScript wrapper is the public package API. Its low-level WASI exports are
kept source-compatible for direct embedders where practical: `tdb_open` uses compiled admission
defaults, `tdb_open_v2` adds frame-byte policy, and `tdb_open_v3` adds persistent object-count policy.
New optional dimensions use a new export instead of changing an existing function signature.

## Errors and deprecation

The stable Rust `ErrorClass`/Node `TurnDbError.code` spellings are versioned API; rendered messages and
context chains are diagnostic text. Adding a new code or reclassifying a previously typed condition
is called out in a minor release before 1.0 and a major release after 1.0 when exhaustive consumers
could break. Existing codes are never reused for a different meaning.

Before 1.0, a compatibility adapter and a deprecation notice are preferred when they preserve
correctness, but a minor release may remove an API. After 1.0, a documented API is deprecated for at
least one minor release before removal in the next major. Deprecation never means silently weakening
durability, validation, erasure, or byte-exact reconstruction.

## On-disk versions

Package semver and format versions are independent. `FORMAT.md` is normative and the current writer
emits part version 2. The compatibility promise is operational:

- a build reads the immediately preceding format revision and can migrate it forward;
- newer readers refuse unknown future layouts unless an explicitly optional section can be ignored;
- older readers may refuse a newer store; downgrade writing is not promised;
- migration is explicit, preflighted, part-sized, restartable, and does not invent missing content
  identities;
- format fixtures are immutable evidence. The legacy reference pack carries real version-1 bytes
  and is migrated through the public Node API; release fixtures remain after newer fixtures
  are added.

Advancing the writer format is a minor-or-major package change, never a patch. A release must add the
new previous-version fixture before it can replace the writer default. Erasure may deliberately purge
retained history; no compatibility promise resurrects erased content.

## Release status

Green source tests do not create a published artifact. Version 0.1.0 of the Rust crate, the
portable npm package, and `@turndb/native` was published on 2026-08-06. The native package's
tracked manifests remain `private` in source, so publication happens only through the owner-gated
provenance-capable release workflow; the portable package and Rust crate remain subject to the
publication checklist in `CONTRIBUTING.md`. Native production support begins only after the exact
tagged tarballs pass the release matrix and an owner approves their publication; this policy does
not manufacture registry or CI facts from source readiness.
