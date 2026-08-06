#!/usr/bin/env bash
set -euo pipefail

tag="${1:-}"
if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: ensure-release-tag.sh vX.Y.Z" >&2
  exit 2
fi

head="$(git rev-parse HEAD)"
if git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null; then
  test "$(git cat-file -t "$tag")" = tag
  test "$(git rev-parse "$tag^{}")" = "$head"
else
  git tag -a "$tag" -m "$tag" HEAD
fi

remote_tag="$(git ls-remote origin "refs/tags/$tag" | awk 'NR == 1 {print $1}')"
remote_commit="$(git ls-remote origin "refs/tags/$tag^{}" | awk 'NR == 1 {print $1}')"
if test -n "$remote_tag"; then
  # An annotated remote tag has a peeled ref. A lightweight tag or a tag aimed elsewhere is an
  # unrecoverable release disagreement, never something this workflow moves.
  test -n "$remote_commit"
  test "$remote_commit" = "$head"
else
  git push origin "refs/tags/$tag"
fi

echo "verified annotated $tag at $head"
