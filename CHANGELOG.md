# Changelog

All notable changes to this project are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

TurnDB does not follow Semantic Versioning yet, because it is pre-1.0
and nothing here is a compatibility promise. See **Stability** below
before relying on anything in this file.

## [0.1.0] — 2026-08-06

First release, published as three artifacts: the `turndb` crate on
crates.io, the portable `turndb` npm package, and `@turndb/native`.

### Stability

**Pre-1.0. Format version 2. Not frozen.** [`FORMAT.md`](FORMAT.md) is
normative: where it and the code disagree, one of them is a bug.

Nothing below is a stability commitment. Specifically:

- **The on-disk format is not frozen and does not need to be.** A
  re-fold rewrites every part and the fold generation, so a format
  change is a migration rather than a break — see
  [`FORMAT.md`](FORMAT.md#compatibility).
- **The API is not frozen.** There is no prior release, so there is
  nothing to be compatible with and no deprecation policy yet.
- **There is no supported-version window.** Security fixes land on
  `main`; see [`SECURITY.md`](SECURITY.md).

**One limit a reader should not miss, because it is a correctness
property rather than a feature gap:** the single-writer invariant is
OS-enforced on Unix via `flock` and **is not enforced under WASI**,
where it is the embedder's obligation. The measured consequence, and why
a clean `verify()` does not settle it, is in
[`FORMAT.md`](FORMAT.md#the-writer-lock).

### Added

First release, so everything is new.

- **Durability** — WAL with an explicit ACK point, all-or-nothing batch
  replay, a single commit point (the manifest) with a checksummed commit
  log, snapshots, and explicit recovery. See
  [`docs/recovery.md`](docs/recovery.md).
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
  [`docs/bounded-compaction.md`](docs/bounded-compaction.md).
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
  [`FORMAT.md`](FORMAT.md#the-writer-lock).
- **No encryption.** The format reserves a flag bit and refuses it.
- **No parity or erasure coding.** Corruption is detected at every
  level; repair is the storage layer's job.
- **No daemon, network, cluster or consensus.** Scale-out is more
  stores, not a bigger one.
- **Threat model, completed hardening and residual risks** are in
  [`docs/security-review.md`](docs/security-review.md), which is scoped
  as of 2026-08-03 and is not an independent third-party audit.

[0.1.0]: https://github.com/turndb/turndb/releases/tag/v0.1.0
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
