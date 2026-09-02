---
default: patch
---

# Reclaim is one rename again everywhere but Windows

0.1.7 made `reclaim` publish through an anchor and a locked candidate copy on every platform, so
that one protocol could be proven under both durability models. That made Linux and macOS pay for a
Windows constraint: a second full copy of the compacted container per reclaim.

The platform layer now declares what each platform guarantees for a replace over an open
destination — atomic under POSIX `rename(2)`; lagged on Windows, whose documented route for
replacing an open file has no write-through form — and `reclaim` chooses its protocol by that
guarantee, never by platform name. Where the replace is atomic, the fresh container is writer-locked
under its staging name and put at the store's name by one rename: no copy, no anchor. Windows keeps
the anchor protocol. Recovery from an anchor runs on every platform, because an anchor is a file
beside its store and travels with it.

The deterministic simulator proves each protocol under the model of the guarantee it is specified
for, on every host, and shows the rename protocol losing the store under the Windows model — the
reason the choice is made by guarantee. The public `ReclaimProtocol` enum names the two protocols,
and `ReclaimProtocol::for_this_platform()` reports the choice. No format bytes change; a store
reclaimed by either protocol opens on every platform as before.
