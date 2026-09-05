# Support and compatibility policy

This document separates four promises that are easy to conflate: platform support, package API
compatibility, on-disk compatibility, and compiled capabilities. It describes the 0.x development
line. Publication of a crate or package remains a separate owner-approved action.

## Evidence tiers

| Surface | Current evidence | Status |
|---|---|---|
| Rust core, default and SQL-off | pinned stable Rust on GitHub's Linux x86-64 runner, and the same steps on its Linux arm64 runner (`ubuntu-22.04-arm`); debug tests, clippy, rustdoc, the corruption suite and the release-profile suite all run hosted on every push and pull request (the release-profile link exceeded the private free-tier runner class while the repository was private; it has run hosted since the repository went public) | qualified development platform |
| Rust crash model | deterministic simulation under two durability models — strict POSIX, and Windows built from documented operations only (no directory fsync; write-through renames; a crash on a rename admitting old, new, or neither; unlinks never durable) — both models run on every platform: nightly on Linux x86-64, and on every push on Windows x86-64 and Linux arm64 as required gates. The harness also fails every attempted sync of every publication sweep once, under both models, and requires the operation to report the failure and the store to converge (see "Sync failures" below) | qualified durability model on both platforms when those gates are green |
| Portable npm/WASI | `wasm32-wasip1` rebuilt from source; required CI matrix is Node 22, 24, and 26 | support candidate once the complete matrix is green and the package is published |
| Native Node | source-built addon plus Linux x86-64 glibc and Windows x86-64 MSVC candidates installed from the same tarballs on Node 22, 24, and 26 | release candidate after the matrices are green; tracked manifests remain private and registry status is owner-approved |
| Python SDK | PyO3 actor binding built and conformance-tested on CPython 3.12/Linux; release workflow builds manylinux x86-64 wheels for CPython 3.9–3.13 and installs each exact wheel. Ships **without** the columnar/Arrow lens, SQL, and cooperative cancellation: `turndb.capabilities()` reports `columnar: false`, `arrowIpc: false`, `sql: false`, `cancellation: {scan: false, lifecycle: false}` | Linux x86-64 release candidate; a consumer that needs SQL or cancellation chooses the Rust crate or native Node |
| Browser viewer | `wasm32-unknown-unknown` structured reader plus local-file and HTTP-range viewer tests in stock Chromium and Firefox | qualified read-only browser artifact when both browser jobs are green |
| Native Linux arm64 glibc | GitHub's hosted `ubuntu-22.04-arm` runner, real hardware: clippy, the debug and SQL-off suites, the corruption suite, the crash sweeps under both durability models, the release-profile suite, and the reference store byte-compared in both directions against the x86-64 Linux fixture — all required gates on every push and pull request. The `@turndb/cli` slice for this architecture is built on that hardware, installed from its packed tarball and driven, both in CI and in the release install matrix; through 0.1.8 it was cross-compiled and published having never been executed. `src/sys.rs` has no arm64-specific arm — this qualifies the Unix arm on a second instruction set. No native Node or Python package is built for this architecture; the portable package serves it, with the capability difference it states. | supported and qualified for the Rust crate and `@turndb/cli` when those gates are green; native Node and Python remain open |
| macOS x86-64 and arm64 | the `@turndb/cli` slices are built on `macos-15-intel` and `macos-15`, installed from their packed tarballs and driven, on every pull request that touches the CLI's inputs. The engine test suite, the crash sweeps and the cross-OS byte-compare do not run on macOS; the macOS arms of `src/sys.rs` (`renamex_np`, the Darwin `statvfs` widths) are exercised only through that drive. No native Node or Python package is built for macOS. | `@turndb/cli` qualified by its slice job; the Rust crate on macOS is built and driven, not suite-tested — no further claim |
| Other Unix systems and architectures | code paths exist but no CI or packaged artifacts prove them | unqualified; no support claim |
| Native Windows x86-64 | the platform floor in `src/sys.rs` (positioned I/O through `seek_read`/`seek_write`; the writer lock through `LockFileEx` on one byte past any read; durable flush through `FlushFileBuffers`; renames through `MoveFileExW` with `MOVEFILE_WRITE_THROUGH`); required jobs run clippy, debug and release suites, corruption and crash sweeps, and byte-identical cross-OS opening in both directions. **Content punch** uses `FSCTL_SET_ZERO_DATA` on a sparse file: the range is guaranteed to read as zeros with offsets unmoved. Physical space return remains best-effort at NTFS's 64 KiB sparse granularity; the in-place reclaim measurement is asserted on Linux only. **Single-file allocation accounting is unavailable on every platform**: `space_usage` reports allocation as absent rather than fabricating a structural zero ([#153](https://github.com/turndb/turndb/issues/153)). **Replacing an open file** takes the POSIX-semantics route, not write-through — the guarantee `src/sys.rs` declares as lagged, which is why reclaim runs its anchor protocol on this platform and one `rename(2)` everywhere else (FORMAT.md, "Free space"); the anchor protocol makes that step crash-safe. Transient names are governed by the exact inventory below: a writer open removes them beside a present store and counts them, refuses and names them beside an absent store, and `turndb inspect` lists them; none is consulted over a present store. Published-shaped native Node, CLI, and CPython 3.9–3.13 artifacts are built, digest-verified, installed from closed local registries, and exercised on `windows-latest`; registry publication remains an owner-approved release action. | supported and qualified on `windows-latest` x86-64 when the required jobs are green; Windows package publication is prepared for the next owner-approved release |

## Installed Windows entrances

The same installed-artifact run proves each row below; it does not infer parity between entrances.

| Entrance | Installed surface exercised on Windows x86-64 | Deliberately not claimed through that entrance |
|---|---|---|
| `@turndb/native` | native open/write/read, query, erase-by-refold, content-punch zeroing, `spaceUsage`, capability report, and opening the Linux fixture | CLI-only transient inventory and container `reclaim` command |
| `@turndb/cli` | `import`, `inspect`, `verify --deep`, transient-name listing/refusal, `reclaim`, and a store opened byte-exact on Linux | programmatic addon methods |
| Python `turndb` | exact wheels install and perform write/scan/close on CPython 3.9–3.13; the full installed capability and cross-OS contract runs on 3.12 | CLI `inspect`/`reclaim` and the Node addon's direct `contentPunch()` operation |

All three Windows binaries import `VCRUNTIME140.dll` and therefore require the Microsoft Visual C++
x64 Redistributable. See the [install guide](install.md) for commands and the qualification bounds.

## Transient names

Every transient protocol name that can remain beside a store after a crash, the
window that leaves it, and what happens to it. One recognizer produces this inventory
(`turndb::store::debris_report`, read-only; `debris_report_with_limits` honours directory-entry
admission), and the same recognizer decides for a writer open. Names are matched exactly, or by
the layout's own grammar — never by substring: a user's file that merely contains `.publish-` is
not touched. Every kind below is a variant of `DebrisKind`, a non-exhaustive enum.

| Kind | Exact name | Left by | Beside a **present** store, a writer open… | Beside an **absent** store, a writer open… |
|---|---|---|---|---|
| `PendingPublish` | `<final>.publish-<pid>-<n>` after a valid final name of the layout | a Windows process that died before the directory sync that installs a newly created protocol file's final name | removes it — the final name was never durable and the staging file is not input to WAL replay, manifest promotion, or an interrupted-installation procedure | refuses to create a fresh store over it, naming it |
| `CreationStaging` | `<store>.creating-<pid>-<n>` | container birth before its no-replace final-name installation (rename on Linux/macOS, hard-link creation on WASI) | removes it | a competing creator may install a fresh complete birth; after that installation the stale staging name is removed |
| `ReclaimStaging` | `<store>.reclaiming` | reclaim, either protocol, before the fresh container replaced the store artifact | removes it | refuses to create over it |
| `ReclaimAnchor` | `<store>.reclaimed` | reclaim's anchor protocol (Windows), from the anchor replacement until its cleanup landed — or a store carried here from that platform | removes it (the store is authority) | **reconstructs and reinstates the store from it** — not debris until a store exists again |
| `ReclaimCandidate` | `<store>.reclaim-candidate`, `<store>.reclaim-candidate.tmp` | reclaim's anchor protocol (Windows) or interrupted-anchor procedure (every platform), between the copy and the replace | removes them | refuses to create over them (the anchor procedure rebuilds them) |
| `MergeScratch` | `<store>-tmp/` | a crashed streaming merge | removes it | reports it |
| `ArtifactStaging` | `<artifact>.backing-up-<pid>-<n>`, `<artifact>.restoring-<pid>-<n>` | a backup or restore before artifact installation | removes it | reports it; exclusive creation refuses a colliding later operation without changing that name |

`<final>` in the `PendingPublish` row is not free-form: it must be a syntactically valid final
name of the current protocol, matched by its grammar, and a name whose `<final>` is anything
else is not `PendingPublish` and is never touched. The full list:

- Beside `<store>`: `<store>` itself, `<store>-wal`, and
  `<store>.reclaiming`, `<store>.reclaimed`, `<store>.reclaim-candidate`,
  `<store>.reclaim-candidate.tmp`, `<store>.creating-<pid>-<n>`, `<store>.backing-up-<pid>-<n>`, and
  `<store>.restoring-<pid>-<n>`.
- Beside merge work: `<store>-tmp/`.

In `.publish-<pid>-<n>`, `.creating-<pid>-<n>`, `.backing-up-<pid>-<n>`, and `.restoring-<pid>-<n>`, `<pid>` is a decimal
`u32` (the producing `std::process::id()`)
and `<n>` a decimal `u64` (that process's per-process counter): digits only, and each must parse
as its type.

The creation exception, stated plainly: a crash while a **brand-new** store is being created can
leave `<store>.creating-<pid>-<n>` beside a name that does not exist. Nothing acknowledged is in it
because the store name was never installed. A later creator stages another complete birth and races
only at the no-replace installation; after one complete store wins, writer open removes the stale
creation staging. `turndb inspect <store>` can list it before that happens.

A writer open counts what it removed in `StoreMetrics.debris_removed` — the one disposition a
returned store can truthfully report. A removal that fails is the open's error, with the path and
the underlying cause, and nothing is counted (a failed barrier is a failure). A reader never
mutates; `turndb inspect` prints this inventory before it opens anything, so debris beside an
absent store is still listed. `<store>-wal` is not debris: it is WAL-replay input. The deterministic
simulator asserts, for every crash state of every
sweep under both durability models, that after writer open and WAL replay a directory holds only current
names plus names this inventory reports.

## Namespace synchronization failures

A barrier that reports failure is a failure: no namespace-changing path reports success after a failed
file or directory sync. The rule is that **an operation reports what it made durable, nothing
more**, and what a failed directory sync means for each operation is:

| Operation | If its directory sync fails | What the caller sees | What to do |
|---|---|---|---|
| store creation | the store's name may not survive a crash | the call returns an error naming the directory and the store | run it again; writer open accepts the whole store if installed or creates it if absent |
| backup, restore artifact installation | the artifact's name may not survive a crash; the artifact is whole or absent at its final name, never torn | the call returns an error naming the directory and the artifact | run it again; the source is untouched |
| reclaim's replace (the rename protocol: every platform but Windows) | the fresh container is visible at the store's name without durability acknowledgement; a crash may reveal either the previous or replacement container, each whole | the call returns an error naming the directory and the store | nothing is lost; reopen, then run reclaim again if still needed |
| reclaim's cleanup (the anchor protocol: Windows) and interrupted-anchor cleanup (every platform) | the store at its name is complete and authoritative; the anchor may be back after a crash | the call returns an error naming the directory and the anchor | nothing is lost; the next writer open removes the stale anchor |
| close (removal of the `-wal` sidecar) | the store is complete; an empty sidecar may be back after a crash | `close` returns an error naming the directory and the log | nothing is lost; the next writer opens normally and a later clean close retries removal |

A failed *file* sync has always propagated; these were the directory syncs that did not. The
deterministic simulator proves each row: every attempted sync of every namespace-change sweep fails
once (the content-punch and free-space-punch sweeps are physical reclamation and are covered by their own
crash sweeps), the operation must report it, and both the real directory and every crash state of
the recording without that barrier must converge under both durability models.

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
Qualified package targets are Linux x86-64 glibc 2.17 or newer and Windows x86-64 MSVC. macOS,
musl, other architectures, and other native Unix systems remain unqualified even if a source build
happens to work. See the [native prebuild contract](native-prebuilds.md).

## Capabilities are runtime facts

`turndb::capabilities()` and every SDK's `capabilities()` describe the compiled implementation.
Consumers should branch on them rather than the host OS or package name. In particular:

- WASI reports embedder-enforced writer exclusion, no threads, and refold-only reclamation even on a
  Linux host — the portable `turndb` npm package gives up exactly three things a consumer choosing
  it must know: advisory locking, in-place content punch, and threads;
- native Linux reports OS-enforced writer exclusion, threads, and content-punch-or-refold reclamation;
- native Windows reports OS-enforced writer exclusion, threads, and content-punch-or-refold reclamation,
  where "content punch" guarantees zeroed bytes. Allocation accounting currently exposes a structural
  zero, not a per-member measurement, on every platform;
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
  format change. Release notes must name the break and say that discarded draft artifacts must be
  regenerated or exported before upgrading;
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
an unfrozen implementation seam; the one current `tdb_open` carries the complete admission profile.

## Errors and deprecation

The stable Rust `ErrorClass`/Node `TurnDbError.code` spellings are versioned API; rendered messages and
context chains are diagnostic text. Adding a new code or reclassifying a previously typed condition
is called out in a minor release before 1.0 and a major release after 1.0 when exhaustive consumers
could break. Existing codes are never reused for a different meaning.

Before 1.0, a compatibility adapter and a deprecation notice are preferred when they preserve
correctness, but a minor release may remove an API. After 1.0, a documented API is deprecated for at
least one minor release before removal in the next major. Deprecation never means silently weakening
durability, validation, erasure, or byte-exact reconstruction.

## On-disk draft

Package semver and the physical draft identity are independent. `FORMAT.md` is normative. The current
build requires draft epoch 1 across the current physical plane identities and has no preceding-format reader, converter, migration
API, or downgrade path. Until the format is explicitly frozen, an incompatible layout change rotates
the affected magic and replaces the current fixtures in place; superseded draft bytes fail closed.

This is deliberately not a compatibility promise. A release note must still call out a physical
reset so users know existing development data must be regenerated or exported before upgrading.

## Release status

Green source tests do not create a published artifact. Version 0.1.0 of the Rust crate, the
portable npm package, and `@turndb/native` was published on 2026-08-06. The native package's
tracked manifests remain `private` in source, so publication happens only through the owner-gated
provenance-capable release workflow; the portable package and Rust crate remain subject to the
publication checklist in `CONTRIBUTING.md`. Native production support begins only after the exact
tagged tarballs pass the release matrix and an owner approves their publication; this policy does
not manufacture registry or CI facts from source readiness.
