---
default: minor
---

#### Opening a `.turndb` no longer copies its history

Opening a container for writing materialized every member into the working directory first, so
appending one record to a large store paid for a full copy of its history before the first write.

Parts and sealed fold segments are immutable once committed, so where they lie is placement rather
than identity — the manifest names them, and the read path has taken range readers rather than
paths since packs existed. They stay in the container now and the writer reads them as extents.
Only state a session actually mutates has to become a file.

What still materializes is the manifest, the dictionaries, the sidecars, and fold segments from the
committed tail's segment upward — that one because recovery truncates it, and any above it because
recovery unlinks those, and neither can be done to a member of a container. Everything below is
sealed by definition: the committed tail is strictly beyond it.

The working directory answers first for any name it holds, which is what makes an interrupted
session still resume correctly — a member beside the manifest is one that session rebuilt, and the
manifest commits to that copy.

The remaining copy is therefore bounded by the segment size rather than by the store. On a fixture
whose fold spans seven segments, a reopen copies one of them — 8,088 bytes of 80,616.
