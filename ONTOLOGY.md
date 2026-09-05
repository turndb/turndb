# TurnDB ontology

> **If it is not on the list, it does not exist.**

Status: **working authority**. This document is normative for TurnDB's project-specific concepts,
distinctions, and relations. Existing code and documentation are not assumed to conform; they must
be reconciled deliberately. [`FORMAT.md`](FORMAT.md) remains normative for physical encodings and
crash ordering.

## Authority and scope

This ontology defines the shared conceptual model used to reason about TurnDB data, observation,
durability, publication, retention, physical liveness, and maintenance. It exists so a method name,
binding convenience, or locally plausible explanation cannot silently create a new semantic state
or collapse two existing ones.

The following rules close the model:

1. The entity, state, transition, observation, relation, and evidence lists below are exhaustive for
   this document's scope.
2. A project-specific term has no TurnDB semantic force unless it is admitted here or in a registry
   explicitly delegated by this document.
3. A source-code symbol or public method may implement or expose an admitted concept. Its existence
   does not admit another concept.
4. A proposed or historical term in another document does not exist in the current model.
5. Ordinary-language words retain their ordinary meanings. Only project-specific distinctions are
   closed; this document does not attempt to define words such as byte, file, process, or failure.
6. A negative statement means only that this ontology does not admit the claim. It does not claim
   that an unknown runtime fact is false.

This ontology does not define API spellings, parameter lists, query grammar, scalar encodings, error
codes, on-disk layouts, operational recipes, migration procedures, or future plans. Those may be
specified elsewhere only within the delegation boundaries below.

## Competency questions

The ontology must be sufficient to answer these questions without importing an API's vocabulary:

1. What exists after a mutation is accepted but before it is synchronized?
2. What do synchronization and publication acknowledgements guarantee, and what remains
   unacknowledged when either barrier reports failure?
3. Which authority determines the state a reader observes?
4. How does a manifest revision differ from a container state?
5. What can a writer observe that an existing read view cannot?
6. Can several accepted mutations produce fewer record versions in a published manifest revision?
7. Which transformations change records, parts, fold bytes, or container layout?
8. Does deletion imply that content or storage has been reclaimed?
9. What keeps a historical revision readable, and what can end that readability?
10. What does verification establish, identify, and leave unchanged?
11. How are store, implementation, and resource facts observed without resolving record slots?
12. How is one byte value decomposed into a reconstructable content program and pieces?
13. How is a store born, settled, or installed at a new artifact path?

A concept that answers none of these questions and supports no axiom below does not belong here.

## Metamodel

Every admitted concept has exactly one kind.

| Kind | Meaning |
|---|---|
| **entity** | A logical, observational, or physical thing modeled by TurnDB. |
| **state** | A condition that can hold for an admitted entity. |
| **transition** | A change between admitted states or entities. |
| **observation** | A read-only act that derives a value or evidence without changing its subject. |
| **relation** | A permitted connection between admitted concepts. |
| **evidence** | A scoped result supporting a claim without changing the subject of that claim. |

The only admitted relation types are:

| Relation | Domain and meaning |
|---|---|
| **is a** | One concept is a more specific form of another and inherits its defining characteristics. |
| **part of** | One entity is a constituent of another. |
| **contains** | An entity owns or encloses another within the modeled state. |
| **describes** | An authority states the composition of another entity or state. |
| **identifies** | An identity denotes one entity at its defined scope. |
| **produces** | An act or entity yields an admitted entity, state determination, or evidence without implying that it remains current. |
| **selects** | An authority determines which candidate is current for an observer. |
| **pinned to** | A view fixes one authority as its observation boundary for the view's lifetime. |
| **references** | An entity identifies another entity required to interpret or reconstruct it. |
| **represents** | A physical entity realizes one logical entity without becoming identical to it. |
| **precedes** | One ordered entity is earlier than another of the same kind. |
| **supersedes** | A later entity replaces an earlier entity for resolution without erasing it. |
| **visible in** | An entity contributes to the result of a particular view. |
| **made durable by** | A transition advances the durability guarantee over an entity. |
| **published by** | A transition makes an entity selectable by new readers. |
| **installed by** | Artifact installation assigns a container state to a previously absent destination path without changing that state internally. |
| **evidenced by** | Evidence supports a scoped claim about an entity. |

No additional relation is implied by prose proximity or by an implementation dependency.

## Entities

### Logical data

| Concept | Definition |
|---|---|
| **store** | One logical TurnDB database, including its current store authority, retained manifest revisions, and any writer-only pending change set. |
| **record slot** | The logical history position that resolves to one record or absence in a particular writer view or read view. |
| **record ID** | The byte-exact UTF-8 identity and primary ordering key of a record slot. |
| **record** | The resolved value for one record slot, consisting of ordered attribute occurrences and named content values. |
| **record version** | One replacement value or tombstone in a record slot's ordered resolution history. |
| **tombstone** | A record version that makes older versions in the same record slot resolve to absence. |
| **mutation** | One requested record replacement or deletion applied in the writer's order. |
| **mutation batch** | A sequence of mutations admitted and recovered all-or-none. |
| **pending change set** | The writer-local, per-record-slot resolution of accepted mutations not yet published in a manifest revision. |
| **attribute occurrence** | One positionally ordered pair of an attribute name and an exact typed scalar; repeated names remain separate occurrences. |
| **byte value** | One finite byte sequence independent of any record name or physical representation. |
| **named content value** | The association of one unique content name and one byte value within a record. |
| **content identity** | The format-defined identity of one complete byte value, independent of its name or physical decomposition. |
| **content program** | An ordered sequence of literal spans and piece references that reconstructs one byte value exactly. |
| **piece** | A content-addressed byte sequence that may be stored, referenced, or both. |
| **piece identity** | The format-defined content address of one piece. |

### Observation and authority

| Concept | Definition |
|---|---|
| **writer** | The sole role permitted to mutate and publish a store while that writer is open. |
| **writer view** | The view produced by resolving the current store authority together with the pending change set. |
| **read view** | An immutable logical observation pinned to one store authority; its continued readability depends on the required physical bytes remaining available. |
| **durability frontier** | The writer-order boundary through which accepted mutations are guaranteed to survive reopening after a crash, either because acknowledged WAL synchronization covered them or because an acknowledged publication made their resolved effect durable. |
| **store authority** | The logical authority selected for a store: either its canonical origin or one manifest revision. |
| **canonical origin** | The manifest-less empty store authority represented only by the canonical sequence-zero container birth state. |
| **manifest revision** | One ordered manifest state, distinguished within a store history by its exact bytes and ordered there by its manifest `commit` counter; it names parts, fold generation and tail, sequence cursor, ancestry, and declared punched blocks. |
| **part sequence interval** | The inclusive numeric range carried by a part and used to order record-version resolution across parts. |

### Physical representation

| Concept | Definition |
|---|---|
| **write-ahead log (WAL)** | The sidecar carrying ordered input used to replay accepted mutations until the store becomes settled; it may transiently retain input already made redundant by publication. |
| **part** | An immutable, ID-sorted columnar representation of record versions, attributes, and content references. |
| **fold** | The content plane that stores content-addressed pieces separately from parts. |
| **fold block** | An independently framed storage unit containing pieces in a fold generation. |
| **fold generation** | One complete address space of fold blocks referenced by a manifest revision. |
| **container** | The writable single-file representation of a store, resolved through alternating superblocks. |
| **container member** | A named logical byte sequence stored in one or more container extents. |
| **container directory** | The checked mapping from member names to extents, member checksums, and free extents for one container state. |
| **superblock** | One of two fixed container authorities that can select a container directory and its tail. |
| **container state** | One atomically selectable physical state distinguished within its container by its exact superblock bytes and ordered there by superblock sequence. |
| **extent** | One physical byte range occupied by a container member or container directory, or recorded as free. |
| **free extent** | An extent not named by the container directory selected by the current container state and recorded there as free; a reader of an older container state may still address it. |
| **punched block** | A fold block whose payload is declared unavailable by the manifest revision selected as current before physical deallocation. |
| **allocation range** | A contiguous physical byte range treated as one subject when filesystem allocation is observed or changed. |

## States

| Concept | Applies to | Definition |
|---|---|---|
| **accepted** | mutation or mutation batch | The writer has completed admission and returned success for it. |
| **durable** | accepted mutation or mutation batch | Every mutation in the unit falls at or before the durability frontier, so reopening after a crash preserves its ordered effect subject to supersession by a later mutation. |
| **pending** | record version | It contributes to the pending change set and is not yet part of a published manifest revision. |
| **present** | record slot in a writer view or read view | Resolution yields a record for the slot in that view. |
| **absent** | record slot in a writer view or read view | Resolution yields no record for the slot in that view, whether no version exists or the newest applicable version is a tombstone. |
| **published** | manifest revision or container state | It has been made selectable through a container superblock transition. Publication may precede acknowledgement of the transition's final durability barrier. |
| **current** | store authority or container state | It is the authority or physical state selected for a newly opened ordinary read view. |
| **retained** | manifest revision | Its exact manifest bytes are preserved as a retained container member for reopening; retention does not determine whether it is current. |
| **revision-reachable** | container member | A current or retained manifest revision requires the member. |
| **live-content-reachable** | piece or fold block | At least one record resolved in the current manifest revision requires its content. |
| **allocated** | allocation range | The filesystem still assigns physical storage to its bytes. |
| **deallocated** | allocation range | The filesystem no longer assigns physical storage to its bytes, although its logical offsets may remain addressable. |
| **readable** | read view | Every physical byte required by the pinned store authority remains available and passes the checks required to read it. |
| **settled** | store | The pending change set is empty and the WAL contains no remaining replay input, including input already redundant with a publication. |

## Transitions

These names describe semantic changes. They are not a registry of public method spellings.

| Concept | Definition |
|---|---|
| **acceptance** | Admits a mutation or mutation batch into writer order and the pending change set. |
| **creation** | Establishes a new store at its canonical origin in a canonical sequence-zero container state. |
| **synchronization** | Applies a WAL durability barrier that advances the durability frontier without publishing a manifest revision. |
| **publication** | Writes and orders the required physical artifacts, then atomically makes a successor container state selectable; it may also publish a new manifest revision. Success of its final durability barrier produces a publication acknowledgement and makes included accepted mutations durable. |
| **settlement** | Removes all remaining WAL replay input after the pending change set is empty, making the store settled. |
| **WAL replay** | Reconstructs the pending change set from complete, valid WAL input after resolving the current store authority. |
| **manifest promotion** | Validates a retained manifest revision, publishes a container state selecting it as current, and abandons newer retained authority and physical tail state. |
| **deletion** | Accepts a tombstone for a record ID. |
| **merge** | Rewrites selected parts into replacement parts without rewriting fold content. |
| **refold** | Writes a new fold generation and rebuilds every part against it. |
| **content punch** | Declares unreachable fold blocks punched and then attempts to deallocate their payload bytes without moving offsets. |
| **free-space punch** | Attempts to deallocate eligible interiors of free extents without moving offsets or changing logical data. |
| **reclaim** | Copies every member named by the container directory selected by the current container state into a fresh container and atomically replaces the existing store-path artifact with it. |
| **artifact installation** | Atomically assigns a verified container artifact to a previously absent destination path without changing the artifact's internal store authority. |

## Observations

| Concept | Definition |
|---|---|
| **resolution** | Selects the newest applicable record version for each record slot, producing a record or absence. |
| **verification** | Checks a stated integrity scope and produces a verification result without repairing the checked object. |
| **inspection** | Reports scoped store, implementation, capability, resource, or operational facts without resolving record slots or changing the inspected subject. |
| **content decomposition** | Derives a byte-exact content program and zero or more pieces from a named content value without changing that value. |

## Evidence

| Concept | Definition |
|---|---|
| **durability acknowledgement** | The successful result of synchronization establishing that all accepted mutations ordered before its barrier are durable. |
| **publication acknowledgement** | The successful result of a publication's final durability barrier, establishing that the selected successor and every physical artifact it names survive reopening after a crash. |
| **verification result** | A scoped report of the checks actually performed and the failures actually found. |
| **inspection result** | A scoped report of facts observed by inspection; it does not imply integrity outside the facts actually inspected. |

Evidence establishes no claim outside its stated scope. In particular, a member-level verification
result is not a whole-store verification result, and successful verification is not repair.

## Relation assertions

Only the following general relation patterns are admitted in this revision. Qualifiers such as
current, retained, pending, earlier, and later use the admitted states and ordering above.

| Subject | Relation | Object |
|---|---|---|
| tombstone | is a | record version |
| free extent | is a | extent |
| punched block | is a | fold block |
| extent | contains | allocation range |
| punched block | contains | allocation range occupied by its payload |
| record | contains | attribute occurrence |
| record | contains | named content value |
| store | contains | manifest revision |
| store | contains | store authority |
| store | contains | pending change set |
| canonical origin | is a | store authority |
| manifest revision | is a | store authority |
| record slot | contains | record version |
| record ID | identifies | record slot |
| named content value | references | byte value |
| content identity | identifies | byte value |
| piece identity | identifies | piece |
| content program | describes | byte value |
| content program | references | piece |
| mutation batch | contains | mutation |
| accepted mutation | produces | record version considered by pending resolution |
| pending change set | contains | pending record version |
| writer view | contains | pending change set |
| read view | pinned to | store authority |
| manifest revision | visible in | read view pinned to it |
| canonical origin | visible in | read view pinned to it, or writer view while it is current |
| current store authority | visible in | writer view |
| pending record version | visible in | writer view |
| manifest revision | references | part |
| manifest revision | references | fold generation |
| part | contains | record version |
| part | contains | part sequence interval |
| part | references | piece |
| fold generation | contains | fold block |
| fold block | contains | piece |
| container state | part of | container |
| container state | contains | container directory |
| container state | selects | store authority |
| container | represents | store |
| superblock | selects | container directory |
| container directory | contains | container member |
| container directory | contains | free extent |
| container member | contains | extent |
| earlier record version | precedes | later record version for the same record ID |
| later record version | supersedes | earlier record version for the same record ID |
| earlier manifest revision | precedes | later manifest revision |
| later manifest revision | supersedes | earlier manifest revision as current |
| first manifest revision | supersedes | canonical origin as current |
| earlier container state | precedes | later container state |
| later container state | supersedes | earlier container state as current |
| accepted mutation or mutation batch | made durable by | synchronization |
| accepted mutation or mutation batch included in a publication whose final durability barrier succeeds | made durable by | publication |
| synchronization | produces | durability acknowledgement |
| publication whose final durability barrier succeeds | produces | publication acknowledgement |
| settlement | produces | settled state for a store |
| resolution | produces | record or absent state for a record slot in a writer view or read view |
| verification | produces | verification result |
| inspection | produces | inspection result |
| content decomposition | produces | content program and zero or more pieces |
| manifest revision | published by | publication |
| container state | published by | publication |
| creation | produces | store, container, canonical origin, and its sequence-zero container state |
| destination container state | installed by | artifact installation |
| container state selecting a retained manifest revision as current | published by | manifest promotion |
| durable accepted mutation or mutation batch covered by synchronization | evidenced by | that synchronization's durability acknowledgement |
| durable accepted mutation or mutation batch included in a publication | evidenced by | that publication's publication acknowledgement |
| entity | evidenced by | verification result whose stated scope covers it |

## Axioms

1. Every record slot has exactly one record ID, and every record is the resolved value of one record
   slot in one view. Content names are unique within a record; attribute names need not be unique,
   and attribute occurrence order is preserved.
2. Except where content has been explicitly made unavailable by content punch, reconstruction
   preserves named-content bytes and exact scalar representations.
3. Resolution is newest-first per record slot. A newest tombstone resolves to absence; an older
   value is not consulted.
4. The pending change set contains at most one resolved pending record version per record slot.
   Therefore several accepted or durable mutations may collapse into fewer record versions in a
   published manifest revision.
5. Acceptance does not imply durability. Durability does not imply publication. Publication makes
   a successor selectable; its acknowledgement makes included mutations durable. Neither makes the
   store settled because redundant WAL input may remain until settlement. A successor observed as
   selected after the operation fails without obtaining its final barrier acknowledgement is
   published for the live file but has no publication acknowledgement; the failed call must not
   claim that the successor or mutations made
   durable only by it will survive a crash.
6. Acknowledged synchronization advances the durability frontier through its WAL barrier and does
   not change an existing read view. An acknowledged publication advances that frontier through
   every mutation it includes. Failure to obtain either acknowledgement does not assert that bytes
   are non-durable; it withholds the corresponding guarantee.
7. Mutation-batch acceptance and WAL replay are all-or-none. A mutation batch is durable exactly
   when all of its member mutations fall within the same durability frontier.
8. A successful reader open across a publication resolves either the preceding current container
   state or the succeeding published container state, never a mixture. `FORMAT.md` defines
   conditions under which the whole open is refused.
9. Part sequence intervals, manifest-revision order, and container-state order are independent
   ordering domains. A container state may change while selecting the same manifest revision;
   maintenance may replace parts while preserving their sequence intervals.
10. A read view never replays the WAL. A writer view may include pending record versions that no read
   view can yet observe.
11. Current and retained are distinct states and may hold for the same manifest revision.
12. A read view is not a retained manifest revision. The former is an open observation; the latter
    is stored authority from which an observation may be opened.
13. Ceasing to be retained prevents a later open at that manifest revision; it does not revoke an
    already-open read view.
14. A read view's selected store authority never changes. Its readability can nevertheless fail
    if physical bytes it requires become unavailable or invalid.
15. Deletion changes record resolution and does not itself deallocate content or storage.
16. Merge, refold, content punch, free-space punch, and reclaim are distinct transformations. Merge
    rewrites parts. Refold rewrites the fold generation and parts. Content punch preserves offsets
    and attempts to deallocate declared fold-block payloads. Free-space punch preserves offsets and
    attempts to deallocate free-extent interiors. Reclaim rewrites container layout.
17. The manifest revision selected as current describes a punched block before its payload
    allocation range is deallocated. That range may remain allocated after interruption; a
    deallocated allocation range in a live-content-reachable block is not a valid content-punch
    result unless the block was declared first.
18. Content identity and piece identity use the same hash function at different scopes and are not
    interchangeable.
19. Verification detects within its stated scope and changes neither logical nor physical state.
    Inspection likewise changes neither logical nor physical state and establishes only its reported
    facts.
20. Manifest promotion includes validation and publication. It changes current authority, may make
    later record versions no longer visible, and neither repairs corrupt bytes nor replays the WAL.
21. Content decomposition may change physical deduplication and compression outcomes but the
    resulting content program always describes the same byte value.
22. The canonical origin is a real empty store authority, not a synthetic manifest revision. The
    first manifest-revision publication supersedes it as current.
23. Artifact installation changes destination namespace state, not the installed container's
    internal store authority; it is not a container-superblock publication.

## Forbidden equivalences and terms

The following equations are false:

```text
accepted              != durable
durable               != published
published             != settled
part sequence interval != manifest revision != container state
retained manifest revision != read view
mutation              != record version
tombstone              != reclaimed storage
revision-reachable    != live-content-reachable
content identity      != piece identity
verification          != repair
merge                 != refold != content punch != free-space punch != reclaim
publication           != publication acknowledgement
```

The following terms are not admitted:

| Term | Required replacement or treatment |
|---|---|
| **commit** without qualification | Use **manifest revision** for manifest authority, **container state** for physical authority, or **publication** for the transition. Existing public numeric `commit` fields are only the authority encoding delegated to the operation registry; they do not admit commit as a concept or make zero a manifest revision. |
| **snapshot** without qualification | Use **read view** or **retained manifest revision**. File-copy workflows belong to an operation registry. |
| **compaction** as one lifecycle action | Name **merge**, **refold**, **content punch**, **free-space punch**, or **reclaim** according to the transformation performed. |
| **punch** without qualification | Use **content punch** or **free-space punch**. A composite API spelling does not merge their semantics. |
| **recovery** without qualification | Use **WAL replay** for mutation replay, **manifest promotion** for selecting retained authority, or the exact interrupted-publication procedure. |
| **sync** as a general completion promise | State whether the promise is acknowledged **synchronization**, **publication**, **publication acknowledgement**, or that the store is **settled**. |

## Delegated closed registries

Delegation admits only the stated subject matter. It does not promote every noun in a delegated
document into this ontology.

| Registry | Delegated scope |
|---|---|
| [`FORMAT.md`](FORMAT.md) | Physical encodings, integrity fields, crash ordering, version levers, format limits, and compatibility. |
| [`docs/record-model.md`](docs/record-model.md) | Detailed record and named-content representation. |
| [`docs/field-types.md`](docs/field-types.md) | Exact scalar types and their cross-runtime representations. |
| [`docs/query-contract.md`](docs/query-contract.md) | Query requests, predicates, ordering, projection, cursors, and query results. |
| [`docs/error-taxonomy.md`](docs/error-taxonomy.md) | Stable engine error classes and codes. |
| [`docs/operation-registry.md`](docs/operation-registry.md) | Current public operation spellings and composites, each defined only as a mapping to concepts admitted here. |

The operation registry may name API spellings, aliases, composites, cancellation points, and
binding coverage. It may not introduce another semantic state, transition, observation, relation,
or evidence kind. A spelling absent from that registry may still exist as an implementation detail
or language convenience, but it has no independent TurnDB lifecycle meaning.

## Change rule

Changing this ontology is a semantic change. A change must identify the admitted or removed concept,
its kind, its exact definition, its permitted relations, the competency question it answers, and any
axiom or forbidden equivalence affected. Candidate concepts remain outside this file until that work
is complete.
