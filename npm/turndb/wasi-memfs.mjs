/**
 * A WASI Preview 1 host over an in-memory directory: the twenty-two imports the portable engine
 * needs, and nothing else.
 *
 * The engine is `wasm32-wasip1`; on Node the host is `node:wasi` over a real directory. A browser
 * tab, a Cloudflare Worker, Bun or Deno have no `node:wasi`, and the engine's write path wants a
 * filesystem: positioned reads and writes, truncation, sync, directory listing, hard links and
 * unlinks under one preopened directory. This module is that filesystem, in memory, with no
 * Node import, so the same `.wasm` writes a store anywhere JavaScript runs and the bytes come
 * back out as a `Uint8Array`.
 *
 * What it does not do is as deliberate as what it does: no clocks the engine did not ask for, no
 * `args`, no `path_rename` (the engine links and unlinks), no sockets, no symlinks. An import the
 * engine does not use is not stubbed; a call for one fails at instantiation, which is the honest
 * answer to an engine build that started needing more.
 *
 * Numbers below are the Preview 1 constants; the struct layouts are the ones `wasi_snapshot_preview1`
 * defines, written out at each site so a reader can check them against the spec.
 */

const ERRNO = Object.freeze({
  SUCCESS: 0,
  BADF: 8,
  EXIST: 20,
  INVAL: 28,
  IO: 29,
  ISDIR: 31,
  NOENT: 44,
  NOSYS: 52,
  NOTDIR: 54,
  NOTEMPTY: 55,
  PERM: 63,
  NOTCAPABLE: 76,
});

const FILETYPE = Object.freeze({ DIRECTORY: 3, REGULAR: 4 });

const OFLAGS = Object.freeze({ CREAT: 1, DIRECTORY: 2, EXCL: 4, TRUNC: 8 });
const FDFLAGS = Object.freeze({ APPEND: 1 });
/** Every right, so the guest's own rights checks never refuse an operation the host allows. */
const ALL_RIGHTS = 0xffff_ffff_ffff_ffffn;

/** A file node: growable bytes with an explicit length, shared by every name linked to it. */
class FileNode {
  constructor() {
    this.kind = FILETYPE.REGULAR;
    this.bytes = new Uint8Array(4096);
    this.length = 0;
    this.links = 0;
    this.ino = nextIno++;
  }

  reserve(length) {
    if (length <= this.bytes.byteLength) return;
    let capacity = this.bytes.byteLength;
    while (capacity < length) capacity *= 2;
    const grown = new Uint8Array(capacity);
    grown.set(this.bytes.subarray(0, this.length));
    this.bytes = grown;
  }

  writeAt(offset, chunk) {
    const end = offset + chunk.byteLength;
    this.reserve(end);
    if (offset > this.length) this.bytes.fill(0, this.length, offset);
    this.bytes.set(chunk, offset);
    if (end > this.length) this.length = end;
  }

  setLength(length) {
    if (length > this.length) {
      this.reserve(length);
      this.bytes.fill(0, this.length, length);
    }
    this.length = length;
  }

  /** The file's exact bytes, copied. */
  snapshot() {
    return this.bytes.slice(0, this.length);
  }
}

class DirNode {
  constructor() {
    this.kind = FILETYPE.DIRECTORY;
    this.entries = new Map();
    this.ino = nextIno++;
  }
}

let nextIno = 1;

class WasiExit extends Error {
  constructor(code) {
    super(`the WASI guest called proc_exit(${code})`);
    this.name = 'WasiExit';
    this.code = code;
  }
}

/**
 * One in-memory directory tree, mounted at `preopen` for one wasm instance.
 *
 * `files()`, `read()`, `write()` and `remove()` are the host-side door: what the guest wrote comes
 * out as bytes, and bytes the host places are what the guest opens.
 */
export class MemoryFileSystem {
  constructor({ preopen = '/store' } = {}) {
    this.preopen = preopen;
    this.root = new DirNode();
    this.fds = new Map();
    this.nextFd = 4;
    this.stdout = [];
    this.stderr = [];
    this.fdSyncCalls = 0;
    this.fds.set(3, { node: this.root, path: '', flags: 0, position: 0, preopen: true });
  }

  // ── host-side door ──────────────────────────────────────────────────────────

  /** Every file path under the root, sorted, relative to the preopen. */
  files() {
    const out = [];
    const walk = (dir, prefix) => {
      for (const [name, node] of [...dir.entries].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) {
        const path = prefix ? `${prefix}/${name}` : name;
        if (node.kind === FILETYPE.DIRECTORY) walk(node, path);
        else out.push(path);
      }
    };
    walk(this.root, '');
    return out;
  }

  /** The exact bytes of a file, copied, or `null` when absent. */
  read(path) {
    const node = this.#lookup(this.root, path);
    return node && node.kind === FILETYPE.REGULAR ? node.snapshot() : null;
  }

  /** Place a file; parents are created; an existing file is replaced. */
  write(path, bytes) {
    const { dir, name } = this.#parent(this.root, path, true);
    const node = new FileNode();
    node.writeAt(0, bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
    const previous = dir.entries.get(name);
    if (previous?.kind === FILETYPE.DIRECTORY) throw new Error(`${path} is a directory`);
    if (previous) previous.links -= 1;
    node.links = 1;
    dir.entries.set(name, node);
  }

  /** Remove a file or empty directory; absent is not an error. */
  remove(path) {
    const { dir, name } = this.#parent(this.root, path, false);
    const node = dir?.entries.get(name);
    if (!node) return false;
    if (node.kind === FILETYPE.DIRECTORY && node.entries.size) throw new Error(`${path} is not empty`);
    if (node.kind === FILETYPE.REGULAR) node.links -= 1;
    dir.entries.delete(name);
    return true;
  }

  /** Bytes the guest wrote to stdout and stderr, decoded. */
  output() {
    const decode = (chunks) => new TextDecoder().decode(concat(chunks));
    return { stdout: decode(this.stdout), stderr: decode(this.stderr) };
  }

  // ── path resolution ─────────────────────────────────────────────────────────

  #segments(path) {
    const text = typeof path === 'string' ? path : new TextDecoder().decode(path);
    let relative = text;
    if (relative.startsWith(this.preopen)) relative = relative.slice(this.preopen.length);
    return relative.split('/').filter((segment) => segment !== '' && segment !== '.');
  }

  #lookup(base, path) {
    let node = base;
    for (const segment of this.#segments(path)) {
      if (node.kind !== FILETYPE.DIRECTORY) return null;
      if (segment === '..') return null;
      node = node.entries.get(segment);
      if (!node) return null;
    }
    return node;
  }

  #parent(base, path, create) {
    const segments = this.#segments(path);
    if (segments.length === 0) return { dir: null, name: '' };
    let dir = base;
    for (const segment of segments.slice(0, -1)) {
      if (dir.kind !== FILETYPE.DIRECTORY) return { dir: null, name: segments.at(-1) };
      let next = dir.entries.get(segment);
      if (!next) {
        if (!create) return { dir: null, name: segments.at(-1) };
        next = new DirNode();
        dir.entries.set(segment, next);
      }
      dir = next;
    }
    return { dir: dir.kind === FILETYPE.DIRECTORY ? dir : null, name: segments.at(-1) };
  }

  // ── the WASI import object ──────────────────────────────────────────────────

  /**
   * The `wasi_snapshot_preview1` namespace bound to `getMemory()`, which is read on every call
   * because linear memory is replaced when it grows. Call `initialize(instance)` after
   * instantiation, as `node:wasi` requires, so the reactor's `_initialize` runs once.
   */
  imports(getMemory) {
    const memory = () => getMemory();
    const view = () => new DataView(memory().buffer);
    const bytes = () => new Uint8Array(memory().buffer);
    const text = (ptr, len) => new TextDecoder().decode(bytes().subarray(ptr, ptr + len));
    const fd = (number) => this.fds.get(number);
    const iovecs = (ptr, count) => {
      const dv = view();
      const out = [];
      for (let i = 0; i < count; i++) {
        out.push({ ptr: dv.getUint32(ptr + i * 8, true), len: dv.getUint32(ptr + i * 8 + 4, true) });
      }
      return out;
    };
    const filestat = (buf, node) => {
      const dv = view();
      dv.setBigUint64(buf, 1n, true);
      dv.setBigUint64(buf + 8, BigInt(node.ino), true);
      dv.setUint8(buf + 16, node.kind);
      dv.setBigUint64(buf + 24, BigInt(node.kind === FILETYPE.REGULAR ? Math.max(node.links, 1) : 1), true);
      dv.setBigUint64(buf + 32, BigInt(node.kind === FILETYPE.REGULAR ? node.length : 0), true);
      const now = BigInt(Date.now()) * 1_000_000n;
      dv.setBigUint64(buf + 40, now, true);
      dv.setBigUint64(buf + 48, now, true);
      dv.setBigUint64(buf + 56, now, true);
    };
    const fs = this;

    return {
      environ_sizes_get(countPtr, sizePtr) {
        const dv = view();
        dv.setUint32(countPtr, 0, true);
        dv.setUint32(sizePtr, 0, true);
        return ERRNO.SUCCESS;
      },
      environ_get() {
        return ERRNO.SUCCESS;
      },
      clock_time_get(id, _precision, timePtr) {
        const dv = view();
        const ns = id === 0
          ? BigInt(Date.now()) * 1_000_000n
          : BigInt(Math.round((globalThis.performance?.now?.() ?? Date.now()) * 1_000_000));
        dv.setBigUint64(timePtr, ns, true);
        return ERRNO.SUCCESS;
      },
      random_get(ptr, len) {
        const target = bytes().subarray(ptr, ptr + len);
        // Web Crypto caps one call at 65,536 bytes; the engine asks for hash keys, not buffers.
        for (let at = 0; at < len; at += 65_536) {
          globalThis.crypto.getRandomValues(target.subarray(at, Math.min(len, at + 65_536)));
        }
        return ERRNO.SUCCESS;
      },
      proc_exit(code) {
        throw new WasiExit(code);
      },
      fd_close(number) {
        const entry = fd(number);
        if (!entry) return ERRNO.BADF;
        if (entry.preopen) return ERRNO.SUCCESS;
        fs.fds.delete(number);
        return ERRNO.SUCCESS;
      },
      fd_sync(number) {
        if (!fd(number)) return ERRNO.BADF;
        fs.fdSyncCalls += 1;
        return ERRNO.SUCCESS;
      },
      fd_fdstat_get(number, buf) {
        const entry = fd(number);
        if (!entry) return ERRNO.BADF;
        const dv = view();
        // fdstat: filetype u8 @0, fdflags u16 @2, rights_base u64 @8, rights_inheriting u64 @16.
        dv.setUint8(buf, entry.node.kind);
        dv.setUint16(buf + 2, entry.flags, true);
        dv.setBigUint64(buf + 8, ALL_RIGHTS, true);
        dv.setBigUint64(buf + 16, ALL_RIGHTS, true);
        return ERRNO.SUCCESS;
      },
      fd_filestat_get(number, buf) {
        const entry = fd(number);
        if (!entry) return ERRNO.BADF;
        filestat(buf, entry.node);
        return ERRNO.SUCCESS;
      },
      fd_filestat_set_size(number, size) {
        const entry = fd(number);
        if (!entry) return ERRNO.BADF;
        if (entry.node.kind !== FILETYPE.REGULAR) return ERRNO.ISDIR;
        entry.node.setLength(Number(size));
        return ERRNO.SUCCESS;
      },
      fd_pread(number, iovsPtr, iovsLen, offset, nreadPtr) {
        const entry = fd(number);
        if (!entry) return ERRNO.BADF;
        if (entry.node.kind !== FILETYPE.REGULAR) return ERRNO.ISDIR;
        let at = Number(offset);
        let total = 0;
        const mem = bytes();
        for (const { ptr, len } of iovecs(iovsPtr, iovsLen)) {
          const take = Math.max(0, Math.min(len, entry.node.length - at));
          mem.set(entry.node.bytes.subarray(at, at + take), ptr);
          at += take;
          total += take;
          if (take < len) break;
        }
        view().setUint32(nreadPtr, total, true);
        return ERRNO.SUCCESS;
      },
      fd_pwrite(number, iovsPtr, iovsLen, offset, nwrittenPtr) {
        const entry = fd(number);
        if (!entry) return ERRNO.BADF;
        if (entry.node.kind !== FILETYPE.REGULAR) return ERRNO.ISDIR;
        let at = Number(offset);
        let total = 0;
        const mem = bytes();
        for (const { ptr, len } of iovecs(iovsPtr, iovsLen)) {
          entry.node.writeAt(at, mem.subarray(ptr, ptr + len));
          at += len;
          total += len;
        }
        view().setUint32(nwrittenPtr, total, true);
        return ERRNO.SUCCESS;
      },
      fd_read(number, iovsPtr, iovsLen, nreadPtr) {
        const entry = fd(number);
        if (!entry) return ERRNO.BADF;
        if (entry.node.kind !== FILETYPE.REGULAR) return ERRNO.ISDIR;
        let total = 0;
        const mem = bytes();
        for (const { ptr, len } of iovecs(iovsPtr, iovsLen)) {
          const take = Math.max(0, Math.min(len, entry.node.length - entry.position));
          mem.set(entry.node.bytes.subarray(entry.position, entry.position + take), ptr);
          entry.position += take;
          total += take;
          if (take < len) break;
        }
        view().setUint32(nreadPtr, total, true);
        return ERRNO.SUCCESS;
      },
      fd_write(number, iovsPtr, iovsLen, nwrittenPtr) {
        const mem = bytes();
        let total = 0;
        if (number === 1 || number === 2) {
          for (const { ptr, len } of iovecs(iovsPtr, iovsLen)) {
            (number === 1 ? fs.stdout : fs.stderr).push(mem.slice(ptr, ptr + len));
            total += len;
          }
          view().setUint32(nwrittenPtr, total, true);
          return ERRNO.SUCCESS;
        }
        const entry = fd(number);
        if (!entry) return ERRNO.BADF;
        if (entry.node.kind !== FILETYPE.REGULAR) return ERRNO.ISDIR;
        if (entry.flags & FDFLAGS.APPEND) entry.position = entry.node.length;
        for (const { ptr, len } of iovecs(iovsPtr, iovsLen)) {
          entry.node.writeAt(entry.position, mem.subarray(ptr, ptr + len));
          entry.position += len;
          total += len;
        }
        view().setUint32(nwrittenPtr, total, true);
        return ERRNO.SUCCESS;
      },
      fd_prestat_get(number, buf) {
        const entry = fd(number);
        if (!entry?.preopen) return ERRNO.BADF;
        const dv = view();
        // prestat: tag u8 @0 (0 = directory), pr_name_len u32 @4.
        dv.setUint8(buf, 0);
        dv.setUint32(buf + 4, new TextEncoder().encode(fs.preopen).byteLength, true);
        return ERRNO.SUCCESS;
      },
      fd_prestat_dir_name(number, pathPtr, pathLen) {
        const entry = fd(number);
        if (!entry?.preopen) return ERRNO.BADF;
        const name = new TextEncoder().encode(fs.preopen);
        if (pathLen < name.byteLength) return ERRNO.INVAL;
        bytes().set(name, pathPtr);
        return ERRNO.SUCCESS;
      },
      fd_readdir(number, buf, bufLen, cookie, bufUsedPtr) {
        const entry = fd(number);
        if (!entry) return ERRNO.BADF;
        if (entry.node.kind !== FILETYPE.DIRECTORY) return ERRNO.NOTDIR;
        const names = [...entry.node.entries.keys()].sort();
        const listing = [
          ['.', entry.node],
          ['..', entry.node],
          ...names.map((name) => [name, entry.node.entries.get(name)]),
        ];
        const mem = bytes();
        const dv = view();
        let used = 0;
        for (let index = Number(cookie); index < listing.length; index++) {
          const [name, node] = listing[index];
          const encoded = new TextEncoder().encode(name);
          // dirent: d_next u64 @0, d_ino u64 @8, d_namlen u32 @16, d_type u8 @20; 24 bytes, then the name.
          const header = new Uint8Array(24);
          const hv = new DataView(header.buffer);
          hv.setBigUint64(0, BigInt(index + 1), true);
          hv.setBigUint64(8, BigInt(node.ino), true);
          hv.setUint32(16, encoded.byteLength, true);
          hv.setUint8(20, node.kind);
          const record = concat([header, encoded]);
          const room = bufLen - used;
          if (room <= 0) break;
          const take = Math.min(room, record.byteLength);
          mem.set(record.subarray(0, take), buf + used);
          used += take;
          if (take < record.byteLength) break;
        }
        dv.setUint32(bufUsedPtr, used, true);
        return ERRNO.SUCCESS;
      },
      path_create_directory(dirfd, pathPtr, pathLen) {
        const entry = fd(dirfd);
        if (!entry) return ERRNO.BADF;
        if (entry.node.kind !== FILETYPE.DIRECTORY) return ERRNO.NOTDIR;
        const path = text(pathPtr, pathLen);
        // `create_dir_all` on the preopen itself asks to create "." — it exists, which is what
        // a POSIX mkdir says about it, and what lets the caller go on to check it is a directory.
        if (fs.#segments(path).length === 0) return ERRNO.EXIST;
        const { dir, name } = fs.#parent(entry.node, path, false);
        if (!dir) return ERRNO.NOENT;
        if (dir.entries.has(name)) return ERRNO.EXIST;
        dir.entries.set(name, new DirNode());
        return ERRNO.SUCCESS;
      },
      path_filestat_get(dirfd, _flags, pathPtr, pathLen, buf) {
        const entry = fd(dirfd);
        if (!entry) return ERRNO.BADF;
        const node = fs.#lookup(entry.node, text(pathPtr, pathLen));
        if (!node) return ERRNO.NOENT;
        filestat(buf, node);
        return ERRNO.SUCCESS;
      },
      path_link(oldFd, _oldFlags, oldPtr, oldLen, newFd, newPtr, newLen) {
        const from = fd(oldFd);
        const to = fd(newFd);
        if (!from || !to) return ERRNO.BADF;
        const node = fs.#lookup(from.node, text(oldPtr, oldLen));
        if (!node) return ERRNO.NOENT;
        if (node.kind !== FILETYPE.REGULAR) return ERRNO.PERM;
        const { dir, name } = fs.#parent(to.node, text(newPtr, newLen), false);
        if (!dir) return ERRNO.NOENT;
        if (dir.entries.has(name)) return ERRNO.EXIST;
        node.links += 1;
        dir.entries.set(name, node);
        return ERRNO.SUCCESS;
      },
      path_open(dirfd, _dirflags, pathPtr, pathLen, oflags, _rightsBase, _rightsInheriting, fdflags, openedFdPtr) {
        const entry = fd(dirfd);
        if (!entry) return ERRNO.BADF;
        if (entry.node.kind !== FILETYPE.DIRECTORY) return ERRNO.NOTDIR;
        const path = text(pathPtr, pathLen);
        const segments = fs.#segments(path);
        let node;
        if (segments.length === 0) {
          node = entry.node;
        } else {
          const { dir, name } = fs.#parent(entry.node, path, false);
          if (!dir) return ERRNO.NOENT;
          node = dir.entries.get(name);
          if (node && oflags & OFLAGS.EXCL && oflags & OFLAGS.CREAT) return ERRNO.EXIST;
          if (!node) {
            if (!(oflags & OFLAGS.CREAT)) return ERRNO.NOENT;
            node = new FileNode();
            node.links = 1;
            dir.entries.set(name, node);
          }
        }
        if (oflags & OFLAGS.DIRECTORY && node.kind !== FILETYPE.DIRECTORY) return ERRNO.NOTDIR;
        if (oflags & OFLAGS.TRUNC && node.kind === FILETYPE.REGULAR) node.setLength(0);
        const number = fs.nextFd++;
        fs.fds.set(number, { node, path, flags: fdflags & FDFLAGS.APPEND, position: 0, preopen: false });
        view().setUint32(openedFdPtr, number, true);
        return ERRNO.SUCCESS;
      },
      path_remove_directory(dirfd, pathPtr, pathLen) {
        const entry = fd(dirfd);
        if (!entry) return ERRNO.BADF;
        const { dir, name } = fs.#parent(entry.node, text(pathPtr, pathLen), false);
        const node = dir?.entries.get(name);
        if (!node) return ERRNO.NOENT;
        if (node.kind !== FILETYPE.DIRECTORY) return ERRNO.NOTDIR;
        if (node.entries.size) return ERRNO.NOTEMPTY;
        dir.entries.delete(name);
        return ERRNO.SUCCESS;
      },
      path_unlink_file(dirfd, pathPtr, pathLen) {
        const entry = fd(dirfd);
        if (!entry) return ERRNO.BADF;
        const { dir, name } = fs.#parent(entry.node, text(pathPtr, pathLen), false);
        const node = dir?.entries.get(name);
        if (!node) return ERRNO.NOENT;
        if (node.kind === FILETYPE.DIRECTORY) return ERRNO.ISDIR;
        node.links -= 1;
        dir.entries.delete(name);
        return ERRNO.SUCCESS;
      },
    };
  }
}

function concat(chunks) {
  const total = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
  const out = new Uint8Array(total);
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.byteLength;
  }
  return out;
}

export { ERRNO, WasiExit };
