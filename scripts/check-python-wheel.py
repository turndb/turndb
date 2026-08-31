#!/usr/bin/env python3
"""Audit a built wheel before a closed-index install."""

import argparse
import email
import pathlib
import zipfile


parser = argparse.ArgumentParser()
parser.add_argument("wheel", type=pathlib.Path)
parser.add_argument("--version", required=True)
parser.add_argument("--platform", required=True)
args = parser.parse_args()

if args.platform not in args.wheel.name:
    raise SystemExit(f"{args.wheel.name} is not tagged for {args.platform}")
with zipfile.ZipFile(args.wheel) as archive:
    metadata_names = [name for name in archive.namelist() if name.endswith(".dist-info/METADATA")]
    if len(metadata_names) != 1:
        raise SystemExit(f"wheel must carry exactly one METADATA file: {metadata_names}")
    metadata = email.message_from_bytes(archive.read(metadata_names[0]))
if metadata["Name"] != "turndb" or metadata["Version"] != args.version:
    raise SystemExit(
        f"wheel identity mismatch: {metadata['Name']} {metadata['Version']} != turndb {args.version}"
    )
dependencies = metadata.get_all("Requires-Dist", [])
unconditional = [requirement for requirement in dependencies if "; extra ==" not in requirement]
if unconditional:
    raise SystemExit(f"closed-index install has undeclared inputs: {unconditional}")
print(
    f"audited {args.wheel.name}: turndb {args.version}, "
    f"no unconditional runtime dependencies"
)
