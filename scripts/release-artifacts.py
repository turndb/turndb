#!/usr/bin/env python3
"""Create or verify the byte manifest carried between build, install, and publish jobs."""

import argparse
import hashlib
import json
import pathlib
import subprocess


def digest(path: pathlib.Path) -> dict[str, object]:
    data = path.read_bytes()
    return {
        "file": path.name,
        "bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }


def create(args: argparse.Namespace) -> None:
    paths = [pathlib.Path(value).resolve() for value in args.files]
    names = [path.name for path in paths]
    if len(names) != len(set(names)):
        raise SystemExit(f"artifact filenames must be unique: {names}")
    missing = [str(path) for path in paths if not path.is_file()]
    if missing:
        raise SystemExit("missing release artifact(s): " + ", ".join(missing))
    commit = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], text=True, encoding="utf-8"
    ).strip()
    manifest = {
        "schema": 1,
        "component": args.component,
        "version": args.version,
        "sourceCommit": commit,
        "files": [digest(path) for path in sorted(paths, key=lambda path: path.name)],
    }
    output = pathlib.Path(args.output)
    output.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {output}: {len(paths)} files at {commit}")


def verify(args: argparse.Namespace) -> None:
    manifest_path = pathlib.Path(args.manifest)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    schema = manifest.get("schema")
    if schema == 1:
        entries = manifest.get("files")
        component = manifest.get("component")
    elif schema == 2:
        # The native packer carries the same digest contract together with
        # target-specific build metadata used by its install test.
        entries = manifest.get("tarballs")
        component = manifest.get("package")
    else:
        entries = None
        component = None
    if not entries or not component:
        raise SystemExit(f"unsupported or empty release artifact manifest: {manifest_path}")
    directory = pathlib.Path(args.directory)
    for expected in entries:
        path = directory / expected["file"]
        actual = digest(path)
        if actual != expected:
            raise SystemExit(
                f"release artifact differs from build manifest: {path}\n"
                f"expected {expected}\nactual   {actual}"
            )
    print(
        f"verified {component} {manifest['version']} from "
        f"{manifest['sourceCommit']}: {len(entries)} files"
    )


parser = argparse.ArgumentParser()
subparsers = parser.add_subparsers(dest="command", required=True)
create_parser = subparsers.add_parser("create")
create_parser.add_argument("--component", required=True)
create_parser.add_argument("--version", required=True)
create_parser.add_argument("--output", required=True)
create_parser.add_argument("files", nargs="+")
create_parser.set_defaults(function=create)
verify_parser = subparsers.add_parser("verify")
verify_parser.add_argument("--manifest", required=True)
verify_parser.add_argument("--directory", required=True)
verify_parser.set_defaults(function=verify)
arguments = parser.parse_args()
arguments.function(arguments)
