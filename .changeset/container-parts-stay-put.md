---
default: minor
---

#### Opening a `.turndb` no longer copies its sealed parts

Opening a container for writing materialized every member into the working directory first, so
appending one record to a large store paid for a full copy of its history before the first write.

Parts are immutable once committed, and where they lie is placement rather than identity — the
manifest names them, and the read path has taken range readers rather than paths since packs
existed. So they stay in the container now and the writer reads them as extents. Only state a
session actually mutates has to become a file: the manifest, the WAL, and the live fold segment.

The working directory answers first for any name it holds, which is what makes an interrupted
session still resume correctly — a part beside the manifest is one that session rebuilt, and the
manifest commits to that copy.

Sealed fold segments are still copied out; that is the larger half and lands separately.
