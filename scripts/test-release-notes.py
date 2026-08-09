#!/usr/bin/env python3
"""Positive and negative controls for the release notes extractor.

The control that matters is `missing`: the failure this replaced was a release step that found
nothing to say and exited zero. An extractor that returns empty output on an absent section would
reproduce it exactly, so that case must be shown to fail.
"""

import importlib.util
import pathlib
import subprocess
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/release-notes.py"

spec = importlib.util.spec_from_file_location("release_notes", SCRIPT)
release_notes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_notes)


def refuses(text: str, version: str, expected: str) -> None:
    try:
        release_notes.section(text, version)
    except SystemExit as exit_reason:
        if expected not in str(exit_reason):
            raise SystemExit(f"{expected!r} control refused for the wrong reason: {exit_reason}")
        return
    raise SystemExit(f"{expected!r} control did not discriminate: extraction succeeded")


# The version under development always has a changelog section; a release cannot describe itself
# otherwise. Running the real script proves the command line contract, not just the function.
version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
baseline = subprocess.run(
    ["python3", str(SCRIPT), version], cwd=ROOT, text=True, capture_output=True
)
if baseline.returncode != 0 or not baseline.stdout.strip():
    raise SystemExit(f"baseline extraction failed for {version}:\n{baseline.stdout}{baseline.stderr}")

SAMPLE = """# Changelog

preamble

## [0.1.0] — 2026-08-06

first release

### Added

- a thing

## 0.2.0 (2026-08-08)

### Features

- another thing

## 0.1.1 (2026-08-07)

### Fixes

- a fix
"""

if release_notes.section(SAMPLE, "0.2.0") != "### Features\n\n- another thing":
    raise SystemExit("extraction did not stop at the next release heading")

# Subsections must not terminate the section, and the hand-authored bracket style must be found.
if "- a thing" not in release_notes.section(SAMPLE, "0.1.0"):
    raise SystemExit("extraction stopped at a subsection heading or missed the bracket style")

refuses(SAMPLE, "9.9.9", "no section for")
# A prerelease heading must not answer a request for the release itself.
refuses(SAMPLE.replace("## 0.2.0 (", "## 0.2.0-rc.1 ("), "0.2.0", "no section for")
refuses("# Changelog\n\n## 0.3.0 (2026-08-09)\n\n## 0.2.0 (2026-08-08)\n\nnotes\n", "0.3.0", "is empty")
# The last section in the file has no following heading to stop at, so its emptiness is a
# different code path from the one above.
refuses("# Changelog\n\n## 0.3.0 (2026-08-09)\n\n", "0.3.0", "is empty")

print("release notes controls: baseline extracts; missing, empty, and over-running sections are refused")
