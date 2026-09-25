#!/usr/bin/env bash
# Baut den Client aus diesem Checkout als Arch-Paket und installiert es (pacman).
# FOREVERDB_GITHUB_TOKEN in der Umgebung bzw. src-tauri/github-token wird einkompiliert (Addon-Updates, siehe README).
set -euo pipefail

cd "$(dirname "$0")/packaging/arch"

command -v makepkg >/dev/null || { echo "makepkg fehlt – dieses Script ist für Arch/CachyOS." >&2; exit 1; }
[[ -n "${FOREVERDB_GITHUB_TOKEN:-}" || -s ../../src-tauri/github-token ]] || echo "Hinweis: weder FOREVERDB_GITHUB_TOKEN noch src-tauri/github-token gesetzt, Addon-Update-Prüfung bleibt aus." >&2

# -s: fehlende Abhängigkeiten nachinstallieren, -f: vorhandenes Paket überschreiben,
# -i: installieren (fragt nach sudo), -c: Build-Ordner danach entfernen.
makepkg -sfic

rm -f ./*.pkg.tar.*
echo "Installiert: $(pacman -Q foreverdb-client)"
