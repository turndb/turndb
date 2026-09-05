---
default: major
---

# One unfrozen physical draft replaces every discarded layout

TurnDB now has one writable single-file store shape and one exact draft-format identity. The
container, part, fold segment, fold sidecar, WAL, and manifest carry only the identities listed in
`FORMAT.md`, and every other byte structure fails closed. The former directory store, the archive
pack, the converter, the resumable format migration, the compatibility readers for earlier
container and part revisions, the fixtures kept as their evidence, and every preceding-format
promise are deleted rather than retired. `ONTOLOGY.md` closes the project vocabulary that every
surface now shares, and `docs/operation-registry.md` maps each public spelling onto it.

The container-level `SEALED` state and the `seal` operation are gone. Backup is the one
snapshot-copy operation everywhere: it produces an ordinary, fully verified, self-contained
container that readers query directly and writers continue independently, staged under
`<artifact>.backing-up-<pid>-<n>` and installed by a no-replace primitive that never replaces a
destination. Restore is a verified no-replace copy staged under `<destination>.restoring-<pid>-<n>`.
The discarded `.sealing`, `.converting`, and `-hot` names have no TurnDB meaning. Unknown members,
identities, epochs, and semantically impossible current bytes refuse before cleanup or mutation,
and every open now refuses a store whose retained manifest history no longer validates; manifest
promotion repairs that (see Features). The language-neutral capability contract advances to v2 with
no v1 responder or adapter remaining; the separately versioned structured-query contract stays at
v1.

## Upgrading

This is an intentional hard reset of an unfrozen development format, not an upgrade path. Nothing
in this release reads what 0.1.x wrote: a 0.1.x directory store, pack, or `.turndb` container is
refused as an unrecognized identity. Export or regenerate development data with the build that
created it before moving to this release. Stores this release writes are readable by every
entrance of this release, and `FORMAT.md` states the identity a future incompatible change would
rotate.

## Renamed and removed surfaces

Rust crate `turndb`:

- `store::recover_manifest_file`, `recover_manifest_file_with_limits_and_control`,
  `RecoveryOptions`, `RecoveryReport`, and `RecoveryError` are `promote_manifest_file`,
  `promote_manifest_file_with_limits_and_control`, `ManifestPromotionOptions`,
  `ManifestPromotionReport`, and `ManifestPromotionError`.
- The `pack` module, `Pack`, `PackLimits`, `open_read_pack`, `open_read_pack_with_limits`,
  `convert_to_file`, `ConvertStats`, `single_file_kind`, `SingleFileKind`, `looks_like_store`,
  and the directory-store refold helpers are removed. The `backup` module carries `BackupStats`,
  `RestoreStats`, `BackupError`, and `ATOMIC_RESTORE`; `BackupStats::files` and
  `RestoreStats::files` are `members`.
- `Store::format_migration_status`, `estimate_format_migration_space`, `migrate_format_step`, their
  controlled variants, `FormatMigrationStatus`, `FormatMigrationPlan`, `FormatMigrationStep`, and
  `Part::format_version` are removed.
- `PunchStats` is `ContentPunchStats`; `Fold::seal_window` is `release_dedup_window`;
  `Container::sealed`, `Container::commit_sealed`, `SB_FLAG_SEALED`, and `HOT_SUFFIX` are removed.
- `capabilities()` replaces `part_format_write`, `part_format_read_max`, and `format_migration`
  with `draft_format_epoch` and `in_place_deallocation`.
- `StoreMetrics::open_recovery`, `compaction`, and `punch` are `open_wal_replay`, `merge`, and
  `content_punch`; `format_migration` is removed. `SpaceAmount::files` is `members`.
  `ChainReport::undigested` and `StoreVerification::unidentified_content_values` are removed,
  because every part is pinned and every content value carries its identity.
- `Manifest` requires `draft_epoch`; `PartRef::file` is `member` and `b3` is required.
- `DebrisKind` drops the directory-layout variants `ManifestStaging`, `ExcessRetainedManifest`,
  `SegmentSidecarStaging`, `PartBuilderSpool`, and `LegacyHotDirectory`, and adds
  `CreationStaging` for `<store>.creating-<pid>-<n>`.
- `types::BodyOp`, `Record::body`, `Record::body_len`, and `Part::body` are removed; use
  `ContentOp` and `Record::content(BODY_CONTENT)`.
- The WAL tag constants are `TOMB_TAG`, `BATCH_COMPLETE_TAG`, `BATCH_TOMB_TAG`, `RECORD_TAG`, and
  `BATCH_RECORD_TAG` with the current values, and every magic constant is the new identity.

CLI `turndb`:

- `seal` is `backup`; `punch` is `content-punch` for declared block deallocation and
  `free-space-punch` for free-extent interiors; `convert` is removed.
- `inspect` reports the manifest revision, the container state sequence, the records in the
  current read view, and the retained manifest revisions under those names.

Native Node `@turndb/native`:

- `seal` is removed in favour of `backup`; `punch` is `contentPunch`; `singleFileKind`,
  `formatMigrationStatus`, `estimateFormatMigrationSpace`, and `migrateFormatStep` are removed.
- `RecoveryOptions` is `ManifestPromotionOptions`; `recoverManifest` keeps its name.
- Backup, restore, and `spaceUsage` results report `members` rather than `files`; `verify`
  results drop `undigestedParts`.
- `capabilities()` reports `contractVersion: 2`, `draftFormatEpoch`, `inPlaceDeallocation`,
  `manifestPromotionControls`, and `reclamation: 'content_punch_or_refold'`, and no longer reports
  `partFormat`, `formatMigration`, or `recoveryControls`. Metrics use `openWalReplay`, `merge`,
  `contentPunch`, and `erase`, with no `formatMigration`.

Python `turndb`:

- `seal` is `backup`. `capabilities()` reports `contractVersion: 2` and `draftFormatEpoch`.
  `verify()` reports `scope: "current_manifest_revision"` and no `unidentifiedContentValues`.

Portable npm `turndb`:

- `singleFileKind` is removed. `capabilities()` reports `contractVersion: 2`, `draftFormatEpoch`,
  and `atomicNoReplaceInstallation` in place of `partFormat` and `atomicNoReplacePublication`.
- Metrics keys match the native binding. `verify()` results carry
  `scope: 'current_manifest_revision'`, `state: 'valid'` only, and no `unidentifiedContentValues`
  or `chain.undigestedParts`.
- The low-level WASI exports `tdb_open_v2`, `tdb_open_v3`, and `tdb_single_file_kind` are gone;
  `tdb_open` takes the complete admission profile.

Browser reader and conformance:

- The capability contract and its schema live at `conformance/v2`; the query and runner contract
  stays at `conformance/v1`, and its fixture is regenerated under the current identity.
