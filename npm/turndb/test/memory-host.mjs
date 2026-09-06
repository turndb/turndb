// The memory host: the same engine, the same `Store`, over an in-memory directory — and the bytes
// that come back out are a container the Node host and the CLI accept as their own.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import './_artifact.mjs';
import { openFile, TurndbError } from '../index.mjs';
import { MemoryFileSystem, MemoryHost } from '../memory.mjs';

const WASM = new URL('../turndb.wasm', import.meta.url);
const CLI = process.env.TURNDB_CLI ?? new URL('../../../target/debug/turndb', import.meta.url).pathname;

async function host() {
  return MemoryHost.create({ module: await readFile(WASM) });
}

const payload = (i) => JSON.stringify([{ role: 'user', content: `message ${i} ${'x'.repeat(200)}` }]);

test('the memory host writes, reads back byte-exact, verifies, and exports a settled container', async () => {
  const memory = await host();
  const caps = memory.capabilities();
  assert.equal(caps.profile, 'wasi');
  assert.equal(caps.host, 'memory');
  assert.equal(caps.writerExclusion, 'embedder_enforced');
  assert.equal(memory.compiledCapabilities().draft_format_epoch, 1);

  const store = memory.open('trace.turndb');
  assert.equal(memory.openHandles, 1);
  for (let i = 0; i < 300; i++) {
    store.write([{
      kind: 'put',
      id: `span/${String(i).padStart(4, '0')}`,
      contents: [{ name: 'gen_ai.input.messages', bytes: payload(i) }],
      attrs: [['otel.name', 'chat'], ['gen_ai.usage.input_tokens', i], ['gen_ai.usage.input_tokens', 7]],
    }]);
  }
  store.delete('span/0010');
  store.sync();
  store.flush();
  const page = store.scan({ prefix: 'span/', limit: 5, attrs: ['gen_ai.usage.input_tokens'] });
  assert.deepEqual(page.rows.map((row) => row.id), ['span/0000', 'span/0001', 'span/0002', 'span/0003', 'span/0004']);
  assert.deepEqual(page.rows[3].attrs, [['gen_ai.usage.input_tokens', 3n], ['gen_ai.usage.input_tokens', 7n]], 'duplicate occurrences and order survive');
  const record = store.getRecord('span/0123');
  assert.equal(new TextDecoder().decode(store.scan({ from: 'span/0123', limit: 1, contents: [{ name: 'gen_ai.input.messages', mode: 'bytes' }] }).rows[0].contents[0].bytes), payload(123));
  assert.equal(record, null, 'the body convenience name is not one of this record\'s contents');
  assert.equal(store.get('span/0010'), null, 'the tombstone resolves to absence');
  const report = store.verify();
  assert.equal(report.state, 'valid');
  assert.equal(report.records, 299);
  assert.equal(report.contentIdentities, 299);
  store.close();
  assert.equal(memory.openHandles, 0);

  // A closed store is settled: one container, no WAL beside it, no scratch directory.
  assert.deepEqual(memory.list(), ['trace.turndb']);
  const bytes = memory.read('trace.turndb');
  assert(bytes.byteLength > 8192, 'the container holds superblocks and members');
  assert.equal(new TextDecoder().decode(bytes.subarray(0, 8)), 'TDBDRFT1', 'the current container magic');

  // The Node host opens the exported bytes as an ordinary single-file store.
  const dir = await mkdtemp(join(tmpdir(), 'turndb-memory-host-'));
  try {
    const path = join(dir, 'exported.turndb');
    await writeFile(path, bytes);
    const file = await openFile(path);
    assert.equal(file.scanIds({ prefix: 'span/', limit: 1000 }).length, 299);
    assert.equal(
      new TextDecoder().decode(file.scan({ from: 'span/0299', limit: 1, contents: [{ name: 'gen_ai.input.messages', mode: 'bytes' }] }).rows[0].contents[0].bytes),
      payload(299),
      'byte-exact through the other host',
    );
    file.close();
    // And the native CLI verifies it deep: the memory host produced a genuine container, not a
    // file only its own instance can read.
    if (existsSync(CLI)) {
      execFileSync(CLI, ['verify', path, '--deep'], { stdio: 'pipe' });
    } else {
      throw new Error(`the turndb CLI is needed to cross-check the exported container; run bash npm/build.sh or set TURNDB_CLI (looked at ${CLI})`);
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('the memory host reopens its own container, reads a placed one, and keeps stores apart', async () => {
  const memory = await host();
  const first = memory.open('a.turndb');
  first.putBody('k', 'one');
  first.sync();
  first.flush();
  first.close();
  const again = memory.open('a.turndb');
  assert.equal(again.getText('k'), 'one', 'a reopened store recovers its own manifest');
  again.putBody('k', 'two');
  again.sync();
  // A second store in the same directory is independent while the first is still open.
  const other = memory.open('nested/b.turndb');
  other.putBody('k', 'other');
  other.sync();
  other.flush();
  assert.equal(again.getText('k'), 'two');
  assert.equal(other.getText('k'), 'other');
  assert.equal(memory.openHandles, 2);
  // `again` closes with an accepted, unpublished mutation: its WAL stays for replay, and the next
  // open recovers the value from it. `other` was flushed, so it settles to one file.
  again.close();
  other.close();
  assert.deepEqual(memory.list(), ['a.turndb', 'a.turndb-wal', 'nested/b.turndb']);
  const replayed = memory.open('a.turndb');
  assert.equal(replayed.getText('k'), 'two', 'the pending mutation replays from the retained WAL');
  replayed.flush();
  replayed.close();
  assert.deepEqual(memory.list(), ['a.turndb', 'nested/b.turndb'], 'settled once published');

  // Bytes placed by the host open read-only and refuse writes by name.
  const placed = await host();
  placed.write('copy.turndb', memory.read('nested/b.turndb'));
  const reader = placed.openFile('copy.turndb');
  assert.equal(reader.getText('k'), 'other');
  assert.throws(() => reader.putBody('k', 'nope'), (e) => e instanceof TurndbError && /read-only/.test(e.message));
  reader.close();
  assert.throws(() => placed.openFile('missing.turndb'), (e) => e instanceof TurndbError && e.code === 'NOT_FOUND');
  assert.throws(() => placed.open('../escape.turndb'), (e) => e instanceof TurndbError && e.code === 'INVALID_ARGUMENT');
});

test('the memory filesystem answers the WASI calls the engine makes with POSIX semantics', () => {
  const fs = new MemoryFileSystem();
  const memory = new WebAssembly.Memory({ initial: 1 });
  const wasi = fs.imports(() => memory);
  const view = () => new DataView(memory.buffer);
  const bytes = () => new Uint8Array(memory.buffer);
  const put = (at, text) => {
    const encoded = new TextEncoder().encode(text);
    bytes().set(encoded, at);
    return [at, encoded.byteLength];
  };
  const openFlags = (path, oflags, fdflags = 0) => {
    const [ptr, len] = put(1024, path);
    const errno = wasi.path_open(3, 0, ptr, len, oflags, 0n, 0n, fdflags, 2048);
    return [errno, view().getUint32(2048, true)];
  };
  // Exclusive creation refuses an existing name; plain creation reuses it; absence is NOENT.
  assert.equal(openFlags('s.turndb', 0)[0], 44);
  const [created, fd] = openFlags('s.turndb', 1 | 4);
  assert.equal(created, 0);
  assert.equal(openFlags('s.turndb', 1 | 4)[0], 20);
  // Positioned writes extend the file, zero-filling any gap; positioned reads are short at the end.
  put(4096, 'hello');
  view().setUint32(8192, 4096, true);
  view().setUint32(8196, 5, true);
  assert.equal(wasi.fd_pwrite(fd, 8192, 1, 10n, 8200), 0);
  assert.equal(view().getUint32(8200, true), 5);
  assert.equal(fs.read('s.turndb').byteLength, 15);
  assert.deepEqual([...fs.read('s.turndb').subarray(0, 10)], new Array(10).fill(0));
  view().setUint32(8192, 12288, true);
  view().setUint32(8196, 100, true);
  assert.equal(wasi.fd_pread(fd, 8192, 1, 10n, 8200), 0);
  assert.equal(view().getUint32(8200, true), 5, 'a read past the end is short, not an error');
  assert.equal(new TextDecoder().decode(bytes().subarray(12288, 12293)), 'hello');
  // Truncation and extension by set_size; filestat reports the new length.
  assert.equal(wasi.fd_filestat_set_size(fd, 3n), 0);
  assert.equal(wasi.fd_filestat_get(fd, 16384), 0);
  assert.equal(view().getBigUint64(16384 + 32, true), 3n);
  // A hard link names the same bytes; unlinking one name leaves the other and the open handle.
  const [a, al] = put(20480, 's.turndb');
  const [b, bl] = put(20600, 's.turndb-anchor');
  assert.equal(wasi.path_link(3, 0, a, al, 3, b, bl), 0);
  assert.equal(wasi.path_link(3, 0, a, al, 3, b, bl), 20, 'a link onto an existing name is EEXIST');
  assert.equal(wasi.path_unlink_file(3, a, al), 0);
  assert.deepEqual(fs.files(), ['s.turndb-anchor']);
  assert.equal(wasi.fd_filestat_get(fd, 16384), 0, 'the open descriptor outlives its unlinked name');
  // Directories: create, list with cookies, refuse removing a non-empty one, then remove.
  const [d, dl] = put(21000, 'work');
  assert.equal(wasi.path_create_directory(3, d, dl), 0);
  const [inner, il] = put(21100, 'work/spool');
  assert.equal(openFlags('work/spool', 1)[0], 0);
  assert.equal(wasi.path_remove_directory(3, d, dl), 55);
  assert.equal(wasi.path_unlink_file(3, inner, il), 0);
  assert.equal(wasi.path_remove_directory(3, d, dl), 0);
  assert.equal(wasi.fd_readdir(3, 24576, 4096, 0n, 28672), 0);
  const used = view().getUint32(28672, true);
  const names = [];
  for (let at = 24576; at < 24576 + used;) {
    const namlen = view().getUint32(at + 16, true);
    names.push(new TextDecoder().decode(bytes().subarray(at + 24, at + 24 + namlen)));
    at += 24 + namlen;
  }
  assert.deepEqual(names, ['.', '..', 's.turndb-anchor']);
  assert.equal(wasi.fd_close(fd), 0);
  assert.equal(wasi.fd_close(fd), 8, 'a closed descriptor is EBADF');
  assert.equal(wasi.fd_close(3), 0, 'closing the preopen is a no-op, as node:wasi treats it');
  assert.equal(wasi.fd_prestat_get(3, 0), 0);
  assert.equal(view().getUint32(4, true), '/store'.length);
});
