---
default: minor
---

# The CLI speaks one layout, and old layouts keep one door

Every verb now takes a `.turndb` file. `import` creates the file if absent and leaves one file
behind; `seal` ships the committed snapshot as one sealed file (what `backup` produces, named for
what it does); `punch` performs both halves of in-place reclamation — dead content blocks under
the manifest's declaration, and free extents older than the retention window — and reports each;
`verify` walks member checksums, the retained chain, and every live part pin against the extents
the file actually holds.

Four verbs are gone. `pack` and `unpack`, because the pack is retired as an artifact — a sealed
container is the single-file archival form, and extraction has no successor (`cp` copies a
store). `checkpoint` and `write`, because ingesting into a file **is** the product now, and
`import` does it.

The retired layouts — store directories, sealed packs — keep exactly one door:

```sh
turndb convert mystore mystore.turndb      # a directory store, WAL settled, manifest verbatim
turndb convert snap.pack snap.turndb       # a pack, copied straight from its extents
```

Reading a retired layout with any other verb refuses and prints the convert line to run. The
library's directory-store constructors, the checkpoint bridge, and the pack write path remain
compiled for the transition, carry retirement notices, and leave with the bindings' rebase onto
the single-file store.
