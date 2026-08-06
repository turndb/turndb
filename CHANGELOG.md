# Changelog

All notable changes to this project are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

TurnDB does not follow Semantic Versioning yet, because it is pre-1.0
and nothing here is a compatibility promise. See **Stability** below
before relying on anything in this file.

## [0.1.0] — unreleased

**Not yet released.** No tag exists and nothing has been published to
crates.io or npm. This entry describes what 0.1.0 *would* contain; the
date is deliberately absent rather than guessed, and should be filled in
by whoever tags it.

Checked, and this is the claim in this file most likely to expire —
publication makes it false immediately. **Three names, because the claim
covers three publishable artifacts:** the `turndb` crate, the portable
`turndb` npm package, and `@turndb/native`.

```
git tag        0 tags

for u in https://crates.io/api/v1/crates/turndb \
         https://registry.npmjs.org/turndb \
         https://registry.npmjs.org/@turndb%2Fnative; do
  printf '%s  %s\n' "$(curl -s -o /dev/null -w '%{http_code}' -A '<your-agent>' "$u")" "$u"
done
               404 on all three
```

**Run the same loop against names that do exist — `serde`, `typescript`,
`@types%2Fnode` — and it must print `200`.** A check that cannot produce
a `200` is measuring your request rather than the registry, and there are
two easy ways to get exactly that:

```
curl … https://crates.io/api/v1/crates/turndb   without -A   403   not 404
curl … registry.npmjs.org/turndb                without https://   301   not 404
```

Send a `User-Agent`, write the scheme, and read the status code rather
than the headers.

### Stability — read this before the feature list

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
[`FORMAT.md`](FORMAT.md#the-writer-lock). It is stated there once rather
than restated here.

### Added

First release, so everything is new. Each item is already documented in
the tree; this file is not the first home of any claim.

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
  `unpack`). **`turndb help` prints the authoritative verb set; where it
  and this list disagree, it is right.**

### Known limitations

Documented rather than omitted. Each is stated in full elsewhere; these
are pointers.

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

### How this entry was derived

Stated so a reader can check it rather than trust it, and so the next
release can use a narrower denominator.

**Denominator: the entire history.** There is no prior tag to diff from
— `git tag` returns nothing — so the bound is every commit reachable
from `main`.

**Measured at `db357fb`.** These counts move with every merge, so they
are pinned to a commit rather than stated as current. Re-run against the
tagged commit when 0.1.0 is cut; a difference is expected and is the
point of naming the measurement point:

```
git rev-list --count db357fb             234 commits
git rev-list --count --no-merges db357fb 215 non-merge commits
git rev-list --count --merges db357fb     19 merge commits
git tag                                    0 tags
```

**Four of those pin to a commit. A fifth measurement does not, and is
deliberately not quoted here:** `gh pr list --state merged` counts merged
pull requests — a live query against GitHub, with no commit to pin it to
and a default result limit. **It has already drifted past the merge count
above, and can only drift further.** Run it yourself if you want
corroboration at the moment you are reading; a number printed here would
be a snapshot wearing the same clothes as the four that are not.

**Capabilities were taken from what is already documented** — the
`## What it does` table in [`README.md`](README.md) and the `docs/`
tree — and not from the commit log. That is deliberate: a changelog
derived from commit subjects reproduces whatever the subjects claimed,
whereas the documented surface has been reviewed. **The commit and PR
record is the traceability, not the source.**

**What this entry leaves out**, so the omission is a decision rather
than an accident: individual commits are not enumerated. 215 non-merge
commits across a first release describe how the engine was built, not
what a consumer receives. **A partition of them by subject prefix covers
205 and silently drops 10**, several substantial — which is why the
prefix partition was not used as the spine:

```
git log --no-merges --format='%s' db357fb | grep -cE '^[a-z][a-z0-9_-]*[(:]'   205
git rev-list --count --no-merges db357fb                                       215
```

[0.1.0]: https://github.com/turndb/turndb/commits/main
