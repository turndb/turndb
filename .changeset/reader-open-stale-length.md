---
default: patch
---

# Lock-free opens no longer race a commit into a false truncation refusal

A lock-free reader open measures the container's length and then reads the superblock slots. A
writer committing in that gap — bytes appended past the old tail, fsync, slot flip — could leave
the newest slot's tail beyond the stale measurement, and the open refused a healthy, fully
committed store as truncated.

Both open paths now re-measure once when the committed tail exceeds the first measurement.
Containers only grow — reclamation punches holes in place — so any slot the open managed to read
was committed by the time the second measurement is taken, and one re-measure is decisive. A tail
still beyond the second answer is genuine truncation and refuses exactly as before.
