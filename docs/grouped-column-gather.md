# Grouped column gather

Structured scans hand already resolved candidates to a bounded physical projection layer. The
layer groups immutable rows by part, gathers only selected or predicate-bearing columns, and restores
the candidates to their original global id order. It is shared by writer-backed scans and immutable
reader views; pending record versions remain an in-memory writer overlay.

## Attribute gather

For all requested rows in one part, the gather:

1. opens attribute layout, layout offsets, and column metadata once;
2. decodes each requested row's selected column ordinals, retaining exact interleaving;
3. opens each used sparse row-id, fixed-width value, and string/binary dictionary section once;
4. addresses each requested occurrence and rebuilds each row in layout order.

Columns are keyed physically by `(name, scalar type)`. Selecting a name therefore gathers every
observed type column for that name. Duplicate occurrences remain duplicates, including interleaving
such as `[a, b, a]`; explicit null still occupies layout/rid space despite having zero value bytes.

## Named-content gather

For each selected content name, the gather opens its metadata, sparse row ids when present, program,
offsets, and whole-value identities once. It decodes programs only for requested rows. Absent, present
empty, and present non-empty values remain distinct, and content results retain canonical name order
until the scan response applies the consumer's requested selection order.

Metadata projection does not resolve fold pieces. Byte projection reuses the gathered program and
identity and reconstructs only after predicate acceptance and reconstruction-budget admission.

## Bounds and interruption

Projection chunks contain at most 64 resolved candidates and are also capped by the number of output
rows still requested. Every gathered candidate is therefore semantically examined before a full page
can stop. Predicate rejection starts another gather instead of speculatively projecting beyond the
page's demand. Deadline and cancellation checks occur around every chunk and during semantic row
processing.

Visibility resolution has its own `max_resolution_entries` budget, predicate evaluation has
`max_examined`, and content reconstruction has `max_reconstructed_bytes`. The gather does not merge
those meanings or weaken their cursor boundaries.

## Boundary of the claim

This is grouped physical decoding, not a new on-disk format and not general vectorized execution.
Sparse occurrence lookup is still directly addressed per requested row; predicates operate on
partial semantic records; and there is no SIMD expression engine or secondary index here. The useful
guarantee is narrower: shared physical decoder setup is batch-scoped, unrelated columns stay closed,
global ordering is restored exactly, and the optimization is below every supported embedding path.
