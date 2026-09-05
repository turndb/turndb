# Exact named-content identity

Every named content value has one identity: BLAKE3 of the exact bytes obtained by concatenating its
ordered content program. The identity describes the value, not its name, record, piece boundaries,
or physical location. Identical bytes therefore have the same identity across records and names.

Writers compute the identity while ingesting spans and persist all 32 bytes in the WAL and immutable
part. The current physical format has no representation for an unidentified stored value. A WAL or
part occurrence without the required identity is malformed and refused.

Metadata projections can return the identity without opening fold blocks. Full reconstruction checks
every piece and then checks the resulting value against the stored identity. Explicit verification
does this for every named content value resolved through the current manifest revision.

Content erasure is not represented by dropping the identity. Content punch is declared by the
current manifest revision's block intervals; refold removes the unreachable record/content occurrence altogether.
