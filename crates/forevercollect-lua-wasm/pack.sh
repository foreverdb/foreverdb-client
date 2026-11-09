#!/usr/bin/env bash
# Builds the parser as WASM, checks it against the golden fixture and packs the npm
# tarball into dist/. Usage: pack.sh. The package carries the parser's own version (the
# `version` of crates/forevercollect-lua/Cargo.toml), not the uploader's: it changes only
# when the parser does, so the website's dependency stays put across uploader releases.
# Needs the target wasm32-unknown-unknown and wasm-bindgen-cli in the version pinned in
# Cargo.toml.
set -euo pipefail

crate_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_dir="$(dirname "$crate_dir")"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$workspace_dir/forevercollect-lua/Cargo.toml" | head -n1)"
if [ -z "$version" ]; then
  echo "no version in $workspace_dir/forevercollect-lua/Cargo.toml" >&2
  exit 1
fi

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
