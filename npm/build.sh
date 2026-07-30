#!/usr/bin/env bash
# Build the npm package from source. No C toolchain, no prebuild matrix — one artifact.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
pkg="$here/turndb"

rustup target list --installed | grep -q wasm32-wasip1 || rustup target add wasm32-wasip1

cargo build --manifest-path "$root/Cargo.toml" \
  --profile wasm-release --target wasm32-wasip1 -p turndb-wasm

cp "$root/target/wasm32-wasip1/wasm-release/turndb_wasm.wasm" "$pkg/turndb.wasm"
cp "$root/LICENSE" "$root/NOTICE" "$pkg/"

printf 'turndb.wasm: %s bytes\n' "$(stat -c%s "$pkg/turndb.wasm")"
node --test "$pkg"/test/*.mjs
