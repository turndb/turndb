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

manifest_path = ROOT / "bindings/node/package.json"
manifest = json.loads(manifest_path.read_text())
package_name = "@turndb/native-linux-x64-gnu"
if package_name not in manifest.get("optionalDependencies", {}):
    raise SystemExit(f"selector has no optional dependency for {package_name}")
manifest["optionalDependencies"][package_name] = VERSION
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")

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
# Only the three version fields change; every other byte of the lock, including the platform
# package's version-less `"optional": true` entry, is preserved.
lock_path = ROOT / "bindings/node/package-lock.json"
npm_lock = json.loads(lock_path.read_text())
npm_lock["version"] = VERSION
npm_lock["packages"][""]["version"] = VERSION
npm_lock["packages"][""]["optionalDependencies"][package_name] = VERSION
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
