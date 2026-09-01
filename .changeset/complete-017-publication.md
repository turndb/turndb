---
default: patch
---

# Ships the npm packages absent from 0.1.7

TurnDB 0.1.7 was published to crates.io and PyPI, but no TurnDB npm package was published at
0.1.7. The npm publication could not complete from that tag because the release path executes its
packaging verification scripts from the tagged tree, while the required release-tooling repairs
landed after the tag was created.

TurnDB 0.1.8 contains the same product changes described in the 0.1.7 release notes, together with
the repaired release tooling needed to publish the npm packages coherently. Rust crate and Python
package users already on 0.1.7 do not need to change versions. npm users should install 0.1.8
directly.
