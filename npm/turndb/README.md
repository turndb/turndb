# turndb

This package declares Node `>=22 <27`; its required rebuild/test matrix is Node 22, 24, and 26. See the
[support and compatibility policy](https://github.com/turndb/turndb/blob/main/docs/support-and-compatibility.md)
for the distinction between the portable WASI profile, native source qualification, and published
support.

A content-addressed columnar store for AI traces. Byte-exact, embedded, single-writer — a store is
a directory you can `tar`.

The engine is Rust compiled to `wasm32-wasip1`. **No native addon, no prebuild matrix, no
postinstall** — one `.wasm` runs everywhere Node does.

```sh
npm install turndb
```

```js
import { open } from 'turndb';

const store = await open('./traces');

store.putBody('alice/1700000000000/req-1#input', JSON.stringify(messages), {
  model: 'claude-opus-5',
  inputTokens: 1204,
});

// One logical inference, with independently addressable request and response bytes. The return
// value itself is the acknowledgement: it is produced only after the durability sync succeeds.
const ack = store.write([{
  kind: 'put',
  id: 'alice/1700000000000/req-1',
  contents: [
    { name: 'request', bytes: requestBytes },
    { name: 'response', bytes: responseBytes },
  ],
  // An array preserves order and duplicate names; an object cannot.
  attrs: [['kind', 'llm_exchange'], ['tag', 'first'], ['tag', 'second']],
}], { durable: true });
if (!ack.durable) throw new Error('the source copy is still required');

store.sync();    // the ACK point — durable from here
store.flush();   // seal into the columnar plane for other readers

store.get('alice/1700000000000/req-1#input');       // bytes, byte-exact
store.getText('alice/1700000000000/req-1#input');   // UTF-8 convenience; lossy on invalid bytes
store.scanIds({ prefix: 'alice/', limit: 50, reverse: true });

// Structured paging: the engine filters and projects, so a timeline page that selects no
// content opens no fold block. `next` is an opaque checked cursor.
const page = store.scan({
  prefix: 'alice/',
  direction: 'reverse',
  limit: 50,
  attrs: ['model', 'inputTokens'],
  contents: [{ name: 'body', mode: 'metadata' }],
  predicates: [{ kind: 'attr', name: 'model', op: 'eq', value: 'claude-opus-5' }],
});
page.rows[0].attrs;              // [['model', 'claude-opus-5'], ['inputTokens', 1204n]]
page.stats.io.foldBlocksTouched; // 0n on a metadata-only page
```

## Why

Agent and chat APIs are stateless, so every request re-sends the whole conversation. A trace store
therefore receives the same message text over and over. General stores pay for those bytes every
time; archival dedup stores collapse them but stop being queryable.

turndb splits the two planes — a **fold** holding content addressed by BLAKE3, and **parts**
holding ids, body programs and typed attribute columns. Because a part holds no content,
compaction never touches content.

## Three things to know

**`sync()` is the ACK point.** `putBody` is not durable on its own. `flush()` is a different thing
again: it seals writes into the columnar plane so *other* readers see them. Your own handle sees
its unflushed writes without either — a live view can read back what it just wrote for free.
`write(operations, { durable: true })` combines an atomic generic batch with that ACK point and
returns `{ applied, durable: true }`; a thrown error is never an acknowledgement.

**Flush cadence is a compression dial.** Blocks sealed short compress worse. Measured on real trace
traffic, flushing every record gave 15×; every 50 gave 171×; every 512 gave 292×. Batch your
writes.

**Cross-process exclusion is yours to provide.** The native engine takes an advisory `flock`, but
**this package is always the `wasm32-wasip1` build**, on every host including Linux and macOS, and
WASI has no advisory locking. The lock file is created and gates nothing.

The host layer permits one live `Store` per process. That is not enough isolation: another process
can still open the same directory. The obligation is therefore **at most one open writer per store
directory across every process.** The guest cannot enforce or detect a violation. In four measured
overlapping-writer runs on Node 24, both writers received successful `sync()` acknowledgements and
one writer's complete record set was silently discarded. The surviving store was internally
consistent and every remaining record was readable, so a clean read or verification cannot prove
that an acknowledged write survived. Other overlap patterns may fail differently.

Sequential opens, including different directories, reuse one WASI instance. This removes
per-construction external-memory pressure while keeping the sandbox narrow: only the current store
directory is mounted. A consumer that must hold multiple stores open concurrently needs separate
processes.

Call `close()` explicitly. A dropped handle is reclaimed when JavaScript eventually collects it, so
forgetting `close()` does not wedge the process forever, but collection has no timing guarantee and
the next `open()` refuses while the old handle is still live.

## Integrity and health

`store.verify()` verifies the **committed snapshot**: the retained-manifest chain, every immutable
part section, every fold frame, and byte-exact reconstruction of every named content value. It
returns exact record, content-value, content-byte, identity, part-section, fold, and retained-
manifest counts. Staged writes are deliberately outside that scope; sync and flush them first when
they must be included.

`store.health()` is the cheap operational view. It reports whether the handle is available plus its
commit and staged-entry counts; it makes no integrity claim. A clean health result is therefore not
a substitute for `verify()`.

Missing records remain `null`. Corrupt persisted state throws `TurndbError` with
`code === 'CORRUPTION'`, whether the damage prevents the store from opening or is found while
reading or verifying it. Callers can classify failures through `error.code` without parsing the
diagnostic message; unsupported operations use `UNSUPPORTED`, and malformed calls use
`INVALID_ARGUMENT`.

## Write admission

`open` accepts `maxRecordBytes`, `maxBatchBytes`, `maxBatchRecords`, and `maxIdentifierBytes` as
positive u32 numbers. Defaults are 64 MiB, 256 MiB, 4,096, and 4 KiB. The first two are not raw body
limits: they count deterministic worst-case complete WAL frames, treating every folded piece as
novel, so acceptance never depends on hidden dedup history. An atomic batch is completely charged and
validated before any member mutates the fold or WAL. See
[write admission limits](https://github.com/turndb/turndb/blob/main/docs/write-admission.md) for the exact unit and native API.

## Frame and persistent object admission

`open` also accepts `maxStoredFrameBytes` and `maxDecodedFrameBytes`, positive u32 values defaulting
to 512 MiB. They are checked before a WAL, part, or fold frame allocates stored or decoded linear
memory. `maxDirectoryEntries`, `maxWalFrames`, and `maxFoldBlocks` are positive u32 object-count
ceilings defaulting to 100,000, 100,000, and 1,000,000. They bound enumeration-driven collection
growth, physical frames in an unflushed WAL, and blocks plus block-id span in one fold generation.
`store.readLimits()` reports all five effective values.

A strict frame profile seals fold blocks early so small records keep progressing; one indivisible
oversized piece is refused before mutation, and an oversized part output is refused before
publication. Batch WAL frames and future filesystem/block objects are likewise admitted before the
associated mutation. See [atomic frame read admission](https://github.com/turndb/turndb/blob/main/docs/read-admission.md) and
[persistent object-count admission](https://github.com/turndb/turndb/blob/main/docs/object-admission.md).

## When a write stalls, and by how much

This build is single-threaded, so two operations run on the calling thread. Both are predictable
enough to budget for; this section gives the arithmetic.

**Block seals.** The fold gathers *unique* content until it reaches `blockTarget` (default 4 MiB),
then seals the block: one zstd compression, executed inside whichever `putBody` crossed the
boundary. Every other put is fast (hundreds of MB/s across the boundary); the crossing one pays the
whole seal. Content the store has seen before — same bytes, same BLAKE3 — dedups and never
accumulates toward a seal.

Your stall budget is a **rate**, not a total: one seal per `blockTarget` of unique content, so

    seals per hour  =  unique bytes ingested per hour  /  blockTarget

and the cost of each seal is set by `level`. Measured on 4 MiB blocks of synthetic unique-content
bodies through this package's wasm build, in a Node 22 container on one Linux x86-64 workstation
(a single workstation, Intel Core Ultra 7 155H): **~80ms per seal at level 3 — this package's default —
versus ~1.7s at level 19** (the engine's default, tuned for the native build where compression
runs on a thread pool).

Level 3 also costs more disk; no figure is published because measurements through the package on
one real trace corpus varied materially with workload ordering and sample configuration. The
default is set by the stall, not the disk cost. If
disk matters, measure your own workload: write a sample at both levels and compare the
directories. A trace workload writing 1.8 GB/day of unique content seals ~430 times a day: ~34
seconds of total stall at level 3, ~12 minutes — in 1.7-second pauses — at level 19. Pass
`level: 19` only if that trade is one you have measured your event loop against.

**Compaction.** Merges never rewrite content — that is the format's load-bearing claim and the
engine asserts it — but they rebuild the reference plane (piece dictionary and columns), whose
size tracks content volume. `autoCompact()` is a **total** merge: linear in the store's on-disk
bytes (~5s/GB at level 19 — four points on synthetic stores from 0.7 MB to 1.9 GB through this
build, Node 22, a single workstation; the level 3 merge cost is unmeasured), only ever run when you call
it, and the only merge that settles deletes. `maybeCompact()` is the bounded alternative: it
merges the oldest few parts, so
the stall is capped by the run you allow rather than the store you've accumulated. A long-lived
single-threaded embedder should call `maybeCompact()` on its idle path and reserve `autoCompact()`
for moments when a multi-second pause is acceptable.

## Attributes keep order and duplicate keys

Byte-exact reconstruction depends on both, and a JS object can represent neither. Pass an object
for convenience, or an array of pairs when it matters:

```js
store.putBody(id, body, [['finishReason', 'end_turn'], ['finishReason', 'max_tokens']]);
```

Stored integers return as JavaScript `bigint`, including small values. Integer-valued `number`
inputs are accepted only inside JavaScript's safe range; use `bigint` for the full signed-i64 range.
Unsigned u64 and UTC nanosecond timestamps use `{ u: bigint }` and
`{ timestampNs: bigint }` wrappers so a read-modify-write cycle retains their type. `Uint8Array`
stores binary metadata and `null` stores explicit null; missing remains absence of the key. See
[the version-2 scalar contract](https://github.com/turndb/turndb/blob/main/docs/field-types-v4.md).
The JSON-only WASM boundary carries those values as decimal text internally, never through a float.
Non-finite f64 values also use explicit text spellings rather than JSON `null`.

## Capability profile

`await capabilities()` (or `store.capabilities()`) reports what is reachable through this binding:
callable operation names, the lifecycle-journal capacity, and facts explicitly absent here. In
particular, allocated filesystem blocks and a cancellation token are absent on WASI; neither is
reported as zero or silently accepted.

`await compiledCapabilities()` is the separate answer to what mechanisms and format guarantees the
guest contains. It describes the WASI guest, not the host OS: this package reports embedder-enforced
writer exclusion and no threads even when Node itself runs on Linux. It deliberately carries no
`physical_erasure` prediction. What one erase did is returned by `store.eraseIds(ids)` as
`measured`, `not_applicable`, or `not_reclaimed` evidence for that operation.

The callable observability surface is pull-based and local to the open handle:

- `metrics()` returns cumulative operation counters and durations.
- `lifecycleEvents({ after, limit })` reads the bounded journal non-destructively, including cursor
  gaps and typed error codes.
- `contentLiveness()` classifies live, stranded-dead, and block-reclaimable content after flush.
- `spaceUsage()` classifies logical file bytes by manifest reachability. Its allocated-byte fields
  are `{ state: 'absent' }` on WASI, never a fabricated zero.

## No SQL here, on purpose

The query engine would dominate the artifact, and the two things an application does — a point
lookup and a page scan — are already served by the id order. Ids sort lexicographically **by
their UTF-8 bytes**, so designing them with the query in mind (`member/timestamp/...`) gives
prefix-then-time paging with no secondary index.

**Compare ids as bytes, not with JS `<`.** JS compares UTF-16 code units, and the two orders
disagree above the BMP: `'a\u{10000}'` sorts *below* `'a\uFFFF'` in JS and *above* it in UTF-8. For
ASCII ids they agree, so this only bites once an id carries an astral character — quietly, as a
wrong page boundary rather than an error. Use `prefixUpperBound` to build a range, or compare
`Buffer.from(id, 'utf8')`.

For analytics, the `turndb` CLI runs SQL against the same directory. No daemon, no second copy.

## License

Apache-2.0. See LICENSE and NOTICE.
