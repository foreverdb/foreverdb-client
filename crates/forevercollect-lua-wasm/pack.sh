#!/usr/bin/env bash
# Builds the parser as WASM, checks it against the golden fixture and packs the npm
# tarball into dist/. Usage: pack.sh [version]  (default 0.0.0-dev; the release passes
# the uploader version). Needs the target wasm32-unknown-unknown and wasm-bindgen-cli
# in the version pinned in Cargo.toml.
set -euo pipefail

version="${1:-0.0.0-dev}"
crate_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(dirname "$crate_dir")"

cd "$workspace_dir"
cargo build -p forevercollect-lua-wasm --release --target wasm32-unknown-unknown

cd "$crate_dir"
rm -rf pkg
wasm-bindgen --target web --out-dir pkg \
  "$workspace_dir/target/wasm32-unknown-unknown/release/forevercollect_lua_wasm.wasm"
cp package.json pkg/
(cd pkg && npm pkg set version="$version")

node test/golden.mjs

mkdir -p dist
npm pack ./pkg --pack-destination dist
