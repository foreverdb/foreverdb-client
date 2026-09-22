#!/usr/bin/env bash
# Baut den Client aus diesem Checkout als Arch-Paket und installiert es (pacman).
# FOREVERDB_GITHUB_TOKEN in der Umgebung wird einkompiliert (Addon-Updates, siehe README).
set -euo pipefail

cd "$(dirname "$0")/packaging/arch"

command -v makepkg >/dev/null || { echo "makepkg fehlt – dieses Script ist für Arch/CachyOS." >&2; exit 1; }
[[ -n "${FOREVERDB_GITHUB_TOKEN:-}" ]] || echo "Hinweis: FOREVERDB_GITHUB_TOKEN nicht gesetzt, Addon-Update-Prüfung bleibt aus." >&2

# -s: fehlende Abhängigkeiten nachinstallieren, -f: vorhandenes Paket überschreiben,
# -i: installieren (fragt nach sudo), -c: Build-Ordner danach entfernen.
makepkg -sfic

rm -f ./*.pkg.tar.*
echo "Installiert: $(pacman -Q foreverdb-client)"
