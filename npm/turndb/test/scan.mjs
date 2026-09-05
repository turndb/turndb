// Structured scan: engine-side predicates, projection, and checked cursors through the portable
// build. These assert COMPLETENESS — the exact row set, the exact attribute sequence, the whole
// paged concatenation — because a scan that returns *some* of the right answer is the failure this
// surface invites, and a presence assertion cannot see it.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import './_artifact.mjs';
import { open, TurndbError } from '../index.mjs';

async function withStore(fn, opts) {
  const dir = join(await mkdtemp(join(tmpdir(), 'turndb-scan-')), 's.turndb');
  const s = await open(dir, opts);
  try {
    return await fn(s, dir);
  } finally {
    try {
      s.close();
    } catch {}
    await rm(dir, { recursive: true, force: true });
  }
}

/** Ten records with a stable, deliberately non-uniform attribute shape. */
function seed(s) {
  for (let i = 0; i < 10; i++) {
    s.putBody(`m/alice/${String(i).padStart(3, '0')}`, `body-${i}`, {
      kind: i % 2 === 0 ? 'llm_exchange' : 'tool_action',
      ts: 1000 + i,
    });
  }
  // A second member, so a prefix range has to actually exclude something.
  s.putBody('m/bob/000', 'bob-body', { kind: 'llm_exchange', ts: 9999 });
  s.sync();
}

const ids = (page) => page.rows.map((r) => r.id);

test('projection returns exactly the requested attributes, in stored order, with duplicates', async () => {
  await withStore((s) => {
    // Order and duplicate keys are the thing byte-exactness depends on, so the array form is the
    // one under test. `skip` must not appear: a projection that returns everything also "contains"
    // what was asked for, and that is the bug this assertion exists to catch.
    s.putBody('r/1', 'x', [
      ['tag', 'a'],
      ['skip', 'no'],
      ['tag', 'b'],
      ['n', 7],
    ]);
    s.sync();

    const page = s.scan({ from: 'r/', attrs: ['tag', 'n'] });
    assert.equal(page.rows.length, 1);
    assert.deepEqual(page.rows[0].attrs, [
      ['tag', 'a'],
      ['tag', 'b'],
      ['n', 7n],
    ]);
    assert.equal(page.stats.duplicateAttrOccurrences, 1);

    // Selecting nothing returns no attributes rather than all of them.
    assert.deepEqual(s.scan({ from: 'r/' }).rows[0].attrs, []);
  });
});

test('a metadata-only page opens zero fold blocks, and a bytes page opens them', async () => {
  await withStore((s) => {
    seed(s);
    s.flush(); // published content has durable fold blocks

    const meta = s.scan({ prefix: 'm/alice/', contents: [{ name: 'body', mode: 'metadata' }] });
    assert.equal(meta.rows.length, 10);
    assert.equal(meta.stats.io.foldBlocksTouched, 0n, 'metadata must not reconstruct');
    assert.equal(meta.stats.contentValuesReconstructed, 0);
    assert.equal(meta.stats.reconstructedBytes, 0n);
    // Metadata still describes the value — length and identity without the bytes.
    assert.equal(meta.rows[0].contents.length, 1);
    assert.equal(meta.rows[0].contents[0].present, true);
    assert.equal(meta.rows[0].contents[0].len, 6n); // 'body-0'
    assert.equal(meta.rows[0].contents[0].bytes, undefined);
    assert.match(meta.rows[0].contents[0].identity, /^[0-9a-f]{64}$/);

    // The other half of the claim. Without this, an engine that never touches a fold block —
    // because `bytes` is broken — passes the assertion above.
    const bytes = s.scan({ prefix: 'm/alice/', contents: [{ name: 'body', mode: 'bytes' }] });
    assert.ok(bytes.stats.io.foldBlocksTouched > 0n, 'bytes mode must read the fold');
    assert.equal(bytes.stats.contentValuesReconstructed, 10);
    assert.equal(Buffer.from(bytes.rows[0].contents[0].bytes).toString(), 'body-0');
    assert.equal(bytes.stats.reconstructedBytes, 60n);
  });
});

test('identical bytes under different ids share one content identity', async () => {
  await withStore((s) => {
    s.putBody('a', 'same bytes');
    s.putBody('b', 'same bytes');
    s.putBody('c', 'other bytes');
    s.sync();
    const page = s.scan({ contents: [{ name: 'body', mode: 'metadata' }] });
    const identity = Object.fromEntries(page.rows.map((r) => [r.id, r.contents[0].identity]));
    assert.equal(identity.a, identity.b, 'same bytes must have the same identity');
    assert.notEqual(identity.a, identity.c);
  });
});

test('predicates filter to the exact set, and keep the row on the boundary', async () => {
  await withStore((s) => {
    seed(s);

    const exchanges = s.scan({
      prefix: 'm/alice/',
      predicates: [{ kind: 'attr', name: 'kind', op: 'eq', value: 'llm_exchange' }],
    });
    assert.deepEqual(ids(exchanges), [
      'm/alice/000',
      'm/alice/002',
      'm/alice/004',
      'm/alice/006',
      'm/alice/008',
    ]);

    // `gte` must ADMIT the boundary value. A filter that refuses one row too many passes any
    // assertion that only checks the excluded rows are gone.
    const from1005 = s.scan({
      prefix: 'm/alice/',
      predicates: [{ kind: 'attr', name: 'ts', op: 'gte', value: 1005 }],
    });
    assert.deepEqual(ids(from1005), [
      'm/alice/005',
      'm/alice/006',
      'm/alice/007',
      'm/alice/008',
      'm/alice/009',
    ]);

    // Two predicates AND together rather than replacing one another.
    const both = s.scan({
      prefix: 'm/alice/',
      predicates: [
        { kind: 'attr', name: 'kind', op: 'eq', value: 'tool_action' },
        { kind: 'attr', name: 'ts', op: 'gte', value: 1005 },
      ],
    });
    assert.deepEqual(ids(both), ['m/alice/005', 'm/alice/007', 'm/alice/009']);

    assert.deepEqual(
      ids(s.scan({ predicates: [{ kind: 'attr_exists', name: 'nothing', present: true }] })),
      [],
    );
    assert.equal(
      s.scan({ predicates: [{ kind: 'content_exists', name: 'body', present: true }], limit: 100 })
        .rows.length,
      11,
    );
  });
});

test('paging with a cursor covers the range exactly once', async () => {
  await withStore((s) => {
    seed(s);
    const expected = s.scan({ prefix: 'm/alice/', limit: 100 }).rows.map((r) => r.id);
    assert.equal(expected.length, 10, 'the one-page baseline must itself be complete');

    // limit 1 maximizes the number of cursor round-trips, which is where a page boundary that
    // drops or repeats a row shows up.
    const walked = [];
    let cursor;
    for (let guard = 0; guard < 50; guard++) {
      const page = s.scan({ prefix: 'm/alice/', limit: 1, cursor });
      walked.push(...page.rows.map((r) => r.id));
      if (page.next === undefined) break;
      cursor = page.next;
    }
    assert.deepEqual(walked, expected, 'paged traversal must equal the whole range, in order');
    assert.equal(new Set(walked).size, walked.length, 'no row may appear twice');
  });
});

test('reverse returns the same rows in the opposite order', async () => {
  await withStore((s) => {
    seed(s);
    const forward = ids(s.scan({ prefix: 'm/alice/', limit: 100 }));
    const reverse = ids(s.scan({ prefix: 'm/alice/', limit: 100, direction: 'reverse' }));
    assert.deepEqual(reverse, [...forward].reverse());
    assert.equal(reverse.length, 10);
  });
});

test('the memtable and the published columns answer identically', async () => {
  await withStore((s) => {
    seed(s);
    const request = {
      prefix: 'm/alice/',
      limit: 100,
      attrs: ['kind', 'ts'],
      contents: [{ name: 'body', mode: 'bytes' }],
      predicates: [{ kind: 'attr', name: 'ts', op: 'gte', value: 1004 }],
    };
    // Before the flush this reads the writer's memtable; after it, the physical columns. They are
    // different code paths and must not disagree.
    const beforeFlush = s.scan(request);
    s.flush();
    const afterFlush = s.scan(request);

    assert.equal(beforeFlush.rows.length, 6);
    assert.deepEqual(ids(afterFlush), ids(beforeFlush));
    assert.deepEqual(
      afterFlush.rows.map((r) => r.attrs),
      beforeFlush.rows.map((r) => r.attrs),
    );
    assert.deepEqual(
      afterFlush.rows.map((r) => Buffer.from(r.contents[0].bytes).toString()),
      beforeFlush.rows.map((r) => Buffer.from(r.contents[0].bytes).toString()),
    );
  });
});

test('a deleted row leaves the page rather than returning empty', async () => {
  await withStore((s) => {
    seed(s);
    s.delete('m/alice/004');
    s.sync();
    const page = s.scan({ prefix: 'm/alice/', limit: 100 });
    // Both halves matter: the tombstoned id is gone AND the other nine are still there.
    assert.equal(page.rows.length, 9);
    assert.ok(!ids(page).includes('m/alice/004'));
    assert.deepEqual(ids(page), [
      'm/alice/000',
      'm/alice/001',
      'm/alice/002',
      'm/alice/003',
      'm/alice/005',
      'm/alice/006',
      'm/alice/007',
      'm/alice/008',
      'm/alice/009',
    ]);
  });
});

test('an exhausted range returns an empty page with no cursor', async () => {
  await withStore((s) => {
    seed(s);
    const page = s.scan({ prefix: 'm/nobody/' });
    assert.deepEqual(page.rows, []);
    assert.equal(page.next, undefined);
    assert.equal(page.stats.returned, 0);
  });
});

test('malformed requests are refused, and the nearest valid one is still accepted', async () => {
  await withStore((s) => {
    seed(s);
    // Refuse: a silently-ignored misspelling is a caller who was told nothing went wrong.
    //
    // The two error types are the contract, not an accident: this wrapper validates the SHAPE it
    // has to understand to build the request at all (TypeError, matching how the package already
    // refuses a bad attribute), and the engine validates VALUES it alone defines (TurndbError).
    assert.throws(() => s.scan({ maxExamine: 5 }), TypeError, 'unknown field must refuse');
    assert.throws(() => s.scan({ predicates: [{ kind: 'nope', name: 'a', present: true }] }), TypeError);
    assert.throws(() => s.scan({ direction: 'sideways' }), TurndbError);
    assert.throws(() => s.scan({ contents: [{ name: 'body', mode: 'raw' }] }), TurndbError);
    assert.throws(
      () => s.scan({ predicates: [{ kind: 'attr', name: 'ts', op: 'approx', value: 1 }] }),
      TurndbError,
    );

    // Accept: every nearest-valid spelling of the things just refused. Without these, an
    // implementation that refuses ALL of them passes the block above.
    assert.equal(s.scan({ maxExamined: 5 }).stats.examined <= 5, true);
    assert.equal(s.scan({ direction: 'reverse' }).rows.length > 0, true);
    assert.equal(
      s.scan({ prefix: 'm/alice/', contents: [{ name: 'body', mode: 'metadata' }] }).rows.length,
      10,
    );
    assert.equal(
      s.scan({ predicates: [{ kind: 'attr_exists', name: 'ts', present: true }], limit: 100 })
        .rows.length,
      11,
    );
    assert.equal(
      s.scan({ predicates: [{ kind: 'attr', name: 'ts', op: 'eq', value: 1003 }] }).rows.length,
      1,
    );
    // The handle survives every refusal above.
    assert.equal(s.scan({ limit: 100 }).rows.length, 11);
  });
});

test('exact integer attributes survive the round trip beyond 2^53', async () => {
  await withStore((s) => {
    // JSON numbers cross JavaScript as f64. The ABI sends ints as decimal text precisely so this
    // value does not arrive rounded — and a predicate on it must compare the exact stored integer.
    const big = 9007199254740993n; // 2^53 + 1, not representable as a double
    s.putBody('big/1', 'x', { n: big });
    s.putBody('big/2', 'x', { n: big + 1n });
    s.sync();

    assert.deepEqual(s.scan({ from: 'big/', attrs: ['n'] }).rows[0].attrs, [['n', big]]);
    const hit = s.scan({ from: 'big/', predicates: [{ kind: 'attr', name: 'n', op: 'eq', value: big }] });
    assert.deepEqual(ids(hit), ['big/1'], 'must not match its neighbour through a rounded double');
  });
});

test('an id predicate narrows within the range', async () => {
  await withStore((s) => {
    seed(s);
    const page = s.scan({
      prefix: 'm/alice/',
      predicates: [{ kind: 'id', op: 'gt', value: 'm/alice/007' }],
    });
    assert.deepEqual(ids(page), ['m/alice/008', 'm/alice/009']);
  });
});
