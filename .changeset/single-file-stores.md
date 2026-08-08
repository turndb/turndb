---
default: minor
---

# A store can be one file you write to

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

## Added

- **`turndb.container`** — `Container`, `reclaim`, and the format behind them. `store::open_read_container`, `store::checkpoint_into_container`, `store::ContainerStore`, `store::single_file_kind`, `store::open_read_file`.
- **CLI** — `write`, `checkpoint`, and `reclaim`. Every read verb (`inspect`, `ids`, `get`, `verify`, `query`) already took a directory or a pack and now takes a container too, told apart by magic rather than by extension.
- **`@turndb/cli`** — the command line as an npm package, so `inspect`, `verify`, and `query` no longer require cargo. A native binary delivered through a per-platform package; no WASM fallback, because the binary needs positioned reads, `flock`, and hole punching, and a platform without a build should say so rather than silently run a different engine. Published slice: `linux-x64-gnu`.
- **Single-file reads from both npm packages** — `NativeSnapshot.openFile` and `checkpointIntoContainer` in `@turndb/native`, `openFile` in the portable `turndb`, and `singleFileKind` in both. A `.turndb` had been unreachable from npm since 0.1.0: packs shipped in that release and neither binding could open one.
- **FORMAT.md** — a normative `## The container` section, and artifact relocatability stated as an invariant. Every offset inside a part or fold segment is relative to that artifact's start, so the same bytes are valid as a file, as an extent, or as a remote object. That was already true and load-bearing for `pack`; a field holding a file-absolute offset would have ended it silently.

## Fixed

- **`Fold::disk_bytes` returned zero for any fold not backed by a directory** — it built paths from a field that is a label for pack- and container-backed folds, and `.ok()` turned the failure into a number. Space accounting under-reported a single-file store's fold as empty.
- **The portable binding's first call reported an errno instead of itself.** Opening a store directory that did not exist failed inside Node's WASI constructor with `UVWASI_ENOENT` — no path, no cause. `Store::open` creates the directory; this binding could not only because WASI preopens before the guest runs, so it now creates it too. `openFile` refuses a missing file by name instead.
- **An unbounded read on every writable open.** Materialization read each member whole before writing it back out, in an engine whose read side is admission-bounded precisely so that cannot happen. It streams. The CLI's `verify` had the same shape for packs as well as containers, and now hashes through a streaming digest.

## Testing

The crash simulator gained a `LastPendingOnly` variant — only each file's last pending write survives, which models the intra-file reordering a missing fsync exposes and which no prior variant expressed. All six pre-existing publication sweeps pass under it unchanged. Two new sweeps cover the container: publishing one state inside the file, and the session cycle around it. The second found a real bug on its first run — materialization made member *contents* durable and never their *names*, so a crash could publish a working directory that looked complete, was adopted on the next open, and was missing files.

The corruption storm gained container coverage, in two forms. Byte mutation cannot reach the directory walk at all, because every route to it is checksum-gated; reaching it needs a harness that damages the payload and then repairs the checksums over it, which is what a writer bug or anyone holding this format's encoder would produce.
