---
default: minor
---

# Manifest promotion repairs retained history

A retained manifest copy is authority a reader may still open at, so a store whose retained
history no longer validates refuses every open, exactly as one with a damaged current `MANIFEST`
does. `turndb recover`, `recoverManifest`, and `promote_manifest_file` now treat that as
promotable: beside an intact current manifest, promotion at rollback zero re-selects the current
revision and ends retention of the first older revision that fails to validate and of everything
behind it, and reports the count as `abandoned_retained_revisions` in Rust and
`abandonedRetainedRevisions` in Node. A store whose current manifest is intact and whose retained
chain validates still refuses as healthy.

Candidate search changed with it. A damaged older copy no longer disqualifies the newest usable
candidate, so promotion keeps the newest acknowledged revisions and abandons the damaged history
behind them instead of rolling back past it.
