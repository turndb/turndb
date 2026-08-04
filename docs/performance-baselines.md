# Performance baselines — real-corpus sample, 2026-08-04

These are the roadmap's first recorded baselines. **The corpus is an offline test sample of
unpublished data**: 22,392 real agent-trace records (7.57 GiB of message bodies, 8.13 GB as
JSONL) from a production system. It is not distributable, so these numbers are our measurements
in the sense the README already defines — the shape of the claim is reproducible against your own
traces; the exact figures are not reproducible without this corpus. Only aggregates appear here.

Environment: one Linux x86-64 workstation (not CI hardware), warm page cache, release build from
the 0.1.0 tree. Every turndb query figure is a complete CLI invocation — process start, store
open, query — so it overstates engine latency by roughly the ~20 ms spawn/open cost. Three runs
are shown where variance matters.

## Ingest and footprint

Import: 22,392 of 22,392 records in 259 s (86 rec/s single-threaded, durable WAL, per-batch
flush), zero refusals. Deep verify of the resulting store: every record, piece, frame, and pin in
7 s.

| State | Bytes on disk | vs. 8.13 GB input |
|---|---:|---:|
| As imported, 63 parts | 32,655,606 | 249× |
| After full compaction (3 s, zero content bytes touched) | 36,771,996 (transient: retention window) | — |
| After refold (8 s, 1.6 MB dead bytes reclaimed) | 28,700,974 | 283× |
| `pack` single file (3 s) | 28,700,780 | 283× |

The fold holds 49,113 unique pieces in 26.2 MB; parts and manifest are the remainder.

## Queries (SQL over the store, metadata-only unless stated)

| Query | Runs | Fold reads |
|---|---|---|
| `count(*)` over 22,392 | 154 ms cold, then 24/27 ms (63 parts); 22 ms (1 part) | 0 |
| group by `kind` | 75/32/31 ms | 0 |
| top-10 by `body_length` | 35/26/26 ms | 0 |
| attr filter, 11,182 matches | 25/24/25 ms | 0 |
| `session_id` correlation, 2,850 matches | 24/23/24 ms | 0 |
| range on `body_length`, 3,603 matches | 26/23/24 ms | 0 |
| substring over every body (7.57 GiB reconstructed, verified) | 19 s | 22,392 |
| point reconstruction of the largest record (3.3 MB), byte-exact | 97/98/104 ms | — |

Metadata searches sit flat at ~24 ms whole-CLI regardless of selectivity. The content scan is the
deliberate worst case: single-threaded reconstruction of the whole corpus through per-piece BLAKE3
verification at ~0.4 GB/s.

Lifecycle: writer open + inspect on 63 parts, 91 ms.

## The same corpus as Parquet

Written by pyarrow 24.0 from the identical serialized-body stream, on the same machine.

| Store | Bytes | vs. input | vs. turndb |
|---|---:|---:|---:|
| Parquet, zstd default, 2k-row groups (85 s) | 3,328,480,250 | 2.4× | 116× larger |
| Parquet, zstd-19, 4k-row groups (180 s) | 224,349,670 | 36× | 7.8× larger |
| turndb, compacted + refolded (259 s import) | 28,700,974 | 283× | — |

Parquet's in-process query timings (pyarrow, no process spawn): metadata count 2 ms, group-by
16 ms, range 1 ms, body substring scan 5 s. Every match count agreed exactly with turndb's answers
(11,182 / 2,850 / 3,603 / 17,043) — an incidental differential check across two unrelated engines.

Read this comparison honestly in both directions. Parquet at maximum compression scans raw content
3.8× faster (it decompresses without verifying content hashes) and its columnar metadata is
in-process-fast; it is the right export format, which is why the SQL surface streams Arrow. But
row-group-local compression cannot see resend redundancy that spans groups, which is most of this
corpus — that is the structural 7.8× — and the Parquet file is a frozen snapshot: no appends,
deletes, versioning, verification, or crash story, where turndb's smaller artifact is the live,
crash-simulated store those properties come from.

These figures are provisional in the same sense as the README's: one corpus, one machine, one day,
stated so a later, larger run can replace them rather than contradict them.
