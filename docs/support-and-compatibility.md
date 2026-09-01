# Support and compatibility policy

This document separates four promises that are easy to conflate: platform support, package API
compatibility, on-disk compatibility, and compiled capabilities. It describes the 0.x development
line. Publication of a crate or package remains a separate owner-approved action.

## Evidence tiers

| Surface | Current evidence | Status |
|---|---|---|
| Rust core, default and SQL-off | pinned stable Rust on GitHub's Linux x86-64 runner; debug tests, clippy, rustdoc, the corruption suite and the release-profile suite all run hosted on every push and pull request (the release-profile link exceeded the private free-tier runner class while the repository was private; it has run hosted since the repository went public) | qualified development platform |
| Rust crash model | deterministic simulation under two durability models — strict POSIX, and Windows built from documented operations only (no directory fsync; write-through renames; a crash on a rename admitting old, new, or neither; unlinks never durable) — both models run on every platform: nightly on Linux x86-64, and on every push on Windows x86-64 as a required gate. The harness also fails every attempted sync of every publication sweep once, under both models, and requires the operation to report the failure and the store to converge (see "Sync failures" below) | qualified durability model on both platforms when those gates are green |
| Portable npm/WASI | `wasm32-wasip1` rebuilt from source; required CI matrix is Node 22, 24, and 26 | support candidate once the complete matrix is green and the package is published |
| Native Node | source-built addon plus Linux x86-64 glibc and Windows x86-64 MSVC candidates installed from the same tarballs on Node 22, 24, and 26 | release candidate after the matrices are green; tracked manifests remain private and registry status is owner-approved |
| Python SDK | PyO3 actor binding built and conformance-tested on CPython 3.12/Linux; release workflow builds manylinux x86-64 wheels for CPython 3.9–3.13 and installs each exact wheel. Ships **without** the columnar/Arrow lens, SQL, and cooperative cancellation: `turndb.capabilities()` reports `columnar: false`, `arrowIpc: false`, `sql: false`, `cancellation: {scan: false, lifecycle: false}` | Linux x86-64 release candidate; a consumer that needs SQL or cancellation chooses the Rust crate or native Node |
| Browser viewer | `wasm32-unknown-unknown` structured reader plus local-file and HTTP-range viewer tests in stock Chromium and Firefox | qualified read-only browser artifact when both browser jobs are green |
| Other Unix systems and architectures | code paths exist but no CI or packaged artifacts prove them | unqualified; no support claim |
| Native Windows x86-64 | the platform floor in `src/sys.rs` (positioned I/O through `seek_read`/`seek_write`; the writer lock through `LockFileEx` on one byte past any read; durable flush through `FlushFileBuffers`; renames through `MoveFileExW` with `MOVEFILE_WRITE_THROUGH`); required jobs run clippy, debug and release suites, corruption and crash sweeps, and byte-identical cross-OS opening in both directions. **Punch** is `FSCTL_SET_ZERO_DATA` on a sparse file: the range is guaranteed to read as zeros with offsets unmoved. Physical space return remains best-effort at NTFS's 64 KiB sparse granularity. It is reported, not promised; the in-place reclaim measurement is asserted on Linux only. **Single-file allocation accounting is unavailable on every platform**: `space_usage` currently reports a structural zero rather than an allocation measurement ([#153](https://github.com/turndb/turndb/issues/153)); directory stores use the platform allocation query. **Replacing an open file** takes the POSIX-semantics route, not write-through; reclaim's anchor protocol (FORMAT.md, "Free space") makes that step crash-safe. That anchor is a byte copy here rather than a hard link, because a linked name is not durable on a platform whose namespace publishes only through write-through renames — so **`turndb reclaim` writes the compacted container twice on Windows and once on a POSIX filesystem that supports hard links**. It is a cost, not a difference in what the operation guarantees, and it is paid only by that command: nothing runs reclaim on a write, a checkpoint or a close. Transient names are governed by the exact inventory below: a writer open removes them beside a present store and counts them, refuses and names them beside an absent store, and `turndb inspect` lists them; none is consulted over a present store. Published-shaped native Node, CLI, and CPython 3.9–3.13 artifacts are built, digest-verified, installed from closed local registries, and exercised on `windows-latest`; registry publication remains an owner-approved release action. | supported and qualified on `windows-latest` x86-64 when the required jobs are green; Windows package publication is prepared for the next owner-approved release |

## Installed Windows entrances

The same installed-artifact run proves each row below; it does not infer parity between entrances.

| Entrance | Installed surface exercised on Windows x86-64 | Deliberately not claimed through that entrance |
|---|---|---|
| `@turndb/native` | native open/write/read, query, erase-by-refold, punch zeroing, `spaceUsage`, capability report, and opening the Linux fixture | CLI-only transient inventory and container `reclaim` command |
| `@turndb/cli` | `import`, `inspect`, `verify --deep`, transient-name listing/refusal, `reclaim`, and a store opened byte-exact on Linux | programmatic addon methods |
| Python `turndb` | exact wheels install and perform write/scan/close on CPython 3.9–3.13; the full installed capability and cross-OS contract runs on 3.12 | CLI `inspect`/`reclaim` and the Node addon's direct `punch()` operation |

All three Windows binaries import `VCRUNTIME140.dll` and therefore require the Microsoft Visual C++
x64 Redistributable. See the [install guide](install.md) for commands and the qualification bounds.

## Transient names

Every name the publication and reclaim protocols can leave beside a store after a crash, the
window that leaves it, and what happens to it. One recognizer produces this inventory
(`turndb::store::debris_report`, read-only; `debris_report_with_limits` honours directory-entry
admission), and the same recognizer decides for a writer open. Names are matched exactly, or by
the layout's own grammar — never by substring: a user's file that merely contains `.publish-` is
not touched. Every kind below is a variant of `DebrisKind`, a non-exhaustive enum.

| Kind | Exact name | Left by | Beside a **present** store, a writer open… | Beside an **absent** store, a writer open… |
|---|---|---|---|---|
| `PendingPublish` | `<final>.publish-<pid>-<n>` after a valid final name of the layout | a Windows process that died before the directory sync that publishes a new name | removes it — it was never durable, never recovery material | refuses to create a fresh store over it, naming it |
| `ReclaimStaging` | `<store>.reclaiming` | reclaim, before the anchor was published | removes it | refuses to create over it |
| `ReclaimAnchor` | `<store>.reclaimed` | reclaim, from the anchor's publish until its cleanup landed | removes it (the store is authority) | **recovers the store from it** — not debris until a store exists again |
| `ReclaimCandidate` | `<store>.reclaim-candidate`, `<store>.reclaim-candidate.tmp` | reclaim or anchor recovery, between the copy and the replace | removes them | refuses to create over them (recovery rebuilds them from the anchor) |
| `MergeScratch` | `<store>-tmp/` | a crashed streaming merge | removes it | reports it |
| `ArtifactStaging` | `<artifact>.sealing`, `<artifact>.restoring`, `<artifact>.converting` | a backup / seal, restore or conversion whose destination was `<artifact>`, before it published | removes it | reports it; the operation's retry removes its own stage |
| `ManifestStaging` | `MANIFEST.tmp` (directory layout) | a commit before its rename | removes it | — |
| `ExcessRetainedManifest` | `MANIFEST.<commit>` older than the retention window, with a live `MANIFEST` (directory layout) | a commit's prune whose unlink a crash undid | removes it | — |
| `SegmentSidecarStaging` | `seg-<n>.dir.tmp` in `fold/` or `fold-<generation>/` | a sidecar before its rename | removes it | — |
| `PartBuilderSpool` | `<part>.s<n>.tmp` (directory layout) | the part builder mid-build | removes it | — |
| `LegacyHotDirectory` | `<store>-hot/` | a **0.1.x** working session (CHANGELOG 0.1.0, 0.1.2) abandoned before an upgrade; it may hold acknowledged writes only that release can settle | **refuses and names it** — never removes it | refuses to create, names it — never removes it. Open the store with the release that wrote the directory (which adopts and settles it), or move the directory aside deliberately |

`<final>` in the `PendingPublish` row is not free-form: it must be a syntactically valid final
name of the layout, matched by the layout's own grammar, and a name whose `<final>` is anything
else is not `PendingPublish` and is never touched. The full list:

- **Single-file layout**, beside `<store>`: `<store>` itself, `<store>-wal`, and
  `<store>.reclaiming`, `<store>.reclaimed`, `<store>.reclaim-candidate`,
  `<store>.reclaim-candidate.tmp`, `<store>.sealing`, `<store>.restoring`, `<store>.converting`.
- **Directory layout, root**: `MANIFEST`, `MANIFEST.tmp`, `MANIFEST.<commit>` (`<commit>` a
  decimal `u64`), `WAL`, `WRITER.lock`, `part-<seq>.part` and a merged `part-<lo>-<hi>.part`
  (`u64` sequence numbers), and the builder spool `<part stem>.part.s<n>.tmp` (`<n>` a decimal
  `u64`).
- **Fold directories** (`fold/` and every `fold-<generation>/`): `seg-<n>.fold`, `seg-<n>.dir`,
  `seg-<n>.dir.tmp` (`<n>` a decimal `u32`), and `zdict-<h>.zd` (`<h>` exactly 64 lowercase hex
  digits — the engine writes lowercase, and nothing else matches).

In `.publish-<pid>-<n>` itself, `<pid>` is a decimal `u32` (the producing `std::process::id()`)
and `<n>` a decimal `u64` (that process's per-process counter): digits only, and each must parse
as its type.

The commonest refusal, stated plainly: a crash (or a failed first sync) while a **brand-new**
store is being created on Windows leaves `<store>.publish-<pid>-<n>` beside a name that does not
exist. Nothing acknowledged is in it — the store was never published — but nothing proves that
to the engine either, so the next writer open refuses to create a store there and names the file;
the user's action is to remove the named file (or move it aside) and open again. `turndb inspect
<store>` lists it first.

A writer open counts what it removed in `StoreMetrics.debris_removed` — the one disposition a
returned store can truthfully report. A removal that fails is the open's error, with the path and
the underlying cause, and nothing is counted (a failed barrier is a failure). A reader never
mutates; `turndb inspect` prints this inventory before it opens anything, so debris beside an
absent store, or beside a directory-layout store, is still listed. Not debris, and excluded from
the inventory: `<store>-wal` and the directory layout's `WAL` (recovery input, replayed and
settled) and `WRITER.lock`. The deterministic simulator asserts, for every crash state of every
sweep under both durability models, that after open-and-recover a directory holds only live
names plus names this inventory reports.

## Sync failures

A barrier that reports failure is a failure: no publication path reports success after a failed
file or directory sync. The rule is that **an operation reports what it made durable, nothing
more**, and what a failed directory sync means for each publication is:

| Publication | If its directory sync fails | What the caller sees | What to do |
|---|---|---|---|
| store creation, and the rebirth of an interrupted creation | the store's name may not survive a crash | the call returns an error naming the directory and the store | run it again; a writer open creates or finishes the store |
| backup / seal, restore, conversion | the artifact's name may not survive a crash; the artifact is whole or absent at its final name, never torn | the call returns an error naming the directory and the artifact | run it again; the source is untouched |
| reclaim's and anchor recovery's cleanup | the store at its name is complete and authoritative; the anchor may be back after a crash | the call returns an error naming the directory and the anchor | nothing is lost; the next writer open removes the stale anchor |
| close (removal of the `-wal` sidecar) | the store is complete; an empty sidecar may be back after a crash | `close` returns an error naming the directory and the log | nothing is lost; the next open settles the sidecar |

A failed *file* sync has always propagated; these were the directory syncs that did not. The
deterministic simulator proves each row: every attempted sync of every publication sweep fails
once (the punch sweeps are physical reclamation, not publication, and are covered by their own
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
  it must know: advisory locking, in-place punch, and threads;
- native Linux reports OS-enforced writer exclusion, threads, and punch-or-refold reclamation;
- native Windows reports OS-enforced writer exclusion, threads, and punch-or-refold reclamation,
  where "punch" guarantees zeroed bytes. Allocation accounting is reported for directory stores;
  single-file stores currently expose a structural zero, not a measurement, on every platform;
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
