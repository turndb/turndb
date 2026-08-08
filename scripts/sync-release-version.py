#!/usr/bin/env python3
"""Synchronize the native selector's generated dependency pin after Knope bumps versions."""

import json
import pathlib
import re
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
VERSION = sys.argv[1] if len(sys.argv) == 2 else ""

if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", VERSION):
    raise SystemExit("usage: sync-release-version.py X.Y.Z")

# Every selector package pins its platform packages by exact version, and Knope bumps the
# `version` field of each manifest without touching the pins that reference them. Naming one
# selector here worked while there was one; the CLI's arrived, its pin stayed at the previous
# version, and the release failed at pack time with a selector that could never resolve a binary.
# Discover them instead: any @turndb/* optional dependency is a sibling in this repository and
# moves in lockstep by definition.
SELECTORS = ["bindings/node/package.json", "cli/package.json"]
synced = 0
for relative in SELECTORS:
    manifest_path = ROOT / relative
    if not manifest_path.is_file():
        raise SystemExit(f"selector manifest does not exist: {relative}")
    manifest = json.loads(manifest_path.read_text())
    pins = manifest.get("optionalDependencies", {})
    siblings = [name for name in pins if name.startswith("@turndb/")]
    if not siblings:
        raise SystemExit(f"{relative} pins no @turndb platform package; is it still a selector?")
    for name in siblings:
        pins[name] = VERSION
        synced += 1
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
print(f"synced {synced} platform pins across {len(SELECTORS)} selectors to {VERSION}")

# Knope updates the root Cargo.lock entry but, with explicit workspace member manifests in
# versioned_files, leaves member entries at their old versions. Ask Cargo to update every local
# package that still differs; deriving the names from the lock keeps a new workspace member from
# silently escaping this step.
lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
stale_local_packages = [
    package["name"]
    for package in lock.get("package", [])
    if "source" not in package and package.get("version") != VERSION
]
for package_name_in_lock in stale_local_packages:
    subprocess.run(
        ["cargo", "update", "-p", package_name_in_lock, "--precise", VERSION],
        cwd=ROOT,
        check=True,
    )

# The lock file is edited directly rather than through `npm install --package-lock-only`: npm
# regenerates the lock by resolving against the registry, and the platform package's new version
# is unpublished at prepare time — this release is what publishes it — so npm silently drops the
# unresolvable optional dependency's entry and `npm ci` then refuses the lock as out of sync.
#
# The platform package's entry is CONVERTED to the version-less `{"optional": true}` stub rather
# than preserved: npm accepts the stub exactly while the pinned version is absent from the
# registry — which is the release branch's whole lifetime — and refuses it afterwards. The
# matching post-publication step (see the release PR body) restores the resolved entry on `main`
# with `npm install --package-lock-only` once the version exists.
lock_path = ROOT / "bindings/node/package-lock.json"
npm_lock = json.loads(lock_path.read_text())
npm_lock["version"] = VERSION
npm_lock["packages"][""]["version"] = VERSION
for name in [n for n in npm_lock["packages"][""].get("optionalDependencies", {}) if n.startswith("@turndb/")]:
    npm_lock["packages"][""]["optionalDependencies"][name] = VERSION
    npm_lock["packages"][f"node_modules/{name}"] = {"optional": True}
lock_path.write_text(json.dumps(npm_lock, indent=2) + "\n")

# THIRD_PARTY_LICENSES.html lists the workspace's own crates with their versions, so a version
# bump stales the committed report even though no license changed. Rewrite exactly those entries;
# CI remains the arbiter — check-third-party-licenses.sh regenerates the report from scratch and
# byte-compares, so a wrong substitution fails the release PR rather than shipping.
report_path = ROOT / "THIRD_PARTY_LICENSES.html"
report = report_path.read_text()
workspace_names = {package["name"] for package in lock.get("package", []) if "source" not in package}
for name in sorted(workspace_names):
    report = re.sub(
        rf"(>{re.escape(name)} )\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(</a>)",
        rf"\g<1>{VERSION}\g<2>",
        report,
    )
report_path.write_text(report)
