#!/usr/bin/env bash
set -euo pipefail

if [[ "$(cargo about --version)" != "cargo-about 0.9.1" ]]; then
  echo "third-party report requires cargo-about 0.9.1" >&2
  exit 1
fi

report_check="$(mktemp)"
trap 'rm -f "$report_check"' EXIT

cargo about generate about.hbs \
  --manifest-path bindings/node/Cargo.toml \
  --all-features \
  --target x86_64-unknown-linux-gnu \
  --frozen \
  --fail \
  --output-file "$report_check"

# License texts come from many upstream packages and occasionally contain CRLF endings or trailing
# blanks. Normalize presentation-only whitespace so the checked-in report remains diff-clean while
# preserving every license word.
sed -i -e 's/\r$//' -e 's/[[:blank:]]*$//' "$report_check"

if ! cmp THIRD_PARTY_LICENSES.html "$report_check"; then
  echo "THIRD_PARTY_LICENSES.html is stale; regenerate it with cargo-about 0.9.1" >&2
  exit 1
fi
