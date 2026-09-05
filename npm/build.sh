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

# The single-file suite reads current containers, and this binding needs a producer fixture. The CLI
# is that producer, so it is built here
# rather than assumed: a test that silently skips when a binary is missing is a test that passes
# on the machine that needed it most.
cargo build --manifest-path "$root/Cargo.toml" --bin turndb
export TURNDB_CLI="$root/target/debug/turndb"

printf 'turndb.wasm: %s bytes\n' "$(stat -c%s "$pkg/turndb.wasm")"
node --test "$pkg"/test/*.mjs
