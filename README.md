# ForeverDB Client

Desktop-Uploader für die ForeverCollect-SavedVariables. Erkennt die WoW-Installation (Faugus, Lutris, Wine, Bottles, Windows, macOS) über `.build.info` – unter Windows über die Registry-Einträge des Launchers (`HKLM\SOFTWARE\WOW6432Node\Blizzard Entertainment\World of Warcraft`), `%ProgramFiles(x86)%\World of Warcraft` sowie `<Laufwerk>:\World of Warcraft` und `<Laufwerk>:\Games\World of Warcraft` auf allen festen Laufwerken; sonst hilft „Installation hinzufügen“ –, zeigt die laut Launcher installierten Clients mit Version und Dateistatus und lädt die Datei mit einem Klick zum Ingress hoch. Ein Client ist ein Flavour-Ordner (`_classic_beta_`, `_classic_era_`, `_retail_` …): `.flavor.info` nennt das Produkt, `Wow*.exe` den zu überwachenden Prozess. Über „Client hinzufügen“ lassen sich weitere Flavour-Ordner der gefundenen Installationen aktivieren oder per nativem Ordnerdialog eine ganze WoW-Installation (Ordner „World of Warcraft“ – dann sind ihre laut Launcher installierten Clients aktiv) bzw. ein einzelner Client-Ordner; beides wird in `settings.json` gemerkt. Überschreibbar per Umgebungsvariable: `FOREVERDB_WOW_DIR`, `FOREVERDB_INGRESS_URL`.

Entwicklung: `pnpm tauri dev` — Build: `pnpm tauri build`.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Automatischer Upload

WoW schreibt die SavedVariables nur beim Ausloggen, bei `/reload` und beim Beenden. Die App prüft alle 3 s die `ForeverCollect.lua` jedes aktiven Clients und lädt jede neue Schreibung hoch, sobald die Datei 3 s unverändert ist und ihr Inhalt vom zuletzt hochgeladenen abweicht (Hash in `settings.json`) – unabhängig davon, ob das Spiel noch läuft. Ist die aktuelle Datei eine geleerte Datenbank (Zustand nach einem Upload), aber die vom Spiel angelegte `ForeverCollect.lua.bak` enthält noch eine Session, wird die `.bak` hochgeladen. Nach erfolgreichem Upload wird die Datei entfernt, sobald das Spiel geschlossen ist (eine hochgeladene `.bak` sofort), damit die nächste Sitzung leer beginnt. Von jedem Upload bleibt eine Kopie unter `~/.local/share/com.alex.foreverdb-client/archive/` (Windows: `%APPDATA%\com.alex.foreverdb-client\archive\`; die letzten 30). Schalter „Automatisch hochladen“ und Hashes liegen in `~/.config/com.alex.foreverdb-client/settings.json` (Windows: `%APPDATA%\com.alex.foreverdb-client\settings.json`); der Button „Jetzt hochladen“ bleibt für manuelle Uploads. Läuft das Spiel länger als 2 h, ohne dass die Datei geschrieben wurde, warnt die App (im Spiel `/fc save` oder `/fc autosave 30`); ebenso, wenn im `Errors/`-Ordner des Clients ein Absturzbericht jünger als die letzte Speicherung liegt – die Daten dieser Sitzung wurden dann nie geschrieben.

## Addon-Updates

Der Client fragt beim Start und alle 6 Stunden das neueste Release des Addon-Repositories (`alexbangert/forevercollect-addon`, überschreibbar per `FOREVERDB_ADDON_REPO`) ab und vergleicht es mit `## Version:` der installierten `Interface/AddOns/ForeverCollect/ForeverCollect.toc` je Client. Ist es neuer (oder das Addon fehlt), zeigt die Client-Karte „ForeverCollect vX.Y.Z verfügbar“ mit „Aktualisieren“/„Installieren“: Das Release-Zip wird geladen, geprüft (nur `ForeverCollect/`, TOC-Version muss zum Release passen) und der Addon-Ordner atomar ersetzt; bei laufendem Spiel gilt es ab dem nächsten Login.

Das Repository ist privat, daher braucht die Abfrage ein GitHub-Token (Fine-grained PAT, nur dieses Repository, Berechtigung „Contents: Read-only“). Reihenfolge: Umgebungsvariable `FOREVERDB_GITHUB_TOKEN` zur Laufzeit, sonst der beim Build einkompilierte Wert (`FOREVERDB_GITHUB_TOKEN=github_pat_… pnpm tauri build`), sonst `github_token` in `settings.json`. Das Token steht nie im Quellcode und wird dem Fenster nicht übergeben.

## Linux

Der Client läuft nativ (GTK 3 + WebKitGTK) und findet WoW in Wine-Prefixen von Faugus, Lutris, Bottles und `~/.wine`; das laufende Spiel wird über `/proc` erkannt. Deshalb kein Flatpak: In der Sandbox wären weder die Prefixe unter `~/.var/app/…` noch die `Wow*.exe`-Prozesse des Hosts sichtbar.

**Bauen** braucht Rust, Node + pnpm und die WebKitGTK-Entwicklungspakete:

- Arch/CachyOS: `webkit2gtk-4.1 gtk3 libsoup3 base-devel` (plus `cargo pnpm nodejs`)
- Debian/Ubuntu: `libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev librsvg2-dev build-essential`
- Fedora: `webkit2gtk4.1-devel gtk3-devel libsoup3-devel librsvg2-devel`

Dann `pnpm install` und `pnpm tauri build` (auf Arch-basierten Systemen `NO_STRIP=true pnpm tauri build`: das in `linuxdeploy` mitgelieferte alte `strip` kennt die `.relr.dyn`-Sektionen aktueller Bibliotheken nicht und lässt sonst den AppImage-Schritt scheitern). `src-tauri/tauri.linux.conf.json` wählt die Ziele; die Pakete liegen unter `src-tauri/target/release/bundle/`, benannt nach dem `productName`:

- `deb/ForeverDB Client_<Version>_amd64.deb` → `sudo apt install "./ForeverDB Client_<Version>_amd64.deb"`; Paketname ist `forever-db-client`, Abhängigkeiten `libwebkit2gtk-4.1-0`, `libgtk-3-0`
- `rpm/ForeverDB Client-<Version>-1.x86_64.rpm` → `sudo dnf install "./ForeverDB Client-<Version>-1.x86_64.rpm"`
- `appimage/ForeverDB Client_<Version>_amd64.AppImage` → ausführbar machen und starten; läuft ohne Installation, braucht aber eine glibc mindestens so neu wie die des Build-Systems. Beim Bauen lädt Tauri `linuxdeploy` nach (Netzzugang nötig). Nur ein Ziel: `pnpm tauri build --bundles appimage`.

**Arch/CachyOS** als Paket: `./install.sh` (oder `cd packaging/arch && makepkg -si`) baut aus dem Checkout (kein Download) und installiert Binary, `.desktop`-Datei und Icons; `pacman -Rns foreverdb-client` entfernt es wieder. `FOREVERDB_GITHUB_TOKEN` in der Umgebung wird wie bei `pnpm tauri build` einkompiliert (siehe „Addon-Updates“).

Datenpfade siehe „Automatischer Upload“. Bleibt das Fenster weiß oder leer (typisch Wayland mit NVIDIA), hilft `WEBKIT_DISABLE_DMABUF_RENDERER=1 foreverdb-client`.

## Windows

Der Client läuft nativ unter Windows 10/11 und braucht die WebView2-Runtime (dort vorinstalliert; der Installer lädt sie andernfalls nach). Der Build liefert einen NSIS-Installer, der ohne Administratorrechte für den aktuellen Benutzer installiert (`src-tauri/tauri.windows.conf.json`). Voraussetzungen zum Bauen: Rust (MSVC-Toolchain, Visual Studio Build Tools mit C++), Node + pnpm. Dann `pnpm install` und `pnpm tauri build`; der Installer liegt unter `src-tauri/target/release/bundle/nsis/ForeverDB Client_<Version>_x64-setup.exe`. `cargo test` in `src-tauri` prüft dort auch die Prozess- und Pfad-Erkennung (ToolHelp32, kanonische Pfade). Das laufende Spiel wird über die Prozessliste erkannt, Pfade aus dem Ordnerdialog werden auf ihre Schreibweise auf der Platte normalisiert, sodass derselbe Client nicht doppelt erscheint.
