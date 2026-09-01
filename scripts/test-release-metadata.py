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


def copy_candidates(dest: pathlib.Path) -> None:
    """Copy version-controlled candidates, including new non-ignored files, and nothing else.

    `cp -a` of the repository copies `target/` with it — tens of gigabytes of build output, three
    times, to check a handful of manifests. On a warm tree that is slow at best and fails on
    `ENOSPC` at worst, which reports as the control being broken rather than as the disk being
    full. A change may add a versioned manifest and its controls must pass before it is staged, so
    include untracked, non-ignored files while continuing to exclude build output.
    """
    tracked = subprocess.run(
        [
            "git", "-C", str(ROOT), "ls-files", "-z", "--cached", "--others",
            "--exclude-standard",
        ],
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
    copy_candidates(copy)
    bad = copy / ".changeset/invalid-control.md"
    bad.parent.mkdir(exist_ok=True)
    bad.write_text("---\ndefault: mjaor\n---\n\ncontrol\n")
    result = run(copy)
    if result.returncode == 0 or "unknown bump 'mjaor'" not in result.stderr:
        raise SystemExit(f"unknown-bump control did not discriminate:\n{result.stdout}{result.stderr}")

with tempfile.TemporaryDirectory() as temp:
    copy = pathlib.Path(temp) / "repo"
    copy_candidates(copy)
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
    copy_candidates(copy)
    lock_path = copy / "bindings/node/package-lock.json"
    lock = json.loads(lock_path.read_text())
    entry = lock["packages"]["node_modules/@turndb/native-linux-x64-gnu"]
    entry.pop("version")
    lock_path.write_text(json.dumps(lock, indent=2) + "\n")
    result = run(copy)
    if result.returncode == 0 or "expected unresolved versioned optional stub" not in result.stderr:
        raise SystemExit(f"versionless-lock-stub control did not discriminate:\n{result.stdout}{result.stderr}")

with tempfile.TemporaryDirectory() as temp:
    copy = pathlib.Path(temp) / "repo"
    copy_candidates(copy)
    workflow = copy / ".github/workflows/release-native.yml"
    workflow.write_text(workflow.read_text().replace(
        'case "$RELEASE_REF" in v[0-9]*.[0-9]*.[0-9]*)',
        'case "$RELEASE_REF" in native-v[0-9]*.[0-9]*.[0-9]*)',
        1,
    ))
    result = run(copy)
    if result.returncode == 0 or "obsolete release tag namespace" not in result.stderr:
        raise SystemExit(f"tag-namespace control did not discriminate:\n{result.stdout}{result.stderr}")

with tempfile.TemporaryDirectory() as temp:
    copy = pathlib.Path(temp) / "repo"
    copy_candidates(copy)
    publisher = copy / "bindings/node/scripts/publish-prebuild.cjs"
    publisher.write_text(publisher.read_text().replace(
        "const expectedTag = `v${version}`;",
        "const expectedTag = 'v0.0.0';",
        1,
    ))
    result = run(copy)
    if result.returncode == 0 or "does not derive the lockstep tag" not in result.stderr:
        raise SystemExit(f"manifest-tag control did not discriminate:\n{result.stdout}{result.stderr}")

workflow_controls = [
    (
        ".github/workflows/release-cli.yml",
        "        shell: bash\n        env:\n          RELEASE_REF: ${{ inputs.release_ref }}\n        run: test",
        "        env:\n          RELEASE_REF: ${{ inputs.release_ref }}\n        run: test",
        "must select Bash explicitly",
        "Windows-shell",
    ),
    (
        ".github/workflows/release-cli.yml",
        "dtolnay/rust-toolchain@1.95.0\n        with:\n          targets: ${{ matrix.target }}",
        "dtolnay/rust-toolchain@stable\n        with:\n          targets: ${{ matrix.target }}",
        "pinned 1.95.0 build toolchain",
        "cross-target toolchain",
    ),
    (
        ".github/workflows/release-python.yml",
        "mapfile -t versions < <(sed -n",
        "mapfile -t package_versions < <(sed -n",
        "single-value package-version check",
        "Windows version extraction",
    ),
    (
        ".github/workflows/release-native.yml",
        "      - name: install locked packaging tools\n",
        "      - run: npm install --package-lock-only --prefix bindings/node\n"
        "      - name: install locked packaging tools\n",
        "must consume the committed versioned optional lock stubs",
        "native lock regeneration",
    ),
    (
        ".github/workflows/release.yml",
        '                "https://registry.npmjs.org/$encoded/$version" >/dev/null',
        '                "https://registry.npmjs.org/$encoded" >/dev/null',
        "top-level release completeness verdict lost",
        "public release completeness",
    ),
    (
        ".github/workflows/release.yml",
        '"python_publish":"success","browser":"skipped","cli":"skipped"',
        '"python_publish":"success","browser":"success","cli":"skipped"',
        "top-level release completeness verdict lost",
        "component-aware skip set",
    ),
    (
        ".github/workflows/release.yml",
        '          elif [ "$COMPONENT" = all ]; then\n            expected=\'{"tag":"success","crate":"success","native":"success","wasm":"success","python_publish":"success","browser":"success","cli":"success"}\'',
        '          elif [ "$COMPONENT" = all ]; then\n            expected=\'{"tag":"success","crate":"success","native":"success","wasm":"success","python_publish":"success","browser":"success","cli":"skipped"}\'',
        "must be stated separately",
        "all-with-skipped-leg",
    ),
    (
        ".github/workflows/release.yml",
        "    if: github.event_name == 'pull_request' || inputs.component == 'cli' || inputs.component == 'all'\n",
        "    if: github.event_name == 'pull_request' || inputs.component == 'cli'\n",
        "component dispatch cannot reach the cli job",
        "all dispatch coverage",
    ),
]

for relative, old, new, expected, label in workflow_controls:
    with tempfile.TemporaryDirectory() as temp:
        copy = pathlib.Path(temp) / "repo"
        copy_candidates(copy)
        workflow = copy / relative
        text = workflow.read_text()
        if old not in text:
            raise SystemExit(f"{label} control could not find the construct it is meant to perturb")
        workflow.write_text(text.replace(old, new, 1))
        result = run(copy)
        if result.returncode == 0 or expected not in result.stderr:
            raise SystemExit(f"{label} control did not discriminate:\n{result.stdout}{result.stderr}")

print(
    "release metadata controls: baseline passes; metadata drift, all four tag-only workflow "
    "defect classes, and incomplete release graphs are refused"
)
