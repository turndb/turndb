---
default: minor
---

# The Node binding opens the file

`open(path)` in the native Node binding now opens the single-file store directly: the path IS the
database, the write-ahead log and lock live beside it, and there is no prepared hot directory and
no fold-back step. `checkpointIntoContainer` is gone with the layout it served — a flush already
leaves the file current, so there is nothing left to checkpoint into. Backups restore through
member-verified copying of the sealed file (packs still restore by conversion, as the retired
artifacts they are), recovery reads the file's own manifest, and a missing backup source now
refuses as `NOT_FOUND` instead of a shrug about unrecognized layouts.

Stores created by earlier releases as directories are not opened by this binding any more; convert
them once with `turndb convert`.
