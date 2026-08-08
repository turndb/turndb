#!/usr/bin/env python3
"""Positive and negative controls for the release metadata detector."""

import json
import os
import pathlib
import shutil
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
CHECK = ROOT / "scripts/check-release-metadata.py"


def copy_tracked(dest: pathlib.Path) -> None:
    """Copy the tracked working tree, and nothing else.

    `cp -a` of the repository copies `target/` with it — tens of gigabytes of build output, three
    times, to check a handful of manifests. On a warm tree that is slow at best and fails on
    `ENOSPC` at worst, which reports as the control being broken rather than as the disk being
    full. The checker only ever reads tracked files, so those are the only ones worth staging.
    """
    tracked = subprocess.run(
        ["git", "-C", str(ROOT), "ls-files", "-z"],
        check=True,
        capture_output=True,
    ).stdout.split(b"\0")
    for raw in tracked:
        if not raw:
            continue
        rel = pathlib.Path(os.fsdecode(raw))
        src = ROOT / rel
        # A tracked path can be absent mid-rebase or mid-edit; the checker would refuse for its own
        # reasons if one it needs is missing, which is the answer we want rather than a crash here.
        if not src.is_file():
            continue
        dst = dest / rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)


def run(root: pathlib.Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["python3", str(root / "scripts/check-release-metadata.py")], cwd=root, text=True, capture_output=True)


baseline = run(ROOT)
if baseline.returncode != 0:
    raise SystemExit(f"baseline detector failed:\n{baseline.stdout}{baseline.stderr}")

with tempfile.TemporaryDirectory() as temp:
    copy = pathlib.Path(temp) / "repo"
    copy_tracked(copy)
    bad = copy / ".changeset/invalid-control.md"
    bad.parent.mkdir(exist_ok=True)
    bad.write_text("---\ndefault: mjaor\n---\n\ncontrol\n")
    result = run(copy)
    if result.returncode == 0 or "unknown bump 'mjaor'" not in result.stderr:
        raise SystemExit(f"unknown-bump control did not discriminate:\n{result.stdout}{result.stderr}")

with tempfile.TemporaryDirectory() as temp:
    copy = pathlib.Path(temp) / "repo"
    copy_tracked(copy)
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
    copy_tracked(copy)
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
