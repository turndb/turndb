#!/usr/bin/env python3
"""Validate changesets and every lockstep version source declared in knope.toml."""

import json
import pathlib
import re
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
CONFIG = tomllib.loads((ROOT / "knope.toml").read_text())
VERSIONED_FILES = CONFIG["package"]["versioned_files"]
ALLOWED_BUMPS = {"major", "minor", "patch"}
SEMVER = re.compile(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?\Z")


def fail(message: str) -> None:
    raise SystemExit(message)


def versions_for(relative: str) -> list[tuple[str, str]]:
    path = ROOT / relative
    if not path.is_file():
        fail(f"knope.toml versioned_files entry does not exist: {relative}")
    if path.name == "Cargo.lock":
        packages = tomllib.loads(path.read_text()).get("package", [])
        local = [(f"{relative}:{p['name']}", p["version"]) for p in packages if "source" not in p]
        if not local:
            fail(f"no workspace packages found in {relative}")
        return local
    if path.suffix == ".toml":
        data = tomllib.loads(path.read_text())
        version = data.get("package", {}).get("version")
        return [(relative, version)]
    if path.suffix == ".json":
        data = json.loads(path.read_text())
        found = [(relative, data.get("version"))]
        if path.name == "package-lock.json":
            root_version = data.get("packages", {}).get("", {}).get("version")
            found.append((f"{relative}:packages['']", root_version))
        return found
    fail(f"unsupported knope.toml versioned_files entry: {relative}")


def check_versions() -> str:
    if not isinstance(VERSIONED_FILES, list) or not VERSIONED_FILES:
        fail("knope.toml package.versioned_files must be a non-empty list")
    if not all(isinstance(item, str) for item in VERSIONED_FILES):
        fail("release detector supports only path entries in package.versioned_files")
    observed = [entry for relative in VERSIONED_FILES for entry in versions_for(relative)]
    python_version = tomllib.loads((ROOT / "bindings/python/pyproject.toml").read_text())["project"]["version"]
    observed.append(("bindings/python/pyproject.toml:project", python_version))
    python_core_version = tomllib.loads((ROOT / "bindings/python/Cargo.toml").read_text())["dependencies"]["turndb"]["version"]
    observed.append(("bindings/python/Cargo.toml:dependencies.turndb", python_core_version))
    reference = observed[0][1]
    if not isinstance(reference, str) or not SEMVER.fullmatch(reference):
        fail(f"invalid release version at {observed[0][0]}: {reference!r}")
    wrong = [(where, value) for where, value in observed if value != reference]
    if wrong:
        fail("lockstep version mismatch: " + ", ".join(f"{where}={value!r}" for where, value in wrong))

    # Every selector's platform pins, not just the native one's. A pin left behind produces a
    # package that installs cleanly and cannot run: npm treats a missing OPTIONAL dependency as a
    # successful install, so the failure surfaces as a launcher that finds nothing.
    for relative in ("bindings/node/package.json", "cli/package.json"):
        path = ROOT / relative
        if not path.is_file():
            continue
        pins = json.loads(path.read_text()).get("optionalDependencies", {})
        stale = {
            name: pinned
            for name, pinned in pins.items()
            if name.startswith("@turndb/") and pinned != reference
        }
        if stale:
            fail(
                f"{relative} pins "
                + ", ".join(f"{n}@{v}" for n, v in sorted(stale.items()))
                + f" but this release is {reference}"
            )

    selector = json.loads((ROOT / "bindings/node/package.json").read_text())
    selector_lock = json.loads((ROOT / "bindings/node/package-lock.json").read_text())
    platforms = [
        json.loads(path.read_text())
        for path in sorted((ROOT / "bindings/node/npm").glob("*/package.json"))
    ]
    for platform in platforms:
        pin = selector.get("optionalDependencies", {}).get(platform.get("name"))
        if pin != reference or platform.get("version") != reference:
            fail(
                f"selector pin {pin!r} / platform version {platform.get('version')!r} "
                f"for {platform.get('name')} do not equal {reference!r}"
            )
    lock_versions = [selector_lock.get("version"), selector_lock.get("packages", {}).get("", {}).get("version")]
    if lock_versions != [reference, reference]:
        fail(f"selector lock versions {lock_versions!r} do not equal {reference!r}")
    lock_pins = selector_lock.get("packages", {}).get("", {}).get("optionalDependencies", {})
    for platform in platforms:
        lock_pin = lock_pins.get(platform.get("name"))
        if lock_pin != reference:
            fail(
                f"selector lock pin {lock_pin!r} for {platform.get('name')} "
                f"does not equal platform version {reference!r}"
            )
        lock_entry = selector_lock.get("packages", {}).get(f"node_modules/{platform.get('name')}")
        expected_entry = {
            "version": reference,
            "optional": True,
            **{key: platform[key] for key in ("cpu", "os", "libc") if key in platform},
        }
        if lock_entry != expected_entry:
            fail(
                f"selector lock entry for {platform.get('name')} is {lock_entry!r}; "
                f"expected unresolved versioned optional stub {expected_entry!r}"
            )
    print(f"release metadata: {len(VERSIONED_FILES)} versioned_files, version {reference}, pin aligned")
    return reference


def check_changesets() -> None:
    directory = ROOT / ".changeset"
    for path in sorted(directory.glob("*.md")) if directory.is_dir() else []:
        if path.name.lower() == "readme.md":
            continue
        lines = path.read_text().splitlines()
        if len(lines) < 4 or lines[0] != "---":
            fail(f"{path.relative_to(ROOT)}: missing opening changeset frontmatter")
        try:
            end = lines.index("---", 1)
        except ValueError:
            fail(f"{path.relative_to(ROOT)}: missing closing changeset frontmatter")
        entries = [line for line in lines[1:end] if line.strip()]
        if len(entries) != 1 or ":" not in entries[0]:
            fail(f"{path.relative_to(ROOT)}: expected exactly one package bump")
        package, bump = (part.strip() for part in entries[0].split(":", 1))
        if package != "default":
            fail(f"{path.relative_to(ROOT)}: unknown package {package!r}; expected 'default'")
        if bump not in ALLOWED_BUMPS:
            fail(f"{path.relative_to(ROOT)}: unknown bump {bump!r}")
        if not any(line.strip() for line in lines[end + 1 :]):
            fail(f"{path.relative_to(ROOT)}: missing release note")


def check_tag_contract() -> None:
    text_files = [
        ROOT / ".github/workflows/release-crate.yml",
        ROOT / ".github/workflows/release-native.yml",
        ROOT / "bindings/node/scripts/publish-prebuild.cjs",
        ROOT / "CONTRIBUTING.md",
        ROOT / "docs/native-prebuilds.md",
    ]
    obsolete = []
    for path in text_files:
        for number, line in enumerate(path.read_text().splitlines(), 1):
            if re.search(r"(?:native-v|npm-v)(?:X|\d|\[|\$|`)", line):
                obsolete.append(f"{path.relative_to(ROOT)}:{number}")
    if obsolete:
        fail("obsolete release tag namespace: " + ", ".join(obsolete))

    crate = text_files[0].read_text()
    native = text_files[1].read_text()
    publisher = text_files[2].read_text()
    contributing = text_files[3].read_text()
    shell_pattern = 'case "$RELEASE_REF" in v[0-9]*.[0-9]*.[0-9]*)'
    if shell_pattern not in crate or shell_pattern not in native:
        fail("both release workflows must accept only the lockstep vX.Y.Z namespace")
    if "const expectedTag = `v${version}`;" not in publisher:
        fail("native publisher does not derive the lockstep tag from its manifest")
    for command in (
        'test "$(git describe --tags --exact-match HEAD)" = "vX.Y.Z"',
        'test "$(git cat-file -t vX.Y.Z)" = tag',
    ):
        if command not in contributing:
            fail(f"portable publish procedure lost tag check: {command}")


def check_release_workflow_runtime_contract() -> None:
    """Keep tag-only workflows aligned with the shells and toolchain they actually execute."""
    cli = (ROOT / ".github/workflows/release-cli.yml").read_text()
    python = (ROOT / ".github/workflows/release-python.yml").read_text()
    native = (ROOT / ".github/workflows/release-native.yml").read_text()

    cli_version_check = """      - name: verify lockstep package version matches the tag
        shell: bash
"""
    if cli_version_check not in cli:
        fail("CLI release version check must select Bash explicitly on Windows")
    if "dtolnay/rust-toolchain@1.95.0\n        with:\n          targets: ${{ matrix.target }}" not in cli:
        fail("CLI release targets must be installed for the pinned 1.95.0 build toolchain")

    if 're.findall(r"(?m)^version = \\\\"([^\\\\"]+)\\\\"$"' in python:
        fail("Windows wheel release still contains the double-escaped version extractor")
    if "mapfile -t versions < <(sed -n" not in python:
        fail("Windows wheel release lost its single-value package-version check")
    if "dtolnay/rust-toolchain@stable" in python:
        fail("Python release jobs must use the repository's pinned 1.95.0 toolchain")

    if "npm install --package-lock-only" in native:
        fail("native release must consume the committed versioned optional lock stubs")
    if "npm ci --ignore-scripts --no-audit --no-fund --prefix bindings/node" not in native:
        fail("native release lost the locked dependency-tree refusal")

    release = (ROOT / ".github/workflows/release.yml").read_text()
    component = re.search(
        r"(?ms)^      component:\n(?P<body>.*?)(?=^      [a-z_]+:\n|^jobs:\n)", release
    )
    if component is None:
        fail("top-level release lost its component dispatch contract")
    options = re.findall(r"^          - (\S+)$", component.group("body"), re.MULTILINE)
    expected_options = ["crate", "native", "wasm", "python", "browser", "cli", "all"]
    if options != expected_options:
        fail(f"top-level release component choices {options!r} do not equal {expected_options!r}")
    if "python = wheels + PyPI" not in component.group("body") or "published registry versions are refused" not in component.group("body"):
        fail("component input must name Python's scope and the rerun refusal")

    dispatch_conditions = {
        "crate": "github.event_name == 'pull_request' || inputs.component == 'crate' || inputs.component == 'all'",
        "native": "github.event_name == 'pull_request' || inputs.component == 'native' || inputs.component == 'all'",
        "wasm": "github.event_name == 'pull_request' || inputs.component == 'wasm' || inputs.component == 'all'",
        "python": "github.event_name == 'pull_request' || inputs.component == 'python' || inputs.component == 'all'",
        "python_publish": "github.event_name == 'pull_request' || inputs.component == 'python' || inputs.component == 'all'",
        "browser": "github.event_name == 'pull_request' || inputs.component == 'browser' || inputs.component == 'all'",
        "cli": "github.event_name == 'pull_request' || inputs.component == 'cli' || inputs.component == 'all'",
    }
    for job, condition in dispatch_conditions.items():
        block = re.search(rf"(?ms)^  {job}:\n(?P<body>.*?)(?=^  [a-z_]+:\n|\Z)", release)
        if block is None or f"    if: {condition}" not in block.group("body"):
            fail(f"component dispatch cannot reach the {job} job under its complete contract")

    complete = re.search(r"(?ms)^  complete:\n(?P<body>.*?)(?=^  [a-z_]+:\n|\Z)", release)
    if complete is None:
        fail("top-level release lost its public completeness verdict")
    complete_body = complete.group("body")
    success = '{"tag":"success","crate":"success","native":"success","wasm":"success","python_publish":"success","browser":"success","cli":"success"}'
    maps = {
        "crate": '{"tag":"success","crate":"success","native":"skipped","wasm":"skipped","python_publish":"skipped","browser":"skipped","cli":"skipped"}',
        "native": '{"tag":"success","crate":"skipped","native":"success","wasm":"skipped","python_publish":"skipped","browser":"skipped","cli":"skipped"}',
        "wasm": '{"tag":"success","crate":"skipped","native":"skipped","wasm":"success","python_publish":"skipped","browser":"skipped","cli":"skipped"}',
        "python": '{"tag":"success","crate":"skipped","native":"skipped","wasm":"skipped","python_publish":"success","browser":"skipped","cli":"skipped"}',
        "browser": '{"tag":"success","crate":"skipped","native":"skipped","wasm":"skipped","python_publish":"skipped","browser":"success","cli":"skipped"}',
        "cli": '{"tag":"success","crate":"skipped","native":"skipped","wasm":"skipped","python_publish":"skipped","browser":"skipped","cli":"success"}',
    }
    for label, marker in (("pull_request", success), ("all", success), *maps.items()):
        if f"expected='{marker}'" not in complete_body:
            fail(f"top-level release completeness verdict lost its {label} expected map")
    if complete_body.count(f"expected='{success}'") != 2:
        fail("pull-request and all recovery expected maps must be stated separately")

    for required in (
        "needs: [tag, crate, native, wasm, python_publish, browser, cli]",
        "if: always() && !cancelled() && needs.tag.result == 'success'",
        "RESULTS: ${{ toJSON(needs) }}",
        "with_entries(.value = .value.result)",
        'https://registry.npmjs.org/$encoded/$version',
        'https://crates.io/api/v1/crates/turndb/$version',
        'https://pypi.org/pypi/turndb/$version/json',
        "grep -Fx 'turndb-viewer.html'",
    ):
        if required not in complete_body:
            fail(f"top-level release completeness verdict lost: {required}")


if __name__ == "__main__":
    check_versions()
    check_changesets()
    check_tag_contract()
    check_release_workflow_runtime_contract()
    print("changesets: all configured packages and bump types are valid")
