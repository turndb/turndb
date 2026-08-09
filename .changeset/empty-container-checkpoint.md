---
default: patch
---

#### A store you create but never write to is still a store

Creating a `.turndb` and applying no records left a container holding no members at all — every
later command refused it with `container member not found: MANIFEST`, and the working directory
was left behind. Reaching it took nothing exotic: `turndb write new.turndb input.jsonl` where
every line of the input is skipped, which is what a mistyped schema or an empty file produces.

A directory store announces itself as new precisely by having no manifest on disk, and a store
that never applies a record never commits one. A container has no equivalent affordance — its
members *are* its state — so the checkpoint now writes the manifest it already holds rather than
looking for a file that was never going to exist. An empty container opens, scans to nothing, and
takes writes afterwards, exactly as the directory it mirrors always did.

Existing containers were never affected: a zero-record write into one committed cleanly and kept
its contents throughout.
