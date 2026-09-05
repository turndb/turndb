# Security review

This review covers TurnDB's current draft storage core, native Node binding, browser/WASI readers,
and Python binding. It is an engineering threat review, not formal verification or an independent
third-party audit. A physical-format or binding change must update this review and its adversarial
tests.

## Threat model

In scope:

- a container, backup container, retained manifest member, or WAL sidecar whose bytes are malformed,
  truncated, adversarial, or carry an unrecognized physical identity;
- accidental corruption, torn writes, decompression bombs, extreme counts and lengths, parser
  panics, and unsafe artifact-installation paths;
- untrusted values crossing a binding boundary, including out-of-range integers, oversized work,
  stale handles, cancellation, and concurrent method calls; and
- protecting host process availability and preventing a crafted artifact from redirecting reads or
  writes outside the paths the host supplied.

Out of scope:

- an attacker who can mutate an open store or WAL sidecar concurrently with TurnDB. Writer locking
  coordinates cooperating writers; it is not a sandbox against a hostile filesystem peer;
- authentication or confidentiality. CRC32 and BLAKE3 detect drift and bind identities but are not
  keyed authenticity. The format is plaintext and refuses its reserved encryption flag;
- authorization, tenant isolation, redaction, network security, and backup transport, which belong
  to the embedding application; and
- denial of service by code already executing in the same native process or WASI host.

## Format boundary

`FORMAT.md` defines one unfrozen draft epoch. Every physical plane has one exact current magic and
epoch. Readers do not probe old layouts, negotiate compatibility ranges, upgrade bytes, or invoke a
migration path. Unknown identities and epochs fail closed. This reduces the attack surface to the
parsers named by the current specification and makes discarded development artifacts ordinary
invalid input.

Checksummed structures are range-checked before allocation or parser use. Part sections, content
pieces, fold frames, manifest bodies, container slots, and member directories carry the integrity
evidence specified in `FORMAT.md`. Content identities are mandatory; reconstruction checks both
piece hashes and the complete value identity. Unknown required structure is corruption, not an
absence result.

## Resource admission

`ReadLimits` bounds stored and decoded atomic frames, WAL frames, fold blocks, and enumerated
objects before allocations or vector growth. `WriteLimits` bounds record, batch, and identifier
admission before WAL or fold mutation. Cache budgets govern retained residency but do not weaken
integrity or publication. Valid policy refusals are `RESOURCE_EXHAUSTED`; malformed settings are
`INVALID_ARGUMENT`.

One zstd frame and one part-encoding unit remain cooperative atomic work. Cancellation surrounds
those units but does not preempt codec execution. Deployments accepting arbitrary third-party
containers should validate them in a resource-limited helper process when codec CPU isolation is
required.

## Publication, WAL replay, and manifest promotion

Container publication alternates two checksummed superblock slots. An authentic slot from any other
draft epoch refuses the whole open rather than falling back to a stale predecessor. The WAL admits
only current frame tags; WAL replay truncates only a torn suffix and never interprets discarded tags.

Backup copies the current container state to a private sibling, fully verifies that exact staged store,
and installs with no replacement. Restore follows the same copy-then-verify rule. Cancellation or
failure before artifact installation removes staging best-effort and leaves the requested destination absent.
Source/staging aliases are refused before cleanup or copying. Manifest-promotion candidates are fully checked
before authority changes.

Checksums do not authenticate bytes against a party able to rewrite both payload and checksum.
No-replace artifact installation does not make a hostile parent directory safe.

## Binding boundaries

The native Node crate contains no `unsafe` blocks. Exact integers cross as `bigint`, content crosses
as `Buffer`, typed scalars retain their declared kind, and one bounded actor owns each writer.
Queue saturation is `BUSY`; stale handles are `CLOSED`; cancellation, filesystem failures,
corruption, and resource refusal retain distinct machine-readable classes.

The WASI binding's `unsafe` surface is its documented pointer/length ABI and allocator pair. A
host that violates that ABI may trap its own instance; WASM linear-memory isolation prevents the
contract from becoming native process memory access. WASI cannot enforce the single-writer lock, so
the embedder owns that exclusion.

## Residual risks

1. A hostile peer concurrently replacing filesystem objects used by an open store is not contained.
2. Lockfiles pin dependencies, but this review is not an audit of Arrow, DataFusion, zstd, napi-rs,
   WASI runtimes, or the package publication chain.
3. Cancellation is cooperative, not a real-time CPU limiter.

## Review rule

Every new persisted count, offset, or length needs an overflow-safe range check before conversion or
allocation, a nearest-invalid test, and mutation coverage. Every new physical identity must be added
to `FORMAT.md`; until the draft is explicitly frozen, replacing an identity means deleting the old
reader and replacing its fixtures. Every publication operation must name its last cancellable point
and prove no-replace behavior. Every new binding input needs exact type/range validation. Every new
`unsafe` block needs a local safety contract and an adversarial boundary test.
