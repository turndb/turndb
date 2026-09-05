---
default: major
---

# One unfrozen physical draft replaces every discarded layout

TurnDB now has one writable single-file store shape and one exact draft-format epoch. Container,
part, fold, sidecar, WAL, manifest, and content-identity encodings use only the identities listed in
`FORMAT.md`; anything else fails closed. The former directory store, archive pack, conversion,
format-migration, compatibility readers, old fixtures, and preceding-format promises are deleted.
This is an intentional hard reset of an unfrozen development format, not an upgrade path. Existing
development artifacts must be regenerated or exported with their originating build before moving
to this one.

The container-level `SEALED` state and operation are also gone. Backups are fully verified,
self-contained current-draft containers that readers may query directly and writers may continue
independently; restore is a verified no-replace copy. Backup staging is named
`<artifact>.backing-up-<pid>-<n>`, and discarded `.sealing`, `.converting`, and `-hot` names have no
TurnDB meaning. The language-neutral capability profile advances to contract v2 and no v1
capability responder or adapter remains; the separately versioned structured-query contract stays
at v1.

Manifest promotion at rollback zero repairs retained history that no longer validates beneath an
intact current manifest, and reports how many older retained revisions it abandoned.
