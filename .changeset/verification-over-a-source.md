---
default: minor
---

# Verification over a positioned source, in declared units

`turndb::store::SourceVerifier` performs the whole-artifact verification — every member's recorded
checksum, the retained manifest chain, every manifest's part digest, every part section and row
grammar, every operational piece-dictionary entry against its fold bytes, every fold frame, every
live named content value's identity, and every retained authority — over any `ReadAt` source as an
ordered list of units, each declaring the byte ranges it will read before it runs. A host that
fetches by range fills exactly those ranges and the unit completes in one pass; a unit that fails
leaves the verifier's state untouched, so the host fetches what the failure named and reruns it.
Once every unit has run the composed result equals `Store::verify` over the same bytes. Before
that the evidence is scoped to the units that ran and `report` refuses to call it more.

The browser core exposes it as `verifySource` and the resumable `verifyUnits`, and the browser
profile now lists `verify`. `BlockReadAt` is exported so a host can subclass it with its own range
fetcher. The transport's retry loop pins every range it fetches for an operation until that
operation completes: previously an operation whose working set exceeded the block cache evicted
its own first block while fetching its last and restarted for ever, and a two-block cache now
completes any operation at the memory cost of that operation's working set.

Measured with `cargo test --test verify_source` (6 tests): the composed report equals the
writer-side report field by field over a six-publication fixture, and every unit's reads lie inside
its declared footprint or the metadata the open already read. The browser conformance run verifies
the shared 53,406-byte fixture over a Blob whose cache holds two 4 KiB blocks and gets the same
report as over the whole buffer, in 100 units and 127 range fetches.
