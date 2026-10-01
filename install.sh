#!/usr/bin/env bash
# Builds the client from this checkout as an Arch package and installs it (pacman).
# FOREVERDB_GITHUB_TOKEN from the environment or src-tauri/github-token is compiled in (addon updates, see README).
set -euo pipefail

cd "$(dirname "$0")/packaging/arch"

command -v makepkg >/dev/null || { echo "makepkg is missing; this script is for Arch/CachyOS." >&2; exit 1; }
[[ -n "${FOREVERDB_GITHUB_TOKEN:-}" || -s ../../src-tauri/github-token ]] || echo "Note: neither FOREVERDB_GITHUB_TOKEN nor src-tauri/github-token is set, so the addon update check stays off." >&2

# -s: install missing dependencies, -f: overwrite an existing package,
# -i: install (asks for sudo), -c: remove the build folder afterwards.
makepkg -sfic

rm -f ./*.pkg.tar.*
echo "Installed: $(pacman -Q foreverdb-client)"
