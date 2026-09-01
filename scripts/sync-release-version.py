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

# PyPI reads PEP 621 metadata rather than Cargo's package table. Knope owns the Cargo member
# version; keep the Python distribution's independent metadata in the same lockstep release.
pyproject_path = ROOT / "bindings/python/pyproject.toml"
pyproject = pyproject_path.read_text()
pyproject, replacements = re.subn(
    r'(?ms)(\[project\].*?^version = ")[^"]+("$)',
    rf'\g<1>{VERSION}\g<2>',
    pyproject,
    count=1,
)
if replacements != 1:
    raise SystemExit("could not update bindings/python/pyproject.toml project.version")
pyproject_path.write_text(pyproject)

python_cargo_path = ROOT / "bindings/python/Cargo.toml"
python_cargo = python_cargo_path.read_text()
python_cargo, replacements = re.subn(
    r'(turndb = \{ path = "\.\./\.\.", version = ")[^"]+("[^\n]*\})',
    rf'\g<1>{VERSION}\g<2>',
    python_cargo,
    count=1,
)
if replacements != 1:
    raise SystemExit("could not update the Python binding's core version requirement")
python_cargo_path.write_text(python_cargo)

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

# Edit the lock directly rather than resolving it against the registry. Each optional platform
# entry records the exact requested version but deliberately has no resolved URL or integrity:
# npm accepts that shape whether the version is unpublished (the release PR) or published (main
# after the release), and an unavailable optional package remains optional. One representation is
# therefore valid on both sides of first publication; there is no post-publication repair step.
lock_path = ROOT / "bindings/node/package-lock.json"
npm_lock = json.loads(lock_path.read_text())
npm_lock["version"] = VERSION
npm_lock["packages"][""]["version"] = VERSION
platform_manifests = {
    manifest["name"]: manifest
    for path in sorted((ROOT / "bindings/node/npm").glob("*/package.json"))
    for manifest in [json.loads(path.read_text())]
}
for name in [n for n in npm_lock["packages"][""].get("optionalDependencies", {}) if n.startswith("@turndb/")]:
    npm_lock["packages"][""]["optionalDependencies"][name] = VERSION
    platform_manifest = platform_manifests[name]
    npm_lock["packages"][f"node_modules/{name}"] = {
        "version": VERSION,
        "optional": True,
        **{key: platform_manifest[key] for key in ("cpu", "os", "libc") if key in platform_manifest},
    }
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
