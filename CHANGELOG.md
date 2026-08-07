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
