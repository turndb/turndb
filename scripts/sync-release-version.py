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

# npm owns the lock-file representation. Asking it to update only the lock avoids duplicating that
# schema here while --ignore-scripts keeps a version bump from building or publishing anything.
subprocess.run(
    ["npm", "install", "--package-lock-only", "--ignore-scripts", "--no-audit", "--no-fund"],
    cwd=ROOT / "bindings/node",
    check=True,
)
