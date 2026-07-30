#!/usr/bin/env bash
# Refuse to publish an artifact that cannot be reproduced from a commit.
#
# `build.sh` rebuilds the wasm from whatever is in the working tree, which makes a STALE artifact a
# non-problem: it gets repaired rather than refused, and refusing it would only tell the publisher to
# run the command the hook is about to run anyway.
#
# What rebuilding does NOT give you is the property the gate actually needs. A rebuild binds the
# artifact to the WORKING TREE, and `npm publish` from a dirty tree therefore ships a binary built
# from source that exists in no commit — unreproducible from the tag, and invisible afterwards
# because the wasm is gitignored and nobody can diff it against anything.
#
# So: staleness is repaired, uncommittedness is refused. Those are different failures and only one of
# them is fixable by building.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"

cd "$root"

if ! git rev-parse --git-dir >/dev/null 2>&1; then
  echo "prepublish: not a git checkout, so the artifact cannot be tied to a commit — refusing." >&2
  exit 1
fi

dirty="$(git status --porcelain --untracked-files=no)"
if [ -n "$dirty" ]; then
  echo "prepublish: the working tree has uncommitted changes, so the wasm this would build exists" >&2
  echo "in no commit and cannot be reproduced from the tag. Commit or stash first." >&2
  echo >&2
  echo "$dirty" >&2
  exit 1
fi

printf 'prepublish: tree clean at %s\n' "$(git rev-parse --short HEAD)"
