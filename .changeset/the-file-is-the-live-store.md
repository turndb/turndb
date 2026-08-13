---
default: minor
---

# The file is the live database

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
