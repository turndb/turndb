// Ids and range bounds cross a UTF-16 → UTF-8 boundary. Every case here failed before the fix, and
// each fails for its own reason — the point of the suite is that a partial fix cannot pass it.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import './_artifact.mjs';
import { open, TurndbError, prefixUpperBound } from '../index.mjs';

async function withStore(fn) {
  const dir = join(await mkdtemp(join(tmpdir(), 'turndb-uni-')), 's.turndb');
  const s = await open(dir);
  try {
    return await fn(s);
  } finally {
    try {
      s.close();
    } catch {}
    await rm(dir, { recursive: true, force: true });
  }
}

test('distinct ids never alias onto one record', async () => {
  await withStore((s) => {
    // Both encode to `61 ef bf bd` through TextEncoder: the lone surrogate becomes U+FFFD. Before
    // the fix the second put SILENTLY OVERWROTE the first — a lost write, no error, in a store
    // whose cardinal invariant is byte-exact reconstruction.
    assert.throws(() => s.putBody('a\uD800', 'FIRST'), TurndbError, 'lone high surrogate must be refused');
    assert.throws(() => s.putBody('a\uDC00', 'SECOND'), TurndbError, 'lone low surrogate must be refused');

    // Refusal is total across the id surface, not just the write that happens to be checked.
    assert.throws(() => s.delete('a\uD800'), TurndbError);
    assert.throws(() => s.get('a\uD800'), TurndbError);
    assert.throws(() => s.getText('a\uD800'), TurndbError);
    assert.throws(() => s.getRecord('a\uD800'), TurndbError);
    assert.throws(() => s.applyBatch([{ id: 'a\uD800', body: 'x' }]), TurndbError);
    assert.throws(() => s.scanIds({ prefix: 'a\uD800' }), TurndbError);
    assert.throws(() => s.scanIds({ from: 'a\uD800' }), TurndbError);
    assert.throws(() => s.scanIds({ to: 'a\uD800' }), TurndbError);
    assert.throws(() => s.putBody('ok', 'x', [['k\uD800', 'v']]), TurndbError, 'attribute keys too');
    assert.throws(() => s.putBody('ok', 'x', [['k', 'v\uD800']]), TurndbError, 'and string values');
  });
});

test('refusing malformed input does not refuse the valid character it would have become', async () => {
  await withStore((s) => {
    // The mirror of the test above: over-refuse and you have broken U+FFFD, which is a perfectly
    // ordinary character an id may legitimately contain. A suite of only "rejects bad input"
    // assertions passes happily against an implementation that also rejects good input.
    s.putBody('a�', 'REPLACEMENT CHAR IS VALID');
    s.putBody('emoji/\u{1F600}', 'astral is valid');
    s.putBody('max/\u{10FFFF}', 'max scalar is valid');
    s.sync();
    assert.equal(s.getText('a�'), 'REPLACEMENT CHAR IS VALID');
    assert.equal(s.getText('emoji/\u{1F600}'), 'astral is valid');
    assert.equal(s.getText('max/\u{10FFFF}'), 'max scalar is valid');

    // And it stays distinct from the malformed input that used to collapse onto it.
    assert.throws(() => s.getText('a\uD800'), TurndbError);
  });
});

test('prefixUpperBound carries by code point, not code unit', () => {
  // Ordinary case.
  assert.equal(prefixUpperBound('alice/'), 'alice0');

  // U+FFFF: `fromCharCode(0xFFFF + 1)` wrapped to U+0000, producing a bound BELOW the prefix — an
  // inverted range that returned nothing at all, silently.
  assert.equal(prefixUpperBound('a￿'), 'a\u{10000}');
  // Compared as UTF-8 BYTES, which is the order the store sorts ids in. JS `>` compares UTF-16
  // code units, and the two orders disagree here: 'a\u{10000}' is a+D800DC00 and sorts BELOW
  // 'a\uFFFF' in JS while sorting above it in UTF-8. Asserting with `>` would have failed a
  // correct bound.
  const u8 = (x) => Buffer.from(x, 'utf8');
  assert.ok(Buffer.compare(u8(prefixUpperBound('a￿')), u8('a￿')) > 0, 'bound sorts above prefix');

  // Astral: bumping the last code UNIT mangles the surrogate pair. U+103FF is the case whose low
  // surrogate is DFFF, so +1 produced an unpaired surrogate that encoded to U+FFFD.
  assert.equal(prefixUpperBound('x\u{103FF}'), 'x\u{10400}');
  assert.ok(prefixUpperBound('x\u{103FF}').isWellFormed(), 'the bound must be well-formed UTF-16');
  assert.equal(prefixUpperBound('x\u{1F600}'), 'x\u{1F601}');

  // The surrogate hole is not a scalar range and must be stepped over.
  assert.equal(prefixUpperBound('a퟿'), 'a');

  // Trailing maximal scalars carry left.
  assert.equal(prefixUpperBound('a\u{10FFFF}'), 'b');
  assert.equal(prefixUpperBound('a\u{10FFFF}\u{10FFFF}'), 'b');

  // No valid string sorts above these prefix families: the bound is unbounded, not an error and
  // not an invented boundary.
  assert.equal(prefixUpperBound(''), null);
  assert.equal(prefixUpperBound('\u{10FFFF}'), null);
  assert.equal(prefixUpperBound('\u{10FFFF}\u{10FFFF}'), null);
});

test('the exported prefixUpperBound refuses malformed input rather than propagating it', () => {
  // It is exported and documented as contract, so a caller may build `from`/`to` with it directly.
  // Unguarded it carried a malformed prefix into a malformed bound — '\uD800' -> '\uD801', which
  // encodes to U+FFFD — reintroducing the wrong-boundary defect through the helper added to fix it.
  // scanIds guards too; that is defence in depth, not a substitute for guarding the entry point.
  assert.throws(() => prefixUpperBound('\uD800'), TurndbError, 'lone high surrogate');
  assert.throws(() => prefixUpperBound('\uDC00'), TurndbError, 'lone low surrogate');
  assert.throws(() => prefixUpperBound('a\uD800'), TurndbError, 'trailing lone high surrogate');
  assert.throws(() => prefixUpperBound('a\uDC00b'), TurndbError, 'interior lone low surrogate');

  // The mirror again: valid input, including the character malformed input would have become.
  assert.equal(prefixUpperBound('a\uFFFD'), 'a\uFFFE');
  assert.ok(prefixUpperBound('x\u{1F600}').isWellFormed(), 'a valid bound stays well-formed');
});

test('an inverted range refuses at the boundary and leaves the handle usable', async () => {
  await withStore((s) => {
    for (const id of ['a', 'b', 'c']) s.putBody(id, 'x');
    s.sync();

    // The engine refuses; the binding must surface that as a TurndbError carrying the engine's own
    // message. Before the guard this PANICKED in Rust, crossed as `RuntimeError: unreachable`, and
    // poisoned the handle — every later call failed with `RefCell already borrowed`.
    assert.throws(
      () => s.scanIds({ from: 'z', to: 'a' }),
      (e) => {
        assert.ok(e instanceof TurndbError, `expected TurndbError, got ${e?.constructor?.name}`);
        assert.match(e.message, /inverted/, `must carry the engine's message, got: ${e.message}`);
        return true;
      },
    );

    // The half that was actually lost: the store still works afterwards.
    assert.deepEqual(s.stats(), { parts: 0, records: 3 }, 'handle survives a refused range');
    assert.deepEqual(s.scanIds({ limit: 10 }), ['a', 'b', 'c'], 'and still pages');

    // Equal bounds are a legitimately empty half-open range, not an error.
    assert.deepEqual(s.scanIds({ from: 'b', to: 'b', limit: 10 }), []);

    // Astral vs BMP end to end: ordering is UTF-8 bytes all the way across the boundary. In JS
    // comparison the astral bound sorts BELOW the BMP one, so a binding that pre-checked with `>`
    // would accept this inverted pair and reject the valid one below it.
    assert.throws(() => s.scanIds({ from: 'a\u{10000}', to: 'a￿' }), TurndbError);
    assert.deepEqual(s.scanIds({ from: 'a￿', to: 'a\u{10000}', limit: 10 }), []);
  });
});

test('a non-string id is refused rather than coerced onto a colliding one', async () => {
  await withStore((s) => {
    // `#putText` used to encode `{}` as "[object Object]", colliding with the literal string of the
    // same name: three writes produced two records and one body was lost. Same silent overwrite as
    // the unpaired-surrogate case, through a different door — and that exact string is one a real
    // serialization bug in this codebase has already produced.
    assert.throws(() => s.putBody({}, 'x'), TurndbError, 'object id');
    assert.throws(() => s.putBody(42, 'x'), TurndbError, 'number id');
    assert.throws(() => s.get(null), TurndbError);
    assert.throws(() => s.delete(undefined), TurndbError);
    assert.throws(() => s.getRecord(['a']), TurndbError);

    // The literal string is a perfectly ordinary id and must still work — the mirror again.
    s.putBody('[object Object]', 'LITERAL');
    s.sync();
    assert.equal(s.getText('[object Object]'), 'LITERAL');
    assert.deepEqual(s.scanIds({ limit: 10 }), ['[object Object]']);

    // applyBatch keeps the ENGINE's error, which names the offending item's index — better than
    // anything the binding could say, so the strict check is deliberately not applied there.
    assert.throws(
      () => s.applyBatch([{ id: 'ok', body: 'a' }, { id: 42, body: 'b' }]),
      (e) => {
        assert.ok(e instanceof TurndbError);
        assert.match(e.message, /batch item 1/, `engine message expected, got: ${e.message}`);
        return true;
      },
    );
  });
});

test('an empty prefix scans everything rather than almost nothing', async () => {
  await withStore((s) => {
    for (const id of ['a', 'b', 'c']) s.putBody(id, 'x');
    s.sync();
    // Before the fix this built the range ['', U+0000) and returned zero rows — a plausible
    // answer, silently wrong, for what most callers read as "all ids".
    assert.deepEqual(s.scanIds({ prefix: '', limit: 50 }), ['a', 'b', 'c']);
    assert.deepEqual(s.scanIds({ limit: 50 }), ['a', 'b', 'c'], 'and matches an absent prefix');
  });
});

test('prefix ranges hold at the boundaries that used to invert or mangle', async () => {
  await withStore((s) => {
    s.putBody('a￿/1', 'in');
    s.putBody('a￿/2', 'in');
    s.putBody('b/1', 'out');
    s.putBody('x\u{103FF}/1', 'in');
    s.putBody('x\u{10400}/1', 'out');
    s.putBody('\u{10FFFF}/1', 'in');
    s.sync();

    assert.deepEqual(s.scanIds({ prefix: 'a￿', limit: 50 }), ['a￿/1', 'a￿/2']);
    assert.deepEqual(s.scanIds({ prefix: 'x\u{103FF}', limit: 50 }), ['x\u{103FF}/1']);
    // An unbounded-above prefix still excludes everything below it.
    assert.deepEqual(s.scanIds({ prefix: '\u{10FFFF}', limit: 50 }), ['\u{10FFFF}/1']);
  });
});
