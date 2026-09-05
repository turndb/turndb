# Content liveness and reclamation

TurnDB reports content reachability without pretending that logical death and physical reclamation
are the same thing. Rust `Store::content_liveness()` and native Node
`store.contentLiveness(options)` inspect the current store authority while the store has an empty
pending change set and return:

- distinct physical piece locations and their logical bytes as named by visible records' owning
  parts (one identity stored at two required locations counts twice);
- compressed fold blocks containing at least one live-content-reachable piece;
- dead logical bytes sharing those blocks (`strandedDeadLogicalBytes`); and
- whole blocks with no live-content-reachable pieces, including their raw and compressed payload sizes.

The last category is immediately reclaimable at the fold block boundary: Linux and Windows content
punch can deallocate those payload extents in place, while every platform can remove them by refold. Stranded bytes require a
refold because removing part of a compressed block would invalidate its live-content-reachable pieces. Thus
`deadLogicalBytes` equals stranded dead bytes plus the raw bytes in reclaimable blocks, while
`reclaimableBlocks.storedBytes` is the exact compressed payload eligible for removal. It is not a
filesystem-free-space prediction: frame bytes, allocation units, compression changes during refold,
and copy-on-write filesystems all affect physical results.

The inventory reads visible record programs and verifies each named physical piece through the
program's owning part dictionary, so it may decompress and warm the Fold block cache. Its cost is
proportional to visible physical rows, their piece references, and Fold blocks. It is therefore
separate from the constant-work health observation and accepts cooperative cancellation and deadlines.

The pending change set must be empty. Call `sync()` and `flush()` first, or use the Node method, whose
serialized actor observes earlier completed flushes. TurnDB refuses pending logical changes instead
of misclassifying unpublished content as dead; redundant WAL input alone does not invalidate the
inventory. Blocks already declared punched are excluded: when the current store authority is a
manifest revision, it has already made those bytes inaccessible, even if an interrupted content punch left some
physical extents for a retry.

These are general storage facts, not an automatic maintenance policy. Consumers decide when the
reclaimable payload justifies content punch or refold and whether stranded bytes justify the larger rewrite.
