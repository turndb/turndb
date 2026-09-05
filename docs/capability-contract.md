# Capability contract v2

Status: normative Phase 3 contract. Package semver and on-disk format versions are independent of
this contract version.

TurnDB bindings report facts about the implementation that was actually opened. A consumer must not
infer capabilities from the package name, host operating system, or the presence of a similarly
named method in another binding. In particular, a WASI guest hosted by Linux still has
embedder-enforced writer exclusion, no threads, and no hole punching; a browser handle is read-only.
Operation meanings are closed by [`operation-registry.md`](operation-registry.md); this contract
reports which of those operations a runtime exposes.

The machine-readable representation is defined by
[`conformance/v2/capabilities.schema.json`](../conformance/v2/capabilities.schema.json). Every
profile carries `contractVersion: 2`. An implementation may add fields without advancing the
version. Consumers must ignore unknown fields. A change to an existing field's type or meaning, or
removal of a required field, advances the contract version.

## Profile shape

`profile` identifies the runtime family for diagnostics, not for feature inference. `operations` is
the authoritative set of callable Tier-1 operations. An operation absent from that set is not
silently approximated; invoking it through a convenience layer returns `UNSUPPORTED` or the layer
does not expose it.

The required mechanism facts are:

| field | meaning |
|---|---|
| `draftFormatEpoch` | the shared draft-epoch discriminator required by the current physical plane identities; not a complete format identity by itself |
| `writerExclusion` | `os_enforced`, `embedder_enforced`, or `read_only` |
| `positionedIo` | reads can address exact byte ranges |
| `threads` | the engine may execute work on native threads |
| `columnar` | the Arrow columnar lens is compiled in |
| `sql` | the read-only SQL adapter is compiled in |
| `arrowIpc` | SQL/columnar results can cross the boundary as Arrow IPC |
| `reclamation` | `content_punch_or_refold`, `refold_only`, or `none` |
| `cancellation.scan` | structured scans accept cooperative cancellation |
| `cancellation.lifecycle` | lifecycle operations accept cooperative cancellation |

`operations` uses the stable lower-camel spellings below. A binding may expose additional
language conveniences, but those do not become Tier 1 merely by existing.

| operation | contract |
|---|---|
| `openWriter` | open or create one container representing a store for a single writer |
| `openSnapshot` | open an immutable read view pinned to a store authority without replaying the WAL |
| `compiledCapabilities` | report compiled mechanisms independently of a particular handle |
| `write` | apply an ordered atomic group, with explicit durability request |
| `sync` | complete any prerequisite delayed publication acknowledgement, perform synchronization, and return its durability acknowledgement; no new publication or settlement |
| `flush` | publish the pending change set into immutable parts and a new manifest revision |
| `scan` | execute the structured query contract |
| `explainScan` | report prepared logical fields and physical scope without evaluating rows |
| `schema` | discover physical attribute/content names without reading values |
| `readContent` | reconstruct one named content value byte-exactly |
| `snapshot` | publish the pending change set as needed and create a read view pinned to the resulting current store authority |
| `querySql` | stream read-only SQL results as Arrow IPC |
| `backup` | synchronize and publish pending source changes, verify a self-contained destination container, and atomically install it at a new path |
| `verify` | verify the current store authority, any retained manifest-revision chain, and byte-exact reconstruction |
| `spaceUsage` | report reachability-aware storage accounting |
| `compactBounded` | execute one exact-budget part-merge unit |
| `refold` | when parts exist, write a new fold generation containing live-content-reachable pieces and rebuild every part against it; otherwise no-op |
| `erase` | perform the strong erasure composition for selected ids whose slots resolve present; an all-absent request is a no-op |
| `close` | apply the requested synchronization, publication, and settled-store policy, then release the handle |

## Runtime profiles

Native Unix writers report OS-enforced exclusion. The portable WASI writer reports
embedder-enforced exclusion even on a Unix host. Browser readers report `read_only`, omit every
mutating operation, and report `reclamation: "none"`.

WASI Preview1 lacks the atomic no-replace rename required by the backup/restore protocol. Container
birth can safely install one already-synchronized inode with WASI's atomic no-replace hard-link
creation, but that narrower primitive does not satisfy the advertised artifact-installation
contract. The portable profile therefore reports `atomicNoReplaceInstallation: "absent"` and omits
`backup`; it does not weaken backup into an unsafe copy. Native Node and Python expose the normative
backup operation only on targets whose compiled core reports the required no-replace installation
guarantee.

Different profiles may truthfully expose different operation sets under contract v2. Semantic
drift inside an operation they both expose is not a capability difference: `scan`, for example,
has one request, ordering, cursor, scalar, and result contract everywhere.

## Errors

Engine errors use the stable codes in [`docs/error-taxonomy.md`](error-taxonomy.md). Bindings may add
process-boundary codes such as native Node's `BUSY` and `CLOSED`; these are declared by that binding
and never reused for an engine condition. Capability discovery itself must not guess from rendered
error text.

## Conformance

[`conformance/v2/capabilities.json`](../conformance/v2/capabilities.json) contains the required
invariants for the core profiles. The Rust gate validates the compiled core against them; each
binding runner validates its public response and then runs only cases whose required operations are
present. A profile cannot make a failing case disappear by omitting an operation that its package
documents as callable.
