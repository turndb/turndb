# Security review

This review covers TurnDB's revision-4 storage core, revision-1 pack, native Node binding, and
portable WASI binding as of 2026-08-03. It is an engineering threat review, not a claim of formal
verification or an independent third-party audit. Findings remain useful only if new format fields,
parsers, and binding methods update both the review and their adversarial tests.

## Threat model

In scope:

- a store directory, retained manifest, or pack whose bytes are malformed or adversarial;
- accidental corruption, torn writes, future-version bytes, decompression bombs, extreme counts and
  lengths, path traversal, symlinks in an offline store supplied for backup, and parser panics;
- untrusted values crossing the Node or WASI API boundary, including out-of-range integers,
  oversized work requests, stale handles, cancellation, and concurrent method calls;
- preservation of integrity and availability, and preventing a crafted store from making backup
  disclose an unrelated host file.

Out of scope:

- an attacker who can write the live store directory concurrently with TurnDB. Writer locking
  coordinates cooperating writers; it is not a sandbox against a malicious filesystem peer, and
  path inspection cannot close arbitrary time-of-check/time-of-use replacement races;
- authentication or confidentiality of store contents. CRC32 and BLAKE3 detect drift and bind
  identities; they are not keyed authenticity. The format is plaintext and explicitly refuses its
  reserved encryption flag;
- authorization, tenant isolation, redaction, network protocol security, and backup transport.
  TurnDB is embedded and exposes filesystem authority supplied by its host application;
- arbitrary denial of service by code already executing inside the same Node process or WASI host.
  That code can consume memory or CPU without invoking TurnDB. The binding nevertheless rejects
  mistaken values and bounds engine-owned queues/work where it can.

## Findings closed in this review

### SR-01: manifest path authority could escape the store root — fixed

A syntactically valid, correctly checksummed manifest previously allowed a part name such as
`../secret.part`. Reader open could follow it, and backup could copy the external file into an
artifact. Manifest parsing now requires one non-empty normal path component and rejects absolute,
parent, nested, and backslash-separated names before any part access. Duplicate part names, inverted
sequence ranges, malformed BLAKE3 digests, and overlapping or inverted punched ranges are also
refused: checksum-valid bytes do not bypass semantic validation.

Backup resolves every member against the canonical store root and requires the ordinary file at the
exact expected path. This additionally refuses final or intermediate symlinks, including an otherwise
valid part moved outside the store and linked back under its original name. Publication remains
atomic and no-replace.

Evidence: `manifest_part_names_cannot_escape_the_store_root`,
`manifest_semantics_reject_ambiguous_or_malformed_authority`, and
`backup_refuses_store_members_that_resolve_through_symlinks`.

### SR-02: pack metadata could request wild allocations — fixed

The pack footer uses u32 stored/raw TOC lengths and file count. Range checks stopped out-of-file
reads, but a sparse file could still make a multi-gigabyte extent appear valid and trigger allocation
before the TOC was parsed. `PackLimits` now gates compressed TOC bytes, decompressed TOC bytes, file
count, and entry-name length before those operations. Defaults are 64 MiB, 64 MiB, 100,000 files, and
16 KiB per name; Rust embedders can deliberately raise or lower them without changing the format.
Integer conversion and buffer reservation are checked. Member verification and restore continue to
stream in 1 MiB chunks, and inline member reads have an explicit bounded variant.

Evidence: `pack_metadata_admission_precedes_hostile_sparse_allocations` plus the seeded pack mutation
storm in `tests/corruption.rs`.

### SR-03: authoritative and advisory whole-file reads were unbounded — fixed for metadata

Manifest reads now inspect the announced length, reserve fallibly, read through a 64 MiB + 1 guard,
and check again after the read to close a growth race. The same path is used for live, retained,
recovery, pack, and chain-verification manifests. Part digest verification was changed from a
whole-file allocation to bounded streaming.

Segment sidecars are advisory and now may consume no more bytes than the segment's maximum possible
block count can describe. An impossible sidecar is ignored and the authoritative segment is scanned.
Candidate zstd dictionaries are admitted under a 64 MiB ceiling for both directory and pack readers.

Evidence: `manifest_size_is_refused_before_reading_a_sparse_body` and
`advisory_sidecar_size_is_bounded_by_the_segment_it_describes`.

### SR-04: atomic data-plane frames could exceed cache budgets — fixed

Cache ceilings bounded retained residency but necessarily admitted one complete entry, so a selected
part section or fold block could request the full u32 stored/decoded length first. WAL replay and fold
tail scanning had equivalent stored-frame allocations. `ReadLimits` now checks both dimensions before
input/output allocation, defaults each to 512 MiB, and is configurable per Rust, native Node, and
portable WASI open. Valid policy refusals are typed `RESOURCE_EXHAUSTED`; malformed policies are
`INVALID_ARGUMENT`.

Tail scanning propagates an over-budget valid header instead of treating it as a torn boundary, so a
strict writer recovery cannot truncate committed bytes. Writers apply the same policy: fold blocks
seal early for small-record progress, one oversized piece fails before mutation, and part flush,
merge, refold, and migration outputs fail before their footer publication marker. Lazy part reads
charge only the selected section. Whole-batch piece preflight precedes the first fold mutation.

Evidence: `part_toc_and_selected_sections_are_admitted_before_decode`,
`part_writer_refuses_an_unreopenable_section_before_footer_publication`,
`strict_fold_profile_splits_for_progress_and_refuses_one_oversized_piece_before_mutation`,
`a_late_oversized_batch_piece_is_refused_before_any_fold_or_wal_mutation`,
`aggregate_wal_frame_admission_precedes_fold_mutation`,
`strict_tail_scan_refuses_without_truncating_valid_large_frames`, and
`replay_admits_a_complete_frame_before_payload_allocation`,
`restore_preserves_frame_budget_refusal_instead_of_calling_the_backup_invalid`, plus
native/portable binding tests.

## Existing controls reviewed

File formats reject unknown future versions/flags rather than guessing. Footer-to-TOC-to-section
checksums and range checks precede parser use; content pieces are BLAKE3-verified on every read, and
available whole-value identities verify reconstruction order. The deterministic mutation suite walks
parts, WAL records/frames, fold segments, manifests, and packs and requires errors rather than panics.
The deterministic crash harness covers durable publication transitions separately; corruption and
crash are not conflated.

Restore verifies every pack member, rejects unsafe relative names, extracts only into a newly created
sibling stage, opens the staged ordinary store, and publishes with an atomic no-replace directory
rename. Backup and restore copy/hash in bounded chunks and remove unpublished staging on cancellation
or failure. Checksums intentionally do not promise authenticity against a party able to rewrite both
payload and checksum.

The native Node crate contains no `unsafe` blocks. BigInt inputs are checked for sign and lossless
i64/u64 conversion, buffers remain binary, exact integers never pass through JavaScript `number`, and
typed scalar objects require one value of the declared kind. One bounded Rust actor owns each writer;
queue saturation is a typed `BUSY`, scan/SQL/maintenance work has explicit ceilings or cooperative
interruption, and concurrent SQL reservations are governed in aggregate. Rust owns durability,
visibility, cursors, read-only SQL policy, and storage paths.

The WASI binding's `unsafe` surface is limited to its documented pointer/length ABI and allocator
pair. The bundled JavaScript wrapper supplies those pairs and stale numeric store handles are refused.
A hostile custom WASM host can violate the ABI contract and trap its instance; WASM linear-memory
isolation prevents that contract from becoming native process memory access. WASI still cannot
enforce the single-writer lock, which is a capability reduction rather than a parser finding.

## Residual risks and required follow-up

1. **Object-count admission is incomplete for directory stores.** Pack metadata is bounded and a
   manifest byte ceiling indirectly limits parts, but directory enumeration of segments,
   dictionaries, and retained files has no explicit count budget. Fold directory reconstruction can
   also resize its block-id index from a checksummed but adversarially sparse id. Add per-open
   filesystem-object, WAL-frame, and fold-directory-entry ceilings with typed resource refusal.
2. **Concurrent hostile filesystem mutation is not contained.** Canonical backup checks protect an
   offline supplied store but are not an `openat2(RESOLVE_BENEATH)` sandbox. Applications must not
   grant untrusted writers access to an actively opened store directory. A future hardened Linux
   profile may use directory descriptors and no-follow/beneath resolution throughout.
3. **Dependency and supply-chain review remains external.** Rust/Node lockfiles pin the resolved
   graph and CI builds all feature profiles, but this review did not independently audit DataFusion,
   Arrow, zstd, napi-rs, or the npm publication chain. Release automation should add vulnerability,
   license, provenance, and reproducible-artifact gates rather than treating source review as a
   substitute.
4. **CPU budgets are cooperative, not preemptive.** Cancellation checkpoints surround bounded work,
   but one zstd frame decode/encode and one part encoding unit are intentionally uninterruptible.
   Admission limits and worker isolation remain the protection against a single expensive atomic
   codec operation.

These are tracked as open hardening work, not hidden behind a blanket “untrusted input safe” claim.
Deployments accepting arbitrary third-party stores should still validate them in a resource-limited
helper process when stronger containment is required: configured allocation ceilings do not make
codec CPU preemptive or defend against a malicious peer concurrently replacing filesystem objects.

## Review checklist for future changes

Every new on-disk count, offset, or length needs an overflow-safe range check before conversion or
allocation, a nearest-invalid test, and mutation coverage. Every path from stored bytes must be
relative, semantically validated, and considered under symlinks. Every new binding input needs exact
type/range validation and an engine-owned work or memory bound where the binding performs a copy.
Every destructive/publication operation must identify its last cancellable point and prove no-replace
publication. Any new `unsafe` block needs a local safety contract and an adversarial boundary test.

Test fixtures that create stores or sparse adversarial files must own their temporary directories
with scope-bound cleanup. The qualification suite also runs with incremental compilation disabled so
repeated long-running verification does not leave unbounded build or fixture data behind.
