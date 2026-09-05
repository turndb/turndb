# Operation registry

Status: **normative closed registry of public operation semantics**. This registry is delegated by
[`ONTOLOGY.md`](../ONTOLOGY.md). It maps product and API spellings to the ontology; it cannot add a
state, transition, observation, relation, or evidence kind.

> If an operation spelling is not listed here, it has no independent TurnDB lifecycle meaning.

An implementation may expose ordinary language aliases or narrower helpers. Those names are
conveniences only: their behavior must decompose entirely into the listed mappings. The capability
contract reports runtime support for its explicitly identified Tier-1 subset; registry entries
outside that subset may be surface-specific conveniences. Presence here defines meaning, not
availability in every binding.

Some writer-bound convenience methods require the current store authority to include every
earlier accepted mutation before their primary work. Native Node's `verify`, `compact`,
`compactBounded`, `contentPunch`, and `refold` first perform
**synchronization**, **publication**, and **settlement** when a pending change set exists. Python's
`compactBounded` and `refold` do the same. CLI `compact` and `refold` also perform synchronization,
publication, and settlement; CLI `content-punch` first performs **publication** and **settlement**
when a pending change set exists. Writer-bound `snapshot` and `querySql` first perform
**publication** and **settlement** when
needed and then open a read view. These preludes are part of those surface-qualified composites;
they do not change the meaning of the underlying verification, merge, content punch, refold,
inspection, or resolution act.

Established public result shapes expose a numeric field named `commit` for the selected store
authority. This applies to the `Store` and `ReadStore` manifest accessors, snapshot `commit`, health
`commit`, and backup/restore result `commit`. The field is an API encoding, not an ontology synonym:
`0` encodes the **canonical origin**, and a positive value encodes the **manifest revision** carrying
that manifest `commit` counter. A manifest-promotion report always names a positive revision. An API
accepting a retained manifest revision, such as `openAt`, accepts only a positive value unless its
own contract explicitly says that it accepts this authority encoding.

## Data and observation operations

| Operation spelling | Ontology mapping |
|---|---|
| `write`, `put`, `put_record`, `apply` | **acceptance** of one mutation or mutation batch. A requested durability option additionally performs **synchronization**. |
| `delete` | **deletion**, and therefore **acceptance** of a tombstone. A requested durability option additionally performs **synchronization**. |
| `sync` | Completes a delayed **publication acknowledgement** first when newer accepted mutations depend on a selected-but-unacknowledged authority, then performs **synchronization**. It performs no new publication or settlement. |
| `flush` with a pending change set | **publication** of the pending change set as a manifest revision and container state. A successful final durability barrier produces a **publication acknowledgement**, makes the included mutations durable, and is followed by **settlement**. If the successor becomes selected but the operation obtains no publication acknowledgement, the live handle adopts the published authority without claiming crash durability from that publication and leaves redundant WAL input for a later settlement attempt. |
| `flush` without a pending change set | When redundant WAL input remains from an earlier publication whose final durability barrier was unacknowledged, completes that barrier, produces the delayed **publication acknowledgement**, and performs **settlement**; otherwise no transition. |
| `Container::commit` | With staged container changes, performs container-state **publication** and produces its **publication acknowledgement** when the final durability barrier succeeds. Without staged changes, it completes any still-unacknowledged final barrier and otherwise performs no transition. It does not by itself publish a manifest revision. |
| `openWriter` on an absent path with no reclaim anchor | Performs **creation**, then opens a writer view over the **canonical origin**. |
| `openWriter` on an absent path with a valid reclaim anchor | Continues the already-started **reclaim**: performs **verification** of the anchor, copies it, and performs **artifact installation** at the absent store path before opening a writer view over the restored current **store authority**. It never performs creation or container-superblock publication over the anchor. |
| `openWriter` on an existing store | Opens a writer view over the current **store authority** and performs **WAL replay** when replay input exists. |
| `openSnapshot`, `open_read`, `open_read_at` | Produces a **read view** pinned to a store authority. The operation is an ordinary handle-opening act, not a semantic transition named snapshot. |
| writer-bound `snapshot` | Performs the applicable publication prelude above and produces a **read view** pinned to the resulting store authority. |
| `get`, `scan`, `readContent`, read-view `querySql` | Read-only **resolution** over a writer view or read view. |
| writer-bound `querySql` | Performs the applicable publication prelude above, then read-only **resolution** over the resulting read view. |
| `schema`, `explainScan` | Read-only **inspection** of field or execution facts without resolving result rows. |
| `carve`, `Carve`, caller-supplied spans | **content decomposition** that produces a byte-exact content program and zero or more pieces; the spelling does not admit a separate carve transition. |
| CLI `import` | For each input record, performs **content decomposition** and **acceptance**; after input completes, performs **synchronization**, **publication**, and **settlement**, then releases the writer handle. |
| CLI `inspect` | Read-only **inspection** of store authority, members, and transient protocol inventory. |
| CLI `ids`, `get`, `query` | Read-only **resolution** over a read view. |
| CLI `snapshots` | Read-only **inspection** that lists retained manifest revisions; the spelling does not admit a snapshot entity or transition. |

## Maintenance and lifecycle operations

| Operation spelling | Ontology mapping |
|---|---|
| CLI `compact`, Node `compact(true)`, total `merge_range` | Total **merge** of the selected parts referenced by the current manifest revision, followed by **publication** when a replacement part is produced. |
| `compactBounded` | One budget-bounded **merge**, followed by **publication** when a replacement part is produced. |
| Node `compact(false)` or default `compact()`, `autoCompact`, `maybeCompact` | A policy-selected optional **merge**, followed by **publication** when a replacement part is produced; a no-op result changes no state. |
| `refold` | When the current authority references one or more parts, performs **refold**, followed by **publication**. When it references no parts, it is a no-op and performs no transition. |
| `contentPunch`, `content-punch`, `punch_unreferenced` | **content punch**. When newly unreachable blocks are not yet declared, it first performs **publication** of a manifest revision and container state declaring the complete dead-block set; only an acknowledged declaration publication may be followed by deallocation. When every dead block is already declared, it retries deallocation without publication. When no dead block exists, it performs no transition. |
| core `punch_free_space` | **free-space punch** only. |
| CLI `free-space-punch` | Performs **WAL replay** when needed, **free-space punch**, then the applicable **publication** and **settlement** close prelude. |
| `reclaim` | **reclaim**. |
| `erase` | First performs **resolution** to determine which requested record slots are present. If none is present, it performs no transition. Otherwise it composes **deletion**, **synchronization**, one or more **publication** transitions, a total **merge** when needed, and **refold** when parts remain to rebuild. It is not a distinct erasure state. |
| `backup` | Performs source **synchronization**, **publication**, and **settlement** as needed; performs **creation** for a temporary destination store; copies the current **store authority** into it; when that authority is a manifest revision, performs destination-container **publication** selecting the copied revision; performs **verification**; then performs **artifact installation** at the destination path. A canonical-origin source leaves the temporary destination at its canonical origin before installation. |
| `restore` | Copies an artifact to a new temporary destination, performs **verification** of that store, then performs **artifact installation** at the destination path. It does not perform container-superblock publication, WAL replay, or manifest promotion. |
| CLI `recover`, `recoverManifest`, `promote_manifest_file` | **manifest promotion**; the short CLI spelling does not admit a general recovery transition. |
| `verify` | **verification** only at the storage primitive; a writer-bound actor may perform the declared manifest-revision prelude first. Its result is scoped evidence, not repair. |
| writer-bound `close` | Applies the requested **synchronization**, **publication**, and **settlement** policy, completing any delayed **publication acknowledgement** before it destroys redundant WAL replay input, then releases the handle. The spelling does not introduce a closed state. |
| read-view `close` | Releases the handle only; no state transition or observation. |

## Reporting and capability operations

| Operation spelling | Ontology mapping |
|---|---|
| core `estimate_compaction_space`, core `estimate_refold_space` | Read-only **inspection**. |
| Node `estimateCompactionSpace`, Node `estimateRefoldSpace` | Performs **synchronization**, conditional **publication**, and **settlement** of prior accepted mutations, then read-only **inspection**. |
| `compiledCapabilities`, `spaceUsage`, health reports, metrics, debris reports | Read-only **inspection**. |

Inspection operations produce scoped inspection results. Their machine-readable fields are
governed by the capability, metrics, or error contracts that define them.

The Tier-1 subset and its per-runtime availability are defined by
[`capability-contract.md`](capability-contract.md). Operational ordering and failure boundaries are
defined by the dedicated lifecycle documents and [`FORMAT.md`](../FORMAT.md); those documents must
use the mappings above rather than assigning a second meaning to an operation name.
