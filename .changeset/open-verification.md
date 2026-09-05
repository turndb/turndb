---
default: minor
---

# Writer open verifies structure; deep verification is a request

Writer open proves the structural evidence before it accepts a mutation: the container directory
and its checksum, the current manifest revision, every retained revision's name, parse, adjacency,
`prev` link, cursor and tail order, the presence of every part they name, every current part's
schema, every fold segment's framing, and the identity of every WAL frame that replay applies. That
costs time proportional to metadata, fold framing, and the WAL, never to content.

Everything `verify` checks can be obtained before the first write instead: `StoreOptions {
open_verification: OpenVerification::Deep, .. }` in Rust and `deepVerificationOnOpen: true` in
`@turndb/native`. Its cost is proportional to the whole store and its retained window. Measured on
a release build against a 32 MiB, 512-record store, the structural open took 14 to 20 ms and the
deep open 270 ms.
