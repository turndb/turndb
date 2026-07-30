# turndb

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

store.sync();    // the ACK point — durable from here
store.flush();   // seal into the columnar plane for other readers

store.get('alice/1700000000000/req-1#input');       // bytes, byte-exact
store.getText('alice/1700000000000/req-1#input');   // UTF-8 convenience; lossy on invalid bytes
store.scanIds({ prefix: 'alice/', limit: 50, reverse: true });
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

**Flush cadence is a compression dial.** Blocks sealed short compress worse. Measured on real trace
traffic, flushing every record gave 15×; every 50 gave 171×; every 512 gave 292×. Batch your
writes.

**Exclusion is yours to provide — this package cannot do it for you.** The native engine takes an
advisory `flock`, but **this package is always the `wasm32-wasip1` build**, on every host including
Linux and macOS, and WASI has no advisory locking. The lock file is created and gates nothing.

So the obligation is: **at most one open writer per store directory, across every process *and*
every instance or handle.** One process is not sufficient isolation — two `Store` handles in one
process can open the same directory. Two writers will interleave their write-ahead logs and corrupt
the store, and **detection is not guaranteed**: a clean read afterwards does not mean it is intact.

## Attributes keep order and duplicate keys

Byte-exact reconstruction depends on both, and a JS object can represent neither. Pass an object
for convenience, or an array of pairs when it matters:

```js
store.putBody(id, body, [['finishReason', 'end_turn'], ['finishReason', 'max_tokens']]);
```

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
