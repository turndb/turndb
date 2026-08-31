---
default: patch
---

# Crash leftovers and failed durability barriers are reported

Writer open now recognises every transient name the publication and reclaim protocols can leave
after a crash. Beside a present store it removes safe-to-discard leftovers and reports the count in
`StoreMetrics.debris_removed`; beside an absent store it refuses to create over pending publication
or reclaim material and names what must be inspected. A legacy `<store>-hot` working directory is
always reported and refused, never removed. The public, non-exhaustive `DebrisReport`, `DebrisEntry`,
and `DebrisKind` types — returned by `debris_report` and `debris_report_with_limits` — expose the
same inventory without mutating it.

`turndb inspect` scans that inventory before opening the target, so it can report leftovers beside
an absent store and gives a conversion hint for a retired directory store. The support and format
documents list every recognised name, when it can appear, and the corresponding recovery action.

Every publication path also propagates a failed file or directory sync. Operations that previously
could report success after the durability barrier failed now return an error describing the name
whose persistence is uncertain. This is a patch-level data-integrity correction: the changed
outcomes were false success or creation over ambiguous recovery material, not valid successful
workflows.
