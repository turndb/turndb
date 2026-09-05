# @turndb/cli

The [turndb](https://github.com/turndb/turndb) command line: inspect, verify, query, and ship a
store from a terminal with no server running.

```sh
npm install -g @turndb/cli
```

```sh
turndb --version                                # the crate version compiled in
turndb import  mystore.turndb traces.jsonl      # each line needs a "body"; other scalars become attributes
turndb inspect mystore.turndb                   # manifest, parts, fold, members, retained revisions
turndb verify  mystore.turndb --deep            # every record, piece, frame, and pin
turndb query   mystore.turndb "SELECT model, count(*) FROM t GROUP BY model"
turndb backup  mystore.turndb snap.turndb       # the current store authority as one self-contained store
turndb query   snap.turndb "SELECT count(*) FROM t"
```

`turndb help` prints the authoritative verb set; where it and this list disagree, it is right.

## What ships

A native binary, delivered through a per-platform package that npm installs as an optional
dependency and skips everywhere it does not apply. There is deliberately **no WASM fallback**: the
binary needs positioned reads and an OS-enforced writer lock. In-place `content-punch` and
`free-space-punch` use Linux hole punching or Windows sparse-range zeroing and report unsupported
on other platforms, so a platform
without a build says so rather than silently running a different engine with different guarantees.

Published slices: `linux-x64-gnu`, `linux-arm64-gnu`, `darwin-x64`, `darwin-arm64`, and
`win32-x64-msvc`. Other targets must build from source with `cargo install turndb`.

## Reading a store the library wrote

Every read verb accepts a current-draft `.turndb` store file. A store written by the
[`turndb`](https://www.npmjs.com/package/turndb)
wasm package or [`@turndb/native`](https://www.npmjs.com/package/@turndb/native) reads here
identically. Any other physical identity is refused. There is no converter, migration reader, or
fallback parser for discarded draft layouts.

Operating verbs (`compact` for part merge, `refold`, `content-punch`, `free-space-punch`, `erase`, `recover`
for manifest promotion, `reclaim`, and `backup`) take the writer role — `flock` on Unix or
`LockFileEx` on Windows, on the store container handle —
and close it when they finish, so a store they have operated on is exactly one file.

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
