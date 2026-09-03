---
default: major
---

# Containers have one lifecycle

The container-level `SEALED` state is removed. Backups are verified, self-contained containers
that readers can query directly and writers can continue independently; restore remains a
verified, no-replace copy and no longer mutates a lifecycle flag. The CLI and Tier-1 binding
operation is now `backup`, and the former `seal` alias is removed.

The container plane advances to revision 3, where superblock bytes 50–51 are reserved and must be
zero. Revision-2 containers carrying the retired finalization bit remain readable and writable;
the bit has no effect, and their next commit publishes revision 3. Backup staging is now named
`<artifact>.backing-up`.
