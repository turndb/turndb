# Records and named content

A TurnDB record has exactly three components:

- one non-empty UTF-8 record id;
- zero or more named content values;
- an ordered list of typed attributes.

Content names are non-empty and unique within one record. Their input order is not semantic; TurnDB
canonicalizes names by UTF-8 bytes for storage and projection. Each content value is an ordered
program of inline literals and content-addressed pieces whose concatenation reconstructs the exact
value. An empty program is a present empty value; absence means no content with that name.

Attributes are different: order and duplicate names are semantic and round-trip exactly. See
[`field-types.md`](field-types.md) for the closed value set and
[`content-identity.md`](content-identity.md) for value identity.

Writes with one id supersede that id's earlier record version. Deletion writes a tombstone so older
immutable parts cannot resurrect the record. Resolution chooses the newest record version or
tombstone across immutable parts, then applies the pending change set for writer-local reads.

The convenience `put`/`reconstruct` API uses the conventional content name `body`; it is a view of
this same model, not a second record shape.
