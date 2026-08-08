# @turndb/cli

The [turndb](https://github.com/turndb/turndb) command line: inspect, verify, query, and ship a
store from a terminal with no server running.

```sh
npm install -g @turndb/cli
```

```sh
turndb import   mystore traces.jsonl     # each line needs a "body"; other scalars become attributes
turndb inspect  mystore                  # manifest, parts, fold, snapshots
turndb verify   mystore --deep           # every record, piece, frame, and pin
turndb query    mystore "SELECT model, count(*) FROM t GROUP BY model"
turndb checkpoint mystore mystore.turndb # the committed snapshot as one growable file
turndb inspect  mystore.turndb           # every read verb takes a directory, a pack, or a container
```

`turndb help` prints the authoritative verb set; where it and this list disagree, it is right.

## What ships

A native binary, delivered through a per-platform package that npm installs as an optional
dependency and skips everywhere it does not apply. There is deliberately **no WASM fallback**: the
binary needs positioned reads, `flock`, and — for `punch` — Linux hole punching, so a platform
without a build says so rather than silently running a different engine with different guarantees.

Published slices: `linux-x64-gnu`. On anything else, including Windows, use WSL or build from
source with `cargo install turndb`.

## Reading a store the library wrote

Every read verb accepts a store directory, a sealed `pack`, or a growable `.turndb` container, and
tells them apart by magic rather than by extension. A store written by the
[`turndb`](https://www.npmjs.com/package/turndb) wasm package or
[`@turndb/native`](https://www.npmjs.com/package/@turndb/native) reads here identically — the
format is one format.

Operating verbs (`compact`, `refold`, `punch`, `recover`, `erase`) take the writer role and so
require a store directory.

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
