---
default: patch
---

# TurnDB has a closed semantic ontology

`ONTOLOGY.md` now defines the project vocabulary, relations, states, transitions, observations,
evidence, and invariants that every public surface must share. Its closed-world rule prevents an API
spelling or plausible local explanation from silently introducing a new lifecycle concept. The
ontology separates acceptance, durability, publication, and settlement; distinguishes manifest,
part-sequence, and container ordering; and rejects `sealed` as a product lifecycle state while the
existing physical format marker remains delegated to `FORMAT.md`.
