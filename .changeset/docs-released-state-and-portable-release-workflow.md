---
default: patch
---

# Documentation describes the released project; the portable npm package publishes through CI

Public documentation is aligned with the released 0.1.0: registry publication is
recorded as fact (2026-08-06, three artifacts), internal branch, commit, and
process references are removed, review-reply and sprint-log prose is rewritten
as documentation, and the npm-facing READMEs use absolute links that resolve
from the registry pages. ROADMAP.md, largely completed, is retired.

The portable `turndb` npm package now publishes through the same tag-gated,
owner-approved release path as the crate and native packages: a dedicated
workflow builds it from the exact annotated lockstep tag, runs the package
suite, exercises the packed tarball on every supported Node major, and
publishes that exact tarball via npm trusted publishing.
