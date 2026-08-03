#!/usr/bin/env bash
# Two-way byte/metadata compatibility across the native C zstd and portable Rust zstd builds.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/turndb-interop.XXXXXX")"
portable="$scratch/portable-store"
native="$scratch/native-store"
trap 'rm -rf -- "$scratch"' EXIT

# Node's WASI preopen validates the host directory before the guest can run Store::open.
mkdir -p "$portable" "$native"

if [ ! -f "$here/turndb/turndb.wasm" ]; then
  echo "cross-runtime: npm/turndb/turndb.wasm is absent; run npm/build.sh first" >&2
  exit 1
fi

node "$here/interop.mjs" write-portable "$portable" 64
cargo run --quiet --manifest-path "$root/Cargo.toml" --no-default-features \
  --example wasm_read -- "$portable" 64

cargo run --quiet --manifest-path "$root/Cargo.toml" --no-default-features \
  --example wasm_smoke -- "$native" 64
node "$here/interop.mjs" read-portable "$native" 64

printf 'cross-runtime: native and WASI stores agree byte-for-byte in both directions\n'
