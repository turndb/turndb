#!/usr/bin/env python3
"""Positive and negative controls for the release metadata detector."""

import json
import pathlib
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
CHECK = ROOT / "scripts/check-release-metadata.py"


def run(root: pathlib.Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["python3", str(root / "scripts/check-release-metadata.py")], cwd=root, text=True, capture_output=True)


baseline = run(ROOT)
if baseline.returncode != 0:
    raise SystemExit(f"baseline detector failed:\n{baseline.stdout}{baseline.stderr}")

with tempfile.TemporaryDirectory() as temp:
    copy = pathlib.Path(temp) / "repo"
    subprocess.run(["cp", "-a", str(ROOT), str(copy)], check=True)
    bad = copy / ".changeset/invalid-control.md"
    bad.parent.mkdir(exist_ok=True)
    bad.write_text("---\ndefault: mjaor\n---\n\ncontrol\n")
    result = run(copy)
    if result.returncode == 0 or "unknown bump 'mjaor'" not in result.stderr:
        raise SystemExit(f"unknown-bump control did not discriminate:\n{result.stdout}{result.stderr}")

with tempfile.TemporaryDirectory() as temp:
    copy = pathlib.Path(temp) / "repo"
    subprocess.run(["cp", "-a", str(ROOT), str(copy)], check=True)
    manifest = copy / "npm/turndb/package.json"
    # Derive the version to perturb rather than hardcoding one: a hardcoded string stops matching
    # on the first release PR that bumps it, injects no drift, and fails the control spuriously.
    current = json.loads(manifest.read_text())["version"]
    manifest.write_text(
        manifest.read_text().replace(f'"version": "{current}"', '"version": "9.9.9"', 1)
    )
    result = run(copy)
    if result.returncode == 0 or "lockstep version mismatch" not in result.stderr:
        raise SystemExit(f"version-drift control did not discriminate:\n{result.stdout}{result.stderr}")

with tempfile.TemporaryDirectory() as temp:
    copy = pathlib.Path(temp) / "repo"
    subprocess.run(["cp", "-a", str(ROOT), str(copy)], check=True)
    workflow = copy / ".github/workflows/release-native.yml"
    workflow.write_text(workflow.read_text().replace(
        'case "$RELEASE_REF" in v[0-9]*.[0-9]*.[0-9]*)',
        'case "$RELEASE_REF" in native-v[0-9]*.[0-9]*.[0-9]*)',
        1,
    ))
    result = run(copy)
    if result.returncode == 0 or "obsolete release tag namespace" not in result.stderr:
        raise SystemExit(f"tag-namespace control did not discriminate:\n{result.stdout}{result.stderr}")

print("release metadata controls: baseline passes; unknown bump, version drift, and old tag namespace are refused")
