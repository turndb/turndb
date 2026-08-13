---
default: minor
---

# One layout remains

The directory store's write path is gone from the engine: the checkpoint bridge
(`ContainerStore`), the pack writer and restorer, `checkpoint_into_container`, and every public
directory-form surface (`Store::open` and its reader family, `recover_manifest`, `verify_chain`,
`retained_commits`) are deleted. The single-file store is the store; `convert` is the retired
layout's one remaining door, proven against a checked-in fixture written by 0.1.3 itself —
unsettled WAL included, because converting must replay acknowledged writes, and now it is proven
to at every crash point.

Retiring the layout's tests forced its replacements to earn their coverage, and they found real
gaps, all fixed: conversion now builds in staging and publishes with a no-replace rename, so a
crash mid-convert is recovered by re-running it; file recovery's promotion flip now also
truncates the fold to the promoted tail, so a rolled-back store reopens instead of refusing its
own manifest; restore's copy goes through the recorded write seam and is fsynced before
publication; `reclaim` takes the writer flock and refuses an unsettled WAL sidecar instead of
checking for a working directory no writer creates any more; and opening a missing container
refuses typed (`NOT_FOUND`) without ever creating a transient file at the queried name.

The CLI's `erase` verb, missed in the file-first migration, now opens the store file and reads
its audit hashes from the manifest member.
