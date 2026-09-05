# Write admission limits

TurnDB bounds the amount of logical input one writer operation may admit before it changes the fold,
WAL, or pending change set. The policy is generic and per writer open. It does not know what an export, span,
generation, activity, tenant, or trace means.

## Policy

`WriteLimits` has four inclusive ceilings:

| limit | default | measures |
|---|---:|---|
| `max_record_bytes` | 64 MiB | worst-case complete WAL frame for one put or delete |
| `max_batch_bytes` | 256 MiB | all member frames plus the batch-completion frame |
| `max_batch_records` | 4,096 | ordered put/delete members in one atomic batch |
| `max_identifier_bytes` | 4 KiB | UTF-8 bytes in an id, attribute name, or content name |

All limits must be greater than zero in Rust and native Node. The portable API uses omitted options
for defaults and accepts positive u32 values because a WASI Preview 1 module has 32-bit linear-memory
addresses.

The settings are runtime admission policy, not format metadata. `Store::open` uses the defaults;
`Store::open_with_limits` accepts an explicit policy. Reopening a store with lower limits affects
future writes only. WAL replay always applies intact accepted frames without applying the current
open's limits, so an operator cannot make durable data unreadable by changing policy.

## The byte unit

A record is charged as if TurnDB encoded its complete current-draft WAL frame and every folded piece in
the input were novel. The count includes:

- the frame tag, sequence, payload length, and checksum;
- length prefixes, ids, field/content names, scalar tags, and scalar value bytes;
- content identity markers and 32-byte whole-value identities;
- literal content bytes and piece-reference hashes/lengths;
- a novel-piece hash, length, and bytes for every piece occurrence.

The batch byte charge is the sum of those member charges plus the framed varint completion marker. A
delete is its frame overhead plus the id bytes. The comparison is inclusive: a value exactly equal
to its ceiling is accepted.

Charging every piece occurrence as novel is deliberately conservative. It may exceed the bytes a
particular WAL append uses when a piece is already present or repeated within the request. In return,
admission is deterministic: the same request has the same charge in an empty store, after restart,
and after another record introduced matching content. A caller never needs access to TurnDB's dedup
history to decide how to split input.

This is an admission/work bound, not a claim that peak process memory or final disk usage equals the
charge. Bindings must first receive and decode their arguments, compression uses working memory, and
content already in the fold consumes no new content storage. Those are separately observable and
budgeted concerns.

## Validation and atomicity

Record ids, attribute names, and content names must be non-empty UTF-8 and fit the configured byte
limit. TurnDB reserves no punctuation or namespace: Unicode, dots, slashes, hashes, and
consumer-chosen conventions remain legal. Duplicate attribute names and their order remain exact.
Content names are map keys and therefore must be unique within one record.

Single writes are fully validated and charged before folding a piece. Atomic batches are completely
validated and charged before folding the first member. A rejected write changes no fold bytes,
dedup-window state, WAL bytes, pending record version, or visibility. A record-level refusal within a batch
reports its zero-based item index.

`WriteAdmissionError` separates caller mistakes from capacity refusals:

- invalid settings, empty/oversized identifiers, and duplicate content names are invalid arguments;
- oversized records, oversized batches, and excess batch members are resource exhaustion.

Native Node maps these to `TurnDbError` codes `INVALID_ARGUMENT` and `RESOURCE_EXHAUSTED`. The
portable package retains the engine's complete error text through `TurndbError`.

## Binding options

Native Node accepts exact byte ceilings as `bigint` and counts as `number`:

```js
const store = await NativeStore.open(path, {
  maxRecordBytes: 64n << 20n,
  maxBatchBytes: 256n << 20n,
  maxBatchRecords: 4096,
  maxIdentifierBytes: 4096,
});
```

The portable package accepts the same camel-case names as positive u32 `number` values. Both
capability profiles report that write admission is supported and publish the four compiled defaults.

Consumers should split batches on these generic limits before calling the engine when possible, then
treat a resource-exhausted refusal as authoritative. A consumer adapter may choose smaller limits or
additional domain rules; those do not belong in TurnDB's core.
