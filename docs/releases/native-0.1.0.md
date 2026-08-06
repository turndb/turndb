# Native Node 0.1.0 release notes

This note is the source-side release record: `@turndb/native` 0.1.0 and its platform package were
published to npm on 2026-08-06.

This is TurnDB's first native Node distribution and the intended starting point for the first
production consumer's live storage integration. It is a pre-1.0 adoption release, not a format or
API freeze.

## Distribution

- Root package: `@turndb/native@0.1.0`
- Platform package: `@turndb/native-linux-x64-gnu@0.1.0`
- Supported target: Linux x86-64 with glibc 2.17 or newer
- Supported runtimes: Node `>=22 <27`, exercised on Node 22, 24, and 26
- Module systems: explicit CommonJS and ESM entry points, plus NodeNext-checked TypeScript declarations
- Installation: prebuilt N-API 6 addon; no Rust compiler, postinstall build, or WASI fallback

Both packages are private in source and ordinary packing remains private. The explicit release mode
creates publishable staging tarballs; the protected workflow install-tests their exact bytes on the
declared matrix and publishes with npm provenance.
See the [native prebuild and release contract](../native-prebuilds.md).

## Storage and query surface

The release exposes a domain-neutral, self-described record model:

- arbitrary ordered typed fields, including exact signed/unsigned integers, binary values, UTC
  nanosecond timestamps, explicit null, duplicate names, NaN, and negative zero;
- zero or more independently named content-addressed byte values per record;
- atomic ordered put/delete batches with explicit WAL durability acknowledgement;
- writer-visible structured scans with projection, typed predicates, checked keyset cursors, forward
  and reverse paging, exact work/I/O evidence, and bounded content reconstruction;
- feature-independent schema discovery;
- immutable current and retained snapshots;
- parameterized read-only SQL over the same newest-wins view with pull-based Arrow IPC;
- bounded queues, memory/work admission, absolute deadlines, AbortSignal cancellation, stable error
  codes, and explicit capability reporting;
- compaction, verification, content liveness, physical erasure, hole punching/refold, backup, restore,
  retained-manifest recovery, format migration, operational metrics, and lifecycle events.

Consumer vocabulary remains outside TurnDB. Activity, generation, tool, provider, and OpenTelemetry
concepts are ordinary consumer-selected field/content names rather than core schema.

## Compatibility

The writer emits format version 2. This release reads version 1 and can migrate it incrementally;
the retained legacy fixture is exercised through the public Node API. Native and portable WASI
builds have bidirectional byte-exact store interoperability where capabilities overlap.

Package and format versions are independent. During `0.x`, a minor release may advance the writer
format or make a documented API break. Patch releases do not intentionally remove documented APIs,
change stable error codes for the same typed condition, raise the runtime floor, or advance the
writer format. Downgrade writing is not promised. Consumers should retain backups and read release
notes before minor upgrades.

## Known limits

- There is only one qualified native platform package. macOS, musl, Windows, ARM, and other native
  targets are not implied by source portability.
- One OS-enforced writer may open a store; readers and immutable snapshots may run concurrently.
- The aggregate SQL reservation budget is not a complete process-RSS limit.
- A single part-encoding or zstd frame operation is not preemptible once entered; surrounding work
  has cooperative cancellation and admission limits.
- Some low-level invariant failures conservatively use `INTERNAL` until a typed engine cause proves a
  narrower class.
- The full SQL/Arrow addon is intentionally substantial: the dedicated-profile candidate measured
  77,852,768 bytes on the 2026-08-03 development host and packed to approximately 26.1 MB.

The support policy, operational constraints, and exact evidence tiers live in
[support and compatibility](../support-and-compatibility.md).
