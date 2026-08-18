---
default: patch
---

# Cold opens no longer scan active content

Every single-file commit now publishes the active fold segment's advisory block directory in the
same superblock state as the segment tail. A ranged reader can therefore open a current store from
container, manifest, segment-index, and part metadata without fetching fold block payloads.

The executable contract measures an uncached positioned source at exactly
`4 + 2*segments + dictionaries + 2*parts` reads. Missing, damaged, stale, or over-budget advisory
indexes remain readable through the existing checked frame scan; the optimization never becomes a
new correctness requirement or a reason to refuse an older valid store.

The browser measurement now reports cold-open and point-query HTTP fetches separately instead of
letting one combined number stand in for both behaviors.
