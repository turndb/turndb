# Content liveness and reclamation

TurnDB reports content reachability without pretending that logical death and physical reclamation
are the same thing. Rust `Store::content_liveness()` and native Node
`store.contentLiveness(options)` inspect a settled live snapshot and return:

- unique pieces and logical bytes referenced by visible records;
- compressed fold blocks containing at least one live piece;
- dead logical bytes sharing those live blocks (`strandedDeadLogicalBytes`); and
- whole blocks with no live references, including their raw and compressed payload sizes.

The last category is immediately reclaimable at the fold block boundary: Linux can punch those
payload extents in place, while every platform can remove them by refold. Stranded bytes require a
refold because removing part of a compressed block would invalidate its live contents. Thus
`deadLogicalBytes` equals stranded dead bytes plus the raw bytes in reclaimable blocks, while
`reclaimableBlocks.storedBytes` is the exact compressed payload eligible for removal. It is not a
filesystem-free-space prediction: frame bytes, allocation units, compression changes during refold,
and copy-on-write filesystems all affect physical results.

The inventory reads visible record programs and fold headers. It does not decompress content or
warm the block cache, but its cost is proportional to visible physical rows and fold blocks. It is
therefore separate from the constant-work health snapshot and accepts cooperative cancellation and
deadlines.

The writer memtable must be empty. Call `sync()` and `flush()` first, or use the Node method, whose
serialized actor observes earlier completed flushes. TurnDB refuses an unsettled inventory instead
of misclassifying unpublished content as dead. Blocks already declared punched are excluded: the
live manifest has already made those bytes inaccessible, even if an interrupted punch left some
physical extents for a retry.

These are general storage facts, not an automatic maintenance policy. Consumers decide when the
reclaimable payload justifies a punch/refold and whether stranded bytes justify the larger rewrite.
