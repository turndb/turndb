---
default: minor
---

# Phase 3 gets one executable contract

TurnDB now carries versioned, machine-readable capability and structured-query contracts, an
independent semantic corpus, and a checked physical `.turndb` fixture. The Rust oracle replays the
write timeline and compares point reads, writer overlays, pinned snapshots, structured pages,
cursor refusals, exact content, work statistics, and the final container bytes. The native Node
package runs the same corpus through N-API and opens the same read-only fixture, so its binding tests
can no longer pass on a parallel hand-written notion of the API.

Python now has the same Tier-1 surface through a bounded single-owner PyO3 actor, including sealing
and maintenance. Node and Python share one normative OpenTelemetry span mapping and cadence policy.
Both exporters produce the same record ids, typed metadata, bulk gen_ai content, stable events, and
links for the language-neutral vector. Dependency-free client-call wrappers create those canonical
spans around synchronous and asynchronous provider SDK calls without changing their results.

The browser build opens the normal container through an arbitrary `ReadAt` source backed by bytes,
Blob/File, or strict HTTP Range. Structured predicates prune from part dictionaries and zone maps
before projection, and every binding reports the avoided rows. A checked, self-contained viewer
contains its wasm and JavaScript, replays the applicable shared corpus, and is exercised in Chromium
and Firefox through both local-file and static-host URL paths. SQL remains explicitly absent after a
documented DataFusion wasm build evaluation.

`capabilities()` adds the contract-v1 profile and operation set without removing its detailed native
facts. The shared corpus exposed a real loss at the JavaScript boundary: V8 canonicalized non-default
NaN payloads before Rust received them. Float attributes now accept an exact sixteen-digit lowercase
`floatBits` lane and emit it for NaNs; ordinary `floatValue` remains unchanged, and contradictory or
malformed dual representations refuse as `INVALID_ARGUMENT`.

Release automation now builds and smoke-tests Python wheels and a source distribution for PyPI,
and attaches the byte-rebuilt browser viewer to the matching GitHub release.
