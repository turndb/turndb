# Changelog

All notable changes to this project are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

TurnDB does not follow Semantic Versioning yet, because it is pre-1.0
and nothing here is a compatibility promise. See **Stability** below
before relying on anything in this file.

> Historical entries name physical layouts and operations that no current reader or API retains.
> They record what a release did; they are never implementation guidance. `FORMAT.md` is the only
> current physical contract and accepts exactly its one unfrozen draft identity.

## [0.1.0] — 2026-08-06

First release, published as three artifacts: the `turndb` crate on
crates.io, the portable `turndb` npm package, and `@turndb/native`.

### Stability

**Pre-1.0. Physical format draft, not frozen.** [`FORMAT.md`](FORMAT.md) is
normative: where it and the code disagree, one of them is a bug.

Nothing below is a stability commitment. Specifically:

- **The on-disk format is not frozen.** An incompatible physical change replaces the draft
  identity and every reader for the preceding draft rather than creating a compatibility range — see
  [`FORMAT.md`](FORMAT.md#draft-identity-rule).
- **The API is not frozen.** There is no prior release, so there is
  nothing to be compatible with and no deprecation policy yet.
- **There is no supported-version window.** Security fixes land on
  `main`; see [`SECURITY.md`](SECURITY.md).

**One limit a reader should not miss, because it is a correctness
property rather than a feature gap:** the single-writer invariant is
OS-enforced on Unix via `flock` and **is not enforced under WASI**,
where it is the embedder's obligation. The measured consequence, and why
a clean `verify()` does not settle it, is in
[`FORMAT.md`](FORMAT.md#store-shape).

### Added

First release, so everything is new.

- **Durability** — WAL with an explicit ACK point, all-or-nothing batch
  replay, a single commit point (the manifest) with a checksummed commit
  log, snapshots, and explicit recovery. See
  [`docs/manifest-promotion.md`](docs/manifest-promotion.md).
- **Query** — bounded structured paging with Rust-owned cursors, an
  optional DataFusion lens, and a read-only SQL-to-Arrow stream. Named
  content is independently projectable and metadata queries open zero
  fold blocks. See
  [`docs/projected-structured-scan.md`](docs/projected-structured-scan.md),
  [`docs/scan-explanation.md`](docs/scan-explanation.md),
  [`docs/sql-arrow-stream.md`](docs/sql-arrow-stream.md).
- **Compaction** — total merge at eight parts, plus work units bounded
  by exact input-part, row and byte limits applied simultaneously.
  Merges provably touch zero content bytes. See
  [`docs/bounded-part-merge.md`](docs/bounded-part-merge.md).
- **Deletion** — tombstone → settle → re-fold removes content *and*
  metadata; `punch` reclaims dead blocks in place without moving an
  offset. See [`docs/content-liveness.md`](docs/content-liveness.md),
  [`docs/maintenance-space.md`](docs/maintenance-space.md).
- **Integrity** — per-piece BLAKE3 on every read, per-section checksums,
  footer and TOC chains, manifest-pinned parts, and a `scrub` that walks
  every frame. Checksums are integrity evidence, not authentication.
- **Shipping** — `pack` puts a whole store in one file that reads, and
  answers SQL, identically. Verified before publication and refuses to
  replace an existing path. See
  [`docs/backup-restore.md`](docs/backup-restore.md).
- **Bindings** — a native Node addon (`@turndb/native`) and a portable
  `wasm32-wasip1` build (`turndb` on npm), both exposing structured
  scan, atomic durable writes, typed errors, cooperative deadlines,
  integrity verification, and per-operation retention outcomes. See
  [`docs/embedding-contract.md`](docs/embedding-contract.md),
  [`docs/cross-runtime-compatibility.md`](docs/cross-runtime-compatibility.md).
- **CLI** — `turndb <verb>`: reading (`inspect`, `ids`, `get`, `verify`,
  `query`), operating (`compact`, `refold`, `punch`, `recover`,
  `snapshots`, `erase`), ingesting (`import`), shipping (`pack`,
  `unpack`). `turndb help` prints the authoritative verb set; where it
  and this list disagree, it is right.

### Known limitations

- **Single-writer is not enforced under WASI** — see Stability above and
  [`FORMAT.md`](FORMAT.md#store-shape).
- **No encryption.** The format reserves a flag bit and refuses it.
- **No parity or erasure coding.** Corruption is detected at every
  level; repair is the storage layer's job.
- **No daemon, network, cluster or consensus.** Scale-out is more
  stores, not a bigger one.
- **Threat model, completed hardening and residual risks** are in
  [`docs/security-review.md`](docs/security-review.md), which is scoped
  as of 2026-08-03 and is not an independent third-party audit.

[0.1.0]: https://github.com/turndb/turndb/releases/tag/v0.1.0
## 0.1.8 (2026-09-01)

### Fixes

#### Ships the npm packages absent from 0.1.7

TurnDB 0.1.7 was published to crates.io and PyPI, but no TurnDB npm package was published at
0.1.7. The npm publication could not complete from that tag because the release path executes its
packaging verification scripts from the tagged tree, while the required release-tooling repairs
landed after the tag was created.

TurnDB 0.1.8 contains the same product changes described in the 0.1.7 release notes, together with
the repaired release tooling needed to publish the npm packages coherently. Rust crate and Python
package users already on 0.1.7 do not need to change versions. npm users should install 0.1.8
directly.

## 0.1.7 (2026-08-31)

### Added

#### Windows x64 packages install the qualified binaries

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

#### The packaged CLI covers five targets and reports its version

`@turndb/cli` now packages Linux x86-64 and arm64 GNU, macOS x86-64 and arm64, and Windows x86-64
MSVC binaries. `turndb --version`, `turndb version`, and `turndb -V` report the crate version compiled
into the selected platform binary.

#### Crash-leftover inventory is public

Writer open now recognises every transient name the publication and reclaim protocols can leave
after a crash. Beside a present store it removes safe-to-discard leftovers and reports the count in
`StoreMetrics.debris_removed`; beside an absent store it refuses to create over pending publication
or reclaim material and names what must be inspected. A legacy `<store>-hot` working directory is
always reported and refused, never removed. The public, non-exhaustive `DebrisReport`, `DebrisEntry`,
and `DebrisKind` types — returned by `debris_report` and `debris_report_with_limits` — expose the
same inventory without mutating it.

The added public field means a downstream Rust caller that constructs `StoreMetrics` with an
exhaustive struct literal must add `debris_removed`; callers that obtain metrics from the store and
read their fields are unaffected. This source-compatibility break ships in 0.1.7 without a breaking
version signal; compatibility hardening is deferred to a future breaking release.

### Fixes

#### CLI writer verbs close sessions cleanly

Writer verbs now close the store they open before returning. A successful `compact`, `refold`,
`punch`, `erase`, or `seal` therefore removes its empty WAL sidecar and leaves the documented
single-file store shape instead of a stray zero-byte `<store>-wal`.

#### Crash leftovers are inspected with remediation

`turndb inspect` scans that inventory before opening the target, so it can report leftovers beside
an absent store and gives a conversion hint for a retired directory store. The support and format
documents list every recognised name, when it can appear, and the corresponding recovery action.
For a refused pending-publication file beside an absent store, inspect it and remove or move aside
the named file before retrying. For `<store>-hot`, use the 0.1.x release that wrote it to settle its
acknowledged writes, or move the directory aside deliberately; a current writer will not delete it.

#### Failed durability barriers are reported

Every publication path also propagates a failed file or directory sync. Operations that previously
could report success after the durability barrier failed now return an error describing the name
whose persistence is uncertain. This is a patch-level data-integrity correction: the changed
outcomes were false success or creation over ambiguous recovery material, not valid successful
workflows.

## 0.1.6 (2026-08-24)

### Fixes

#### Lock-free opens no longer race a commit into a false truncation refusal

A lock-free reader open measures the container's length and then reads the superblock slots. A
writer committing in that gap — bytes appended past the old tail, fsync, slot flip — could leave
the newest slot's tail beyond the stale measurement, and the open refused a healthy, fully
committed store as truncated.

Both open paths now re-measure once when the committed tail exceeds the first measurement.
Containers only grow — reclamation punches holes in place — so any slot the open managed to read
was committed by the time the second measurement is taken, and one re-measure is decisive. A tail
still beyond the second answer is genuine truncation and refuses exactly as before.

## 0.1.5 (2026-08-18)

### Fixes

#### Cold opens no longer scan active content

Every single-file commit now publishes the active fold segment's advisory block directory in the
same superblock state as the segment tail. A ranged reader can therefore open a current store from
container, manifest, segment-index, and part metadata without fetching fold block payloads.

The executable contract measures an uncached positioned source at exactly
`4 + 2*segments + dictionaries + 2*parts` reads. Missing, damaged, stale, or over-budget advisory
indexes remain readable through the existing checked frame scan; the optimization never becomes a
new correctness requirement or a reason to refuse an older valid store.

The browser measurement now reports cold-open and point-query HTTP fetches separately instead of
letting one combined number stand in for both behaviors.

## 0.1.4 (2026-08-14)

### Features

#### One layout remains

The directory store's write path is gone from the engine: the checkpoint bridge
(`ContainerStore`), the pack writer and restorer, `checkpoint_into_container`, and every public
directory-form surface (`Store::open` and its reader family, `recover_manifest`, `verify_chain`,
`retained_commits`) are deleted. The single-file store is the store; `convert` is the retired
layout's one remaining door, proven against a checked-in fixture written by 0.1.3 itself —
unsettled WAL included, because converting must replay acknowledged writes, and now it is proven
to at every crash point.

Retiring the layout's tests forced its replacements to earn their coverage, and they found real
gaps, all fixed: conversion now builds in staging and publishes with a no-replace rename, so a
crash mid-convert is recovered by re-running it; file recovery's promotion flip now also
truncates the fold to the promoted tail, so a rolled-back store reopens instead of refusing its
own manifest; restore's copy goes through the recorded write seam and is fsynced before
publication; `reclaim` takes the writer flock and refuses an unsettled WAL sidecar instead of
checking for a working directory no writer creates any more; and opening a missing container
refuses typed (`NOT_FOUND`) without ever creating a transient file at the queried name.

The CLI's `erase` verb, missed in the file-first migration, now opens the store file and reads
its audit hashes from the manifest member.

#### Phase 3 gets one executable contract

TurnDB now carries versioned, machine-readable capability and structured-query contracts, an
independent semantic corpus, and a checked physical `.turndb` fixture. The Rust oracle replays the
write timeline and compares point reads, writer overlays, pinned snapshots, structured pages,
cursor refusals, exact content, work statistics, and the final container bytes. The native Node
package runs the same corpus through N-API and opens the same read-only fixture, so its binding tests
can no longer pass on a parallel hand-written notion of the API.

Python now has the same Tier-1 surface through a bounded single-owner PyO3 actor, including sealing
and maintenance. Node and Python share one normative OpenTelemetry span mapping and cadence policy.
Both exporters produce the same record ids, typed metadata, bulk gen_ai content, stable events, and
links for the language-neutral vector. Dependency-free client-call wrappers create those canonical
spans around synchronous and asynchronous provider SDK calls without changing their results.

The browser build opens the normal container through an arbitrary `ReadAt` source backed by bytes,
Blob/File, or strict HTTP Range. Structured predicates prune from part dictionaries and zone maps
before projection, and every binding reports the avoided rows. A checked, self-contained viewer
contains its wasm and JavaScript, replays the applicable shared corpus, and is exercised in Chromium
and Firefox through both local-file and static-host URL paths. SQL remains explicitly absent after a
documented DataFusion wasm build evaluation.

`capabilities()` adds the contract-v1 profile and operation set without removing its detailed native
facts. The shared corpus exposed a real loss at the JavaScript boundary: V8 canonicalized non-default
NaN payloads before Rust received them. Float attributes now accept an exact sixteen-digit lowercase
`floatBits` lane and emit it for NaNs; ordinary `floatValue` remains unchanged, and contradictory or
malformed dual representations refuse as `INVALID_ARGUMENT`.

Release automation now builds and smoke-tests Python wheels and a source distribution for PyPI,
and attaches the byte-rebuilt browser viewer to the matching GitHub release.

#### The CLI speaks one layout, and old layouts keep one door

Every verb now takes a `.turndb` file. `import` creates the file if absent and leaves one file
behind; `seal` ships the committed snapshot as one sealed file (what `backup` produces, named for
what it does); `punch` performs both halves of in-place reclamation — dead content blocks under
the manifest's declaration, and free extents older than the retention window — and reports each;
`verify` walks member checksums, the retained chain, and every live part pin against the extents
the file actually holds.

Four verbs are gone. `pack` and `unpack`, because the pack is retired as an artifact — a sealed
container is the single-file archival form, and extraction has no successor (`cp` copies a
store). `checkpoint` and `write`, because ingesting into a file **is** the product now, and
`import` does it.

The retired layouts — store directories, sealed packs — keep exactly one door:

```sh
turndb convert mystore mystore.turndb      # a directory store, WAL settled, manifest verbatim
turndb convert snap.pack snap.turndb       # a pack, copied straight from its extents
```

Reading a retired layout with any other verb refuses and prints the convert line to run. The
library's directory-store constructors, the checkpoint bridge, and the pack write path remain
compiled for the transition, carry retirement notices, and leave with the bindings' rebase onto
the single-file store.

#### The file is the live database

A `.turndb` file was, until now, a checkpoint target: a writer materialized working state into a
`-hot` directory beside it, ran the directory engine there, and folded the result back in when it
closed. That bridge proved the format. This release replaces it with the model itself, taken from
SQLite and applied literally: **the file holds the data plane at every moment a writer runs**, and
what sits beside it while hot is flat and few — a `-wal` sidecar for acknowledged-but-unflushed
records, and writer exclusion by `flock` on the file itself, which the kernel releases when the
process dies. A cleanly closed store is exactly one file.

```rust
let mut s = Store::open_file("traces.turndb".as_ref(), FoldCfg::default())?;
s.put_body("trace:1#input", body, vec![])?;
s.sync()?;      // the ACK — an fsync of the sidecar, never of the store file
s.flush()?;     // the flip: one superblock write publishes everything above it
s.close()?;     // settles, removes the sidecar, leaves one file
```

Parts and fold segments append **into** the file past the committed tail; a commit is one
superblock flip, which is the crash model the alternating slots were designed for — an interrupted
write lands in bytes no committed superblock refers to. The protocol collapse is measurable: a
flush costs three fsyncs where the directory protocol needs six, and a whole session records 31
filesystem mutations where the checkpoint bridge recorded 66. Recovery gets simpler for the same
reason it gets safer: a retained commit newer than live cannot exist, manifest staging litter
cannot exist, and the fold is never truncated at open — the committed extent lists *are* the
truncation, read rather than performed.

Every documented operation now runs against the file: merge splices its output with one flip;
re-fold performs its generation swap, its retained-log purge, and its space frees as **one atomic
state**, so the window where erased content stayed readable through a retained name — which the
directory protocol closes with propagated unlinks and an open-time reconciliation pass — cannot
occur; verification walks members with the same chain-and-pin standard; recovery
(`recover_manifest_file`) validates candidates at their exact tails and promotes with one flip.
**Backup of a single-file store now produces a sealed container**: the committed snapshot, every
member one aligned extent, flagged final so no writer will ever open it, verified whole, published
only through a rename that refuses to replace.

The container plane advances to revision 2 to carry this: members are extent lists (a growing
segment gains an extent per commit that extended it, and physically adjacent runs coalesce), fresh
extents start on 4096-byte boundaries, and freed extents carry the commit that freed them. That
stamp is what makes the new reclamation honest: `punch_free_space` deallocates the aligned
interior of extents older than the retention window — superseded parts, purged manifests,
abandoned generations — in place, offsets unmoved, and defers anything a recent reader could still
be holding. Revision-1 containers open exactly as written and publish revision 2 on their first
commit.

The proof grew before the engine did, in the order the work was done: the crash simulator gained
tears that land inside a superblock's 56 defined bytes (the old length-proportional tears provably
never could), a trace lint that proves slot alternation from the recorded operation log, and four
new protocol sweeps — session, merge, erase, and free-punch — enumerating 288, 369, 819, and 63
crash states respectively. Each passed on its first run against the finished engine.

The checkpoint bridge and the directory layout remain in this release and leave in the next: the
roadmap's Phase 1 ends with the directory store ceasing to be a layout at all, a one-shot
converter carrying old stores forward, and the CLI reworked around the file. This release is the
engine underneath that subtraction.

#### The Node binding opens the file

`open(path)` in the native Node binding now opens the single-file store directly: the path IS the
database, the write-ahead log and lock live beside it, and there is no prepared hot directory and
no fold-back step. `checkpointIntoContainer` is gone with the layout it served — a flush already
leaves the file current, so there is nothing left to checkpoint into. Backups restore through
member-verified copying of the sealed file (packs still restore by conversion, as the retired
artifacts they are), recovery reads the file's own manifest, and a missing backup source now
refuses as `NOT_FOUND` instead of a shrug about unrecognized layouts.

Stores created by earlier releases as directories are not opened by this binding any more; convert
them once with `turndb convert`.

#### The portable package writes one file

`open(path)` in the `turndb` npm package now names a `.turndb` **file**, not a store directory:
the WASI preopen is the file's parent, the store grows inside the single file, and the `-wal`
sidecar lives beside it under the same mount. Parent directories are created exactly as the
directory open always created its own. Opening an existing directory refuses and names `convert`
as the retired layout's one door.

The whole portable suite — and the two-way native/WASI interoperability proof — now runs on
single-file stores, which also hardened the engine underneath: a native open now refuses a fold
segment whose committed bytes scan short ("the fold lost durable data"), the same claim the
directory layout made through its committed-tail check.

## 0.1.3 (2026-08-11)

### Features

#### Opening a `.turndb` no longer copies its history

Opening a container for writing materialized every member into the working directory first, so
appending one record to a large store paid for a full copy of its history before the first write.

Parts and sealed fold segments are immutable once committed, so where they lie is placement rather
than identity — the manifest names them, and the read path has taken range readers rather than
paths since packs existed. They stay in the container now and the writer reads them as extents.
Only state a session actually mutates has to become a file.

What still materializes is the manifest, the dictionaries, the sidecars, and fold segments from the
committed tail's segment upward — that one because recovery truncates it, and any above it because
recovery unlinks those, and neither can be done to a member of a container. Everything below is
sealed by definition: the committed tail is strictly beyond it.

The working directory answers first for any name it holds, which is what makes an interrupted
session still resume correctly — a member beside the manifest is one that session rebuilt, and the
manifest commits to that copy.

The remaining copy is therefore bounded by the segment size rather than by the store. On a fixture
whose fold spans seven segments, a reopen copies one of them — 8,088 bytes of 80,616.

### Fixes

#### A store you create but never write to is still a store

Creating a `.turndb` and applying no records left a container holding no members at all — every
later command refused it with `container member not found: MANIFEST`, and the working directory
was left behind. Reaching it took nothing exotic: `turndb write new.turndb input.jsonl` where
every line of the input is skipped, which is what a mistyped schema or an empty file produces.

A directory store announces itself as new precisely by having no manifest on disk, and a store
that never applies a record never commits one. A container has no equivalent affordance — its
members *are* its state — so the checkpoint now writes the manifest it already holds rather than
looking for a file that was never going to exist. An empty container opens, scans to nothing, and
takes writes afterwards, exactly as the directory it mirrors always did.

Existing containers were never affected: a zero-record write into one committed cleanly and kept
its contents throughout.

## 0.1.2 (2026-08-08)

### Features

#### A store can be one file you write to

turndb has always had a single-file form. `pack` produced one, and it read and answered SQL
identically to the directory it came from — but only that: a pack is sealed by definition, its
footer at EOF is what marks it complete, and appending past that footer leaves a window where EOF
is not a footer and a crash leaves a file nothing can open.

This release adds the form that grows. A **container** holds what a pack holds under the same
names, and addresses itself from two superblock slots at the head of the file instead. The slots
are written alternately, so the state a reader resolves is never the slot a writer is touching, and
everything else appends beyond the last committed tail. Recovery is not a repair: the newest slot
that passes its checksum is the state, and uncommitted bytes past its tail are ignored.

```sh
turndb write mystore.turndb traces.jsonl   # creates the file and ingests into it
turndb write mystore.turndb more.jsonl     # appends
turndb query mystore.turndb "SELECT model, count(*) FROM t GROUP BY model"
turndb reclaim mystore.turndb              # return the space repeated sessions leave behind
```

Writing keeps the arrangement SQLite settled on, because append semantics, fsync, and rename
atomicity are properties a directory has and a byte range inside a file does not: one file at rest,
working state in `<file>-hot` while a writer holds it, folded back in on close. A working directory
that outlives its writer is **adopted** on the next open rather than rebuilt from the file — it
holds writes the container was never told about.

### Added

- **`turndb.container`** — `Container`, `reclaim`, and the format behind them. `store::open_read_container`, `store::checkpoint_into_container`, `store::ContainerStore`, `store::single_file_kind`, `store::open_read_file`.
- **CLI** — `write`, `checkpoint`, and `reclaim`. Every read verb (`inspect`, `ids`, `get`, `verify`, `query`) already took a directory or a pack and now takes a container too, told apart by magic rather than by extension.
- **`@turndb/cli`** — the command line as an npm package, so `inspect`, `verify`, and `query` no longer require cargo. A native binary delivered through a per-platform package; no WASM fallback, because the binary needs positioned reads, `flock`, and hole punching, and a platform without a build should say so rather than silently run a different engine. Published slice: `linux-x64-gnu`.
- **Single-file reads from both npm packages** — `NativeSnapshot.openFile` and `checkpointIntoContainer` in `@turndb/native`, `openFile` in the portable `turndb`, and `singleFileKind` in both. A `.turndb` had been unreachable from npm since 0.1.0: packs shipped in that release and neither binding could open one.
- **FORMAT.md** — a normative `## The container` section, and artifact relocatability stated as an invariant. Every offset inside a part or fold segment is relative to that artifact's start, so the same bytes are valid as a file, as an extent, or as a remote object. That was already true and load-bearing for `pack`; a field holding a file-absolute offset would have ended it silently.

### Fixed

- **`Fold::disk_bytes` returned zero for any fold not backed by a directory** — it built paths from a field that is a label for pack- and container-backed folds, and `.ok()` turned the failure into a number. Space accounting under-reported a single-file store's fold as empty.
- **The portable binding's first call reported an errno instead of itself.** Opening a store directory that did not exist failed inside Node's WASI constructor with `UVWASI_ENOENT` — no path, no cause. `Store::open` creates the directory; this binding could not only because WASI preopens before the guest runs, so it now creates it too. `openFile` refuses a missing file by name instead.
- **An unbounded read on every writable open.** Materialization read each member whole before writing it back out, in an engine whose read side is admission-bounded precisely so that cannot happen. It streams. The CLI's `verify` had the same shape for packs as well as containers, and now hashes through a streaming digest.

### Testing

The crash simulator gained a `LastPendingOnly` variant — only each file's last pending write survives, which models the intra-file reordering a missing fsync exposes and which no prior variant expressed. All six pre-existing publication sweeps pass under it unchanged. Two new sweeps cover the container: publishing one state inside the file, and the session cycle around it. The second found a real bug on its first run — materialization made member *contents* durable and never their *names*, so a crash could publish a working directory that looked complete, was adopted on the next open, and was missing files.

The corruption storm gained container coverage, in two forms. Byte mutation cannot reach the directory walk at all, because every route to it is checksum-gated; reaching it needs a harness that damages the payload and then repairs the checksums over it, which is what a writer bug or anyone holding this format's encoder would produce.

## 0.1.1 (2026-08-07)

### Fixes

#### Documentation describes the released project; the portable npm package publishes through CI

Public documentation is aligned with the released 0.1.0: registry publication is
recorded as fact (2026-08-06, three artifacts), internal branch, commit, and
process references are removed, review-reply and sprint-log prose is rewritten
as documentation, and the npm-facing READMEs use absolute links that resolve
from the registry pages. ROADMAP.md, largely completed, is retired.

The portable `turndb` npm package now publishes through the same tag-gated,
owner-approved release path as the crate and native packages: a dedicated
workflow builds it from the exact annotated lockstep tag, runs the package
suite, exercises the packed tarball on every supported Node major, and
publishes that exact tarball via npm trusted publishing.
