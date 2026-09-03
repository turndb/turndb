# TurnDB roadmap

**Status: active, 2026-08-15; realigned 2026-08-30 against the surveyed state of `50832b9`** (obj-mtfdibju-2).
The previous roadmap was deleted at 0.1.0 because its six gates had been exercised and it described a
sprint, not a direction. This one states the direction. Each phase now carries a status line that says
what has shipped and what its gate still lacks, so the document claims exactly what is true.

## The thesis

TurnDB's target is to be **the database for AI traces** — LLM requests and agent activity, the whole
record of what an agent system did — embedded like SQLite, portable as one file, and queryable
wherever that file goes, including a browser tab with no server behind it. A centralized deployment
is a *consumer* of the engine (a server that embeds it), never a property of the engine itself.

This narrows the previous roadmap's aim ("a credible, general-purpose, embedded, content-addressed
columnar store") without reversing its architecture. The layering holds: the on-disk format and the
engine stay domain-generic — no trace vocabulary in FORMAT.md, ever — and the trace product is built
*on* that engine as a first-party adapter, importer set, and viewer. Generality below, opinion above.
What changes is which layer we invest in: the generic engine is no longer the product; the trace
experience is.

The claim that has to stay true through every phase is the cardinal invariant: byte-exact
reconstruction, and dedup ratios that come from carving at the boundaries where trace traffic
actually repeats.

## Why this order

The phases are gates, not dates, and the order is forced by dependencies:

1. The **container** must finish first, because every later phase hands someone a `.turndb` file and
   calls it the store. Publishing benchmark numbers against a layout we intend to replace means
   re-proving twice.
2. The **proof** comes second, because the offline runs on public datasets already happened — what's
   missing is the reproducible, published form, and it should measure the layout we're keeping.
3. The **interfaces** come third, split in two. First the SDK baseline — one capability
   contract, one serializable query contract, one trace-mapping spec, each binding a thin shell
   over them, Node ahead of Python because CommandSuite is the first production consumer. Then
   the **browser**, which is deliberately not a new API: the same contract over one more
   transport, reading what phases 1–2 produced. Its remote read pattern (range requests against
   a CDN) must still inform container layout *before* it calcifies — which is why Phase 1
   carries a read-locality deliverable on the browser's behalf.
4. The **trace vocabulary** and the **server** close the loop: raw logs in at one end, a shareable
   queryable file at the other.

Work may overlap; gates may not be reordered.

## The entrances

The product has several entrances, and they serve different people. The CLI is the **operator's
porch** — evaluate, inspect, back up, recover; "point turndb at your traces and measure" lives
there. The viewer is the **reader's porch**: a link someone opens. The server is the
**deployment's porch**. The entrance that decides adoption is none of these: it is the **SDKs** —
npm install, pip install, open a path, write — the way SQLite is adopted, by being everywhere and
boring to integrate. Node leads because CommandSuite, the first production consumer, is
TypeScript; Python follows because the agent ecosystem's center of gravity lives there. Neither
may grow a bespoke surface. Every binding is a thin shell over one baseline:

* one **capability contract** — what every Tier-1 binding exposes: open/put/sync/flush,
  structured scan, content reads, backup, maintenance, typed error classes, explicit capability
  reporting where a platform gives something up;
* one **query contract, serializable as data** — the structured scan request and its Arrow IPC
  and row results, the same shape in-process, over N-API, over wasm, over HTTP. SQL stays an
  optional lens where DataFusion fits, never the hot-path contract;
* one **trace-mapping spec** — the gen_ai/agent-activity mapping defined once, implemented by
  every tracer and importer rather than reinvented per language.

Two tiers per language. **Tier 1 is the store API** — explicit records, explicit durability, for
people building trace systems. **Tier 2 is the market's expected motion — wrap or export**: an
OpenTelemetry span exporter that writes a `.turndb` file, thin client wrappers, and cadence
policy. A tracer cannot ask its host to call flush, so batching and flush policy live in Tier 2,
never in the engine: the engine's explicitness is its honesty; the SDK's policy is its
ergonomics. The pitch this tier carries against the incumbents is the file itself — they need a
server and an account; this is two lines and a local file with the collapse ratios on the label.

Release machinery changes only when a phase demands it — new artifacts, new SDK packaging, a
renamed surface — never as its own workstream. The full freeze is a 1.0 property, not a today
property. The open meta-issues were triaged once, on 2026-08-30, in the survey report under
obj-mtfdibju-2 (§4); the dispositions there are the ones this roadmap assumes.

## Every OS a consumer runs on

**Requirement, 2026-08-30 (Andrew):** TurnDB supports every operating system a consumer runs on —
Linux x64 and arm64, macOS x64 and arm64, Windows x64 — with native packages where they are built
and the portable package everywhere else, its capability difference stated where the consumer
chooses. A consumer embedding TurnDB must never inherit an OS restriction from it: CommandSuite's
adoption cannot cost CommandSuite a platform.

What exists today, by registry — re-surveyed 2026-09-02 against npm, crates.io and PyPI at 0.1.8 — so
this claims exactly what is true:

| slice | `@turndb/native` | `@turndb/cli` | Python wheels | portable `turndb` (wasm) |
|---|---|---|---|---|
| Linux x64 glibc | 0.1.8 | 0.1.8 | 0.1.8 (cp39–cp313, manylinux_2_17) | yes |
| Linux arm64 | open | 0.1.8 — published cross-built and never executed; since 2026-09-02 built on `ubuntu-22.04-arm`, installed and driven there, with the engine suite, the crash sweeps and the cross-architecture byte-compare as required gates | open | yes |
| macOS x64 | open | 0.1.8 — built and driven on `macos-15-intel`; the engine suite does not run on macOS | open | yes |
| macOS arm64 | open | 0.1.8 — built and driven on `macos-15`; the engine suite does not run on macOS | open | yes |
| Windows x64 | 0.1.8 | 0.1.8 | 0.1.8 (cp39–cp313, win_amd64) | yes |

The portable package runs on every host Node 22–26 does, and gives up exactly three things —
advisory locking, in-place punch, threads — which the capability contract reports and the front
door must state. Native slices are Phase 3a-i work ("ours to meet"); the Windows row is engine
work before it is packaging work; no dates are implied by this table.

**Platform rule, 2026-09-02 (Andrew):** a constraint one platform has is that platform's protocol,
never every platform's. `src/sys.rs` declares what each platform *guarantees* — the first such fact
is `replace_open_durability`: atomic under POSIX `rename(2)`, lagged on Windows — and a protocol
chooses its steps by the guarantee, never by `cfg!(windows)`. The simulator proves each protocol
under the model of the guarantee it is specified for, on every host, so a split never halves the
proof, and it shows the cheaper protocol failing under the stricter model, so the choice is
evidence rather than caution. First application: `reclaim` is one rename again everywhere but
Windows; 0.1.7 had made every platform pay the anchor protocol's extra copy of the compacted
container to keep one protocol (FORMAT.md, "Free space"). The 0.1.7 decision was not recorded
here when it was made; this paragraph records both.

## Phase 0: the front door tells the truth

**Status: closed 2026-08-30** (obj-mtfi3akf-5). Inserted ahead of every other phase because it is what
a stranger meets first: the survey found the README's first store command failing on its bare path, `inspect` and
`verify` then passing misleadingly on the empty store it left, the remaining four commands failing,
and the `seal` step leaving debris beside the store. Every item below landed as its own reviewed PR:
#128 (#121), #127 (#122), #133 (#120), #129 (#97), #131 (#102), #130 (the four document corrections),
#132 (#118's nightly catch — the publish-time restore remains #118's open tracker, a release-gate
decision).

This phase is finite and boring on purpose. It is not a quality sweep; it is the list of places where
what we ship and what we say diverge, closed one by one:

- `turndb import mystore.turndb -` — the README's first command — fails on a bare relative path and
  leaves an empty store and a WAL (#121).
- `seal`, `compact`, `refold`, `punch`, `erase` leave a `<store>-wal` beside a cleanly operated store,
  against FORMAT.md's own promise (#122).
- The published `@turndb/cli` README documents a verb and a store shape the binary refuses (#120);
  the CLI cannot report its version (#97).
- The native Node suite fails on a developer's box after the documented `npm ci` and passes in CI only
  because that job never installs the optional platform package (#102); the test establishes its own
  no-artifact condition.
- The README's record model ("three records per call because a record has one body") predates
  format version 2's named content, which the trace mapping already uses; one mapping, stated once.
- `docs/support-and-compatibility.md` says the release-profile suite is verified only locally; CI has
  run it hosted and green since the repository went public, and the private-tier skip is dead code.
- The Python package ships without the columnar/SQL lens and without cancellation, and the portable
  package without advisory locking, punch, or threads, and no document says either in the words a
  consumer choosing a package would read; the support policy and each package README state the
  capability difference explicitly.
- `docs/embedding-contract.md` — the consumer's design surface — is linked from no current document;
  its sole inbound reference is the 0.1.0 CHANGELOG entry.
- Release preparation writes versioned unresolved optional lock entries for native platform
  packages. The same entries remain valid before and after first publication, so publishing cannot
  silently invalidate main's next `npm ci`; the nightly job repeats that clean install on main.

### Maturity gate

A stranger follows the README on a clean machine and every command does what the README says and
leaves what the README says it leaves. Every registry description and README describes the artifact
behind it. A publication cannot leave main displaying a green it would no longer earn. Phase 2's harness
runs against this quickstart, not a corrected one.

## Phase 1: one file is the store

**Status: complete, 2026-08-15 — with one deliverable landed differently than described below, recorded
2026-08-30.** The maturity gate is executable across the native file lifecycle, crash and corruption
harnesses, binding parity, and the cold-open positioned-read bound below. The commit authority did not
become "superblock states plus a commit journal": the manifest is restaged as JSON members
(`MANIFEST`, `MANIFEST.<commit>`) on every flush and published by the superblock flip — a port of the
manifest-file design into the container, not the redesign the deliverable text asks for. It is correct,
crash-proven by the DST container sweeps, and it is what FORMAT.md now specifies. Whether the redesign
is still wanted is an open decision; it is not a completed one, and this paragraph exists so nobody
reads the deliverable below as done in the form it states.

The container (FORMAT.md, "The container") already holds what a pack holds and grows past it under
alternating superblocks. But the current writer treats it as a checkpoint target: `ContainerStore`
materializes working state into a hot *directory*, runs the directory engine there, and folds the
result back in at checkpoint — a pack while closed, a directory while open. That was the right
bridge to prove the format. It is not the model.

The model is SQLite's, taken literally: **the file is the live database, and what sits beside it
while hot is flat and few.** `mystore.turndb` holds the data plane at every moment a writer runs —
parts and fold segments are appended *into it* past the committed tail, and a commit is a
superblock flip, which is the crash model the alternating slots were designed for: an interrupted
write lands in bytes no committed superblock refers to. Beside it while open: `mystore.turndb-wal`
(append and fsync, replayed on open, settled and removed on clean close) and writer exclusion by
`flock` on the main file itself. A cleanly closed store is exactly one file. The materialize/
fold-back cycle and its checkpoint tax do not survive this phase.

A doctrine for the cutting: **the model is the authority, and no existing code has seniority
against it.** Where the tree assumes a rename, a manifest file, a directory fsync, or a segment
with its own directory entry, the assumption is displaced, not accommodated. The read side was
born ready — every offset in a part or fold segment is artifact-relative, and `readat.rs` never
learns where bytes live — so honesty to the model is a write-side demolition, and it is cheaper
now than it will ever be again. The single-file live store is the differentiation; it does not get
traded away to keep code we already have.

The companion decision follows from the doctrine: **the directory store ceases to be a layout at
all.** Not demoted to internal — displaced. The WAL sidecar is a flat file, not a directory store
in disguise; the hot-directory bridge is deleted once the native path passes its sweeps; and every
surface that lets a user hold a store as a directory goes with it. One file, one story, before
anything is published against it.

### Deliverables

- **The native single-file write path.** Flush seals the memtable into a part written directly
  into the container; fold segments grow the same way; the manifest's authority moves into the
  superblock flip. Rename-and-dir-fsync publication is replaced by append-past-tail plus slot
  alternation. Flush *is* the checkpoint — there is no other.
- **The commit authority moves, and everything that leaned on manifest files moves with it.**
  `MANIFEST.NNNNNNNN`, the checksummed commit log, retained commits, reader-pinned snapshots,
  recovery evidence, and rollback authorization are all manifest-*file* concepts today. In-file
  they become superblock states plus a commit journal held as a member — a redesign, not a port.
  Two design documents open this phase and are settled before code: where retained commits live
  when there are no manifest files, and how in-place punching respects extents an older retained
  state still pins. `recover` becomes superblock recovery; `docs/recovery.md` is rewritten against
  the new protocol.
- **The DST harness learns the second crash model.** The simulator today enumerates crash states
  of renames and directory-entry durability; the in-file protocol needs its own op vocabulary —
  positional writes, fsync barriers, the slot flip — with torn-superblock writes modeled
  explicitly (a torn slot fails its checksum and the previous slot is, by alternation, never the
  one being written). Sweeps print their coverage, as the existing protocol sweeps do.
- Corruption-storm coverage of superblocks, the member directory, extent map, and live-tail
  states.
- Full operation parity on container stores across Rust, CLI, and native Node: write, sync, flush,
  scan, SQL, compaction, verification, erasure, reclamation, backup, restore, recovery. A
  documented matrix, not an aspiration — each cell is a test.
- **Reclamation in place comes back.** Freed extents are never reused, but on Linux a dead extent
  in the single file can be hole-punched — the existing `punch` invariant ("reclaim without moving
  an offset") applied to the container; rewrite remains the portable fallback.
- Backup produces a self-contained ordinary container — one format and one lifecycle — and
  `unpack` and every restore-to-directory path go away. Restore produces a `.turndb`; copying a
  store is `cp`.
- The purge: CLI commands, binding constructors, examples, and docs stop accepting or producing
  directory stores, and the layout is not preserved internally either — once the native path
  passes its sweeps, the directory store's only surviving reader is the one-shot converter that
  moves existing stores forward. "A store is a directory you can tar" leaves the README; "a store
  is a file" replaces it.
- **The model's own prices, measured:** cold open (superblock, member directory, WAL replay) and
  flush-into-file latency, published with the Phase 2 tables.
- DST sweeps over the container plane at parity with the directory store: every crash point of the
  superblock alternation, member append, directory publication, and reclamation protocols, with the
  sweep printing its own coverage as the existing six do.
- Corruption-storm coverage of the superblock, member directory, and extent map.
- **Remote-read locality, measured:** a cold open (superblock → member directory → manifest → part
  TOCs) costs a documented, small, bounded number of positioned reads, and FORMAT.md states the
  bound. This is the browser and CDN read pattern; every round trip here is user-visible latency in
  Phase 3. Layout changes that reduce open-time round trips are in scope for this phase precisely
  because they are format changes.
- Format consolidation: whatever version number the finished container plane needs, it lands before
  the numbers are published, under the existing migration machinery.

### Maturity gate

A writer lives its whole life against one file and its WAL sidecar: killed at any DST-enumerated
crash point, the next open resumes from `.turndb` plus `.turndb-wal` and nothing else, and a clean
close leaves exactly one file. Every documented operation runs against that file and says so in
CI. A cold open over a high-latency `ReadAt` performs the documented number of round trips. The
only store a user can create, name, ship, or restore is a `.turndb` file, and handing someone one
requires zero caveats about which operations work on it.

## Phase 2: publish the proof

**Status: deferred until the later product phases mature.** Their storage and query changes can
move the measured numbers; publish the reproducible tables once those systems have settled rather
than canonizing intermediate results and re-running the proof after every phase. When it runs, it
runs against the README quickstart Phase 0 fixed, on a clean machine, with no step a stranger would
not type.

The dedup and collapse numbers were measured on private corpora and re-run offline against public
datasets, but nothing published is reproducible by a stranger. Fix that, against the layout Phase 1
finished.

### Deliverables

- A supported `bench/` harness promoted from `examples/` (`ingest_bench`, `fold_corpus`,
  `merge_bench`, `open_bench`, `query_demo` are most of it), runnable end-to-end with one command
  per published table.
- Named public datasets with scripted download and preparation. The sensitive corpus stays private
  and out of the published tables; where its results appear at all, they are labeled as
  non-reproducible context, as the README already does.
- The measurements: piece dedup and total on-disk collapse, ingest throughput and acknowledgement
  latency, the trace-UI query set (member page, id lookup, aggregates), compaction cost, verify and
  restore time — plus the single-file model's own prices: cold open with WAL replay, and
  flush-into-file latency.
- **A market scan before a baseline list.** Survey what teams actually store traces in today —
  observability defaults, the SQLite-shaped embedded paths, and the analytical stores — and let
  the survey pick the baselines. Three are already certain: the SQLite full-resend path (already
  built, it is what products do today), a columnar-file baseline (Parquet via DuckDB or
  DataFusion, what a columnar person would reach for), and **ClickHouse**, because it is the
  cluster-scale default the market reaches for and comparing only against embedded peers would
  look like hiding.
- **Every baseline is a steelman.** Each one is configured the way its own advocate would run it
  for this workload — real schema design, sorting keys, codecs, and compression tuned per store,
  not defaults — with the full configuration published beside the results and corrections
  invited. A benchmark that beats a strawman converts nobody who matters. Where a baseline wins a
  column, the table says so.
- A methodology document: hardware, dataset versions, run counts, variance. The README's
  provisional-numbers caveat is replaced by a pointer to it.

### Maturity gate

A stranger clones the repository and reproduces every published table within stated tolerance
without asking us anything. The README carries no number the harness cannot regenerate.

## Phase 3a: the SDK baseline

**Status: shipped 2026-08-14 (0.1.4, #107); gate unmet as of 2026-08-30.** The three contracts exist
with conformance vectors, the Node native and portable packages, the Python package and both Tier-2
exporters are published at 0.1.6 and install from the registries. What the gate asked for has not
happened: no demonstrated external or production consumer writes and reads through the Node package,
and the one named below has no dependency on TurnDB at all — its traces live in SQLite with its own
content-addressed blob store. The gate is therefore restated in two halves below so it can be met by
something this repository controls, and the CommandSuite half is stated as the dependency it is.

The contracts above, written down and enforced, then the bindings rebased onto them.

### Deliverables

- The capability contract and the query contract as documents with conformance tests — the
  three-path differential gate extended to run per binding, so a binding cannot drift from the
  engine's answers.
- The Node binding rebased onto the single-file store: `open_file` semantics, backup, the space
  operations, the query contract as its query surface. CommandSuite-ready, and CommandSuite is
  the gate's consumer.
- Python Tier 1: a PyO3 binding on the actor discipline the Node binding proved, same contract,
  same conformance suite.
- Tier 2 for both: the OTel span exporter writing `.turndb`, thin client wrappers, cadence
  policy, all implementing the one trace-mapping spec.

### Maturity gate

Two halves, and the phase is done when both are:

1. **Ours to meet.** A consumer that is not this repository's own test suite — the reference
   consumer harness under `bindings/node/qualification/` is the current stand-in, and it is not
   enough — writes and reads through the Node package with no reach into engine internals. A Python
   agent traces itself into a local file with two lines. Both speak the same query contract the
   browser and server will. The Python package states its capability difference from Node (no
   columnar/SQL lens, no cancellation) where a consumer chooses a binding, or closes it. The
   deliverable is native slices for Linux arm64, macOS x64 and macOS arm64 across the native Node
   package, the Python wheels and the CLI, built and install-tested in CI on the toolchain that
   targets each; Windows x64 native follows now that `src/sys.rs` carries a Windows floor; the portable
   package serves everywhere else, never as a silent fallback. The gate: no consumer on any of the
   five OS slices named above installs TurnDB and inherits a restriction TurnDB did not state up
   front.
2. **Theirs to state.** CommandSuite adopts the Node package for traces. What that requires of TurnDB
   — the agent-activity record family, retention semantics, a runner-side durable spool, anything
   else — is unknown until CommandSuite states its need, and this roadmap does not guess. The seam
   rule applies to every item that arrives: it goes into TurnDB only if a second trace consumer
   would want it; CommandSuite-only needs are built on top.

## Phase 3b: the browser — query a `.turndb` anywhere

**Status: shipped 2026-08-14 (#107); gate measured on one shape, 2026-08-30.** The one-file viewer is
attached to every GitHub release since v0.1.4 (v0.1.1 and v0.1.3 carry none). `docs/browser.md` records a 2.13 GiB container opening in 5 range
requests (250,267 bytes, 0.0109 % of the file) and answering a metadata point query with no further
bytes — on a store of 56 parts each holding one 52 MiB element. A many-part, many-segment store pays
`4 + 2S + D + 2P` reads by FORMAT.md's own formula and has not been measured over HTTP; that
measurement is what remains of this gate.

The dream, stated plainly: a precompiled HTML page, served from any static host or CDN, that opens a
`.turndb` file — dragged in, picked from disk, or fetched by URL with range requests — and lets the
user query it. No server compute. A shared trace becomes a link.

The engine is closer to this than it looks. `readat.rs` was built as the seam for exactly this
backend shape ("an object store's range request"); the SQL surface already emits complete Arrow IPC
streams, which is the natural wasm-to-JavaScript boundary; and the columnar lens is feature-gated
apart from DataFusion, so a scan-only build exists if SQL-in-wasm disappoints.

### Deliverables

- **Spike, first:** the read path (`--no-default-features --features columnar`) compiled to
  `wasm32-unknown-unknown`, and DataFusion's wasm story evaluated against our feature set.
  Community precedent exists (datafusion-wasm-bindings on npm; DataFusion's own wasm compile
  checks), so this is verification, not research. The spike's output is a decision: SQL in the
  first viewer, or structured scan first with SQL following.
- `ReadAt` backends for the browser: in-memory buffer (drag-and-drop), `Blob`/File API, and HTTP
  Range with a small block cache. Read-only: the write path stays on real files and real fsync by
  design, and the browser is never asked to fake either.
- Pruning everywhere the browser reads. The part format already carries advisory min/max zone
  maps and the columnar lens already prunes with them; the structured scan path does not. Extend
  zone pruning to structured scans, and audit what pruning needs beyond min/max (dictionary-bound
  checks for strings exist; presence/null maps may earn their place). Over HTTP, a skipped
  section is a fetch that never happens — the browser turns this from optimization into product.
- The viewer artifact: one self-contained `.html` — engine wasm inlined, no external requests
  except to the store URL — with a SQL box, structured browse, and paging. Size is explicitly not
  a goal; people loading a database into a tab will wait for a progress bar. Boot time and
  bytes-fetched-per-query are the goals.
- Bytes-proportionality measured and published alongside the Phase 2 tables: a point query against
  a multi-GiB container over HTTP fetches kilobytes to low megabytes, not the file.

### Maturity gate

A stock browser opens a container by URL from a static host, runs the trace-UI query set, and the
network tab shows fetch volume proportional to the query, not the store. The same page opens a
local file with no network at all.

## Phase 4: the trace vocabulary — one-stop shop

**Status: not started, 2026-08-30.** One family exists (the gen_ai exchange, through the Tier-2
exporters and `docs/trace-mapping.md`), and five trace-UI queries exist hard-coded in
`examples/genai_query.rs`; the agent-activity family, every *trace-format or provider* importer (the
generic JSONL `turndb import` exists), every *supported and documented* query recipe and every viewer
rendering below are unwritten. The activity family's shape waits on a consumer
stating what it records (Phase 3a, half 2); the engine already carries everything the shape will
need (named content, ordered typed attributes, prefix-ordered ids).

`examples/genai_dogfood.rs` is the working mapping and stays the reference for *why*; this phase
makes the mapping a supported surface rather than an example. The live motion — tracers and
exporters — landed with Phase 3a on the shared mapping spec; this phase completes the vocabulary
around it: importers for the logs that already exist, the canned queries, and the viewer's
rendering of them. The format learns nothing; the adapter layer and the viewer learn everything.

### Deliverables

- A first-party trace adapter with two record families: **LLM exchanges** (the existing
  `#system`/`#input`/`#output` shape, OTel `gen_ai.*` attribute names) and **agent activity**
  (sessions, tool calls, sub-agent spawns, handoffs — the structure of what the agent did between
  model calls).
- Importers behind `turndb import --format ...`: OTLP gen_ai spans, the major provider API log
  shapes, and at least one agent-framework session format. Each importer is a mapping into the
  adapter's families, tested for byte-exact round-trip of the raw payloads it preserves.
- Canned trace queries as documented recipes over the generic engine: session timeline, token
  spend by model and member, tool-call failure rates, slowest calls. The viewer renders these as
  views, which is where "one-stop shop" becomes something a person can see.
- The boundary restated for this phase, because it will be tested: importers and the viewer may be
  opinionated about trace semantics; FORMAT.md and the engine may not. A deployment's custom
  fields keep flowing through inferred-type columns with no schema change, as today.

### Maturity gate

A team with raw provider logs and no knowledge of TurnDB's record model gets to a queryable
container and a shareable viewer link without writing mapping code. The private corpus and at least
one public dataset both pass through the importers byte-exact.

## Phase 5: the center — serve stores

**Status: not started.**

Some deployments want ingestion and querying behind one endpoint. The engine does not grow a
network; a separate binary embeds it. "No daemon, no network" remains true of the core and FORMAT.md
while the product gains a server the way SQLite gained litestream and sqld — beside, not inside.

### Deliverables

- A server crate embedding the engine: single writer per store behind an ingest endpoint
  (importer formats accepted directly), query endpoint speaking Arrow IPC, health and metrics from
  the existing pull-based surfaces.
- The distribution loop that the earlier phases make possible, as a supported workflow: the server
  writes live stores, backs up history into containers on a schedule, and publishes them to static
  hosting — where Phase 3 viewers query them with no server involvement. Write centrally, read
  anywhere, pay for compute only at the write path.
- Multi-store tenancy (one store per member/project, which the id design already anticipates)
  rather than any multi-writer ambition inside a store.

### Maturity gate

A product points ingestion at the endpoint, queries recent data through it, and serves historical
trace links as static containers — with the server's absence provably harmless to every already-
published link.

## What this roadmap does not do

No multi-writer stores, no consensus, no encryption (the format still reserves and refuses the
bit), no Windows native build, and no second store layout: the directory form is an open writer's
working state, never again a thing a user ships. And no promise that the format freezes at the
end. Freezing is a 1.0 conversation, and it happens after the browser and importers have had their
chance to demand format changes — that is much of why they come before it.
