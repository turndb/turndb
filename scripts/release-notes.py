#!/usr/bin/env python3
"""Print the CHANGELOG.md section for one release, or refuse.

The release job used to delegate the GitHub release to `knope release`. That step decides whether
there is anything to release by comparing the version in `versioned_files` against the newest git
tag — and the step before it, `ensure-release-tag.sh`, has already created and pushed that tag so
the release attaches to an annotated object instead of the lightweight ref the Releases API would
leave behind. Knope therefore read its own precondition as proof the work was done, logged
`Last tag is 0.1.2`, and exited zero having created nothing. The v0.1.2 tag shipped with no GitHub
release and a green job.

Extracting the notes here makes the same failure loud: a missing or empty section is a non-zero
exit, and the caller cannot publish a release with nothing in it.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CHANGELOG = ROOT / "CHANGELOG.md"

# Two heading styles coexist. Knope writes `## 0.1.2 (2026-08-08)`; the hand-authored first release
# is `## [0.1.0] — 2026-08-06`. Accepting only one of them would silently skip the other.
HEADING = re.compile(r"^##\s+\[?(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)\]?\s*[—(-]?")
ANY_HEADING = re.compile(r"^##\s")


def section(text: str, version: str) -> str:
    lines = text.splitlines()
    start = None
    for index, line in enumerate(lines):
        match = HEADING.match(line)
        if match and match.group(1) == version:
            start = index + 1
            break
    if start is None:
        raise SystemExit(f"CHANGELOG.md has no section for {version}")

    end = len(lines)
    for index in range(start, len(lines)):
        if ANY_HEADING.match(lines[index]):
            end = index
            break

    body = "\n".join(lines[start:end]).strip()
    if not body:
        raise SystemExit(f"CHANGELOG.md section for {version} is empty")
    return body


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: release-notes.py X.Y.Z")
    print(section(CHANGELOG.read_text(), sys.argv[1]))
