# ForeverDB Client

Desktop-Uploader für die ForeverCollect-SavedVariables. Erkennt die WoW-Installation (Faugus, Lutris, Wine, Bottles, Windows, macOS) über `.build.info`, zeigt die laut Launcher installierten Clients mit Version und Dateistatus und lädt die Datei mit einem Klick zum Ingress hoch. Ein Client ist ein Flavour-Ordner (`_classic_beta_`, `_classic_era_`, `_retail_` …): `.flavor.info` nennt das Produkt, `Wow*.exe` den zu überwachenden Prozess. Über „Client hinzufügen“ lassen sich weitere Flavour-Ordner der gefundenen Installationen aktivieren oder per nativem Ordnerdialog eine ganze WoW-Installation (Ordner „World of Warcraft“ – dann sind ihre laut Launcher installierten Clients aktiv) bzw. ein einzelner Client-Ordner; beides wird in `settings.json` gemerkt. Überschreibbar per Umgebungsvariable: `FOREVERDB_WOW_DIR`, `FOREVERDB_INGRESS_URL`.

Entwicklung: `pnpm tauri dev` — Build: `pnpm tauri build`.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)

## Automatischer Upload

WoW schreibt die SavedVariables nur beim Ausloggen, bei `/reload` und beim Beenden. Die App prüft alle 3 s die `ForeverCollect.lua` jedes aktiven Clients und lädt jede neue Schreibung hoch, sobald die Datei 3 s unverändert ist und ihr Inhalt vom zuletzt hochgeladenen abweicht (Hash in `settings.json`) – unabhängig davon, ob das Spiel noch läuft. Ist die aktuelle Datei eine geleerte Datenbank (Zustand nach einem Upload), aber die vom Spiel angelegte `ForeverCollect.lua.bak` enthält noch eine Session, wird die `.bak` hochgeladen. Nach erfolgreichem Upload wird die Datei entfernt, sobald das Spiel geschlossen ist (eine hochgeladene `.bak` sofort), damit die nächste Sitzung leer beginnt. Von jedem Upload bleibt eine Kopie unter `~/.local/share/com.alex.foreverdb-client/archive/` (die letzten 30). Schalter „Automatisch hochladen“ und Hashes liegen in `~/.config/com.alex.foreverdb-client/settings.json`; der Button „Jetzt hochladen“ bleibt für manuelle Uploads. Läuft das Spiel länger als 2 h, ohne dass die Datei geschrieben wurde, warnt die App (im Spiel `/fc save` oder `/fc autosave 30`); ebenso, wenn im `Errors/`-Ordner des Clients ein Absturzbericht jünger als die letzte Speicherung liegt – die Daten dieser Sitzung wurden dann nie geschrieben.

## Addon-Updates

Der Client fragt beim Start und alle 6 Stunden das neueste Release des Addon-Repositories (`alexbangert/forevercollect-addon`, überschreibbar per `FOREVERDB_ADDON_REPO`) ab und vergleicht es mit `## Version:` der installierten `Interface/AddOns/ForeverCollect/ForeverCollect.toc` je Client. Ist es neuer (oder das Addon fehlt), zeigt die Client-Karte „ForeverCollect vX.Y.Z verfügbar“ mit „Aktualisieren“/„Installieren“: Das Release-Zip wird geladen, geprüft (nur `ForeverCollect/`, TOC-Version muss zum Release passen) und der Addon-Ordner atomar ersetzt; bei laufendem Spiel gilt es ab dem nächsten Login.

Das Repository ist privat, daher braucht die Abfrage ein GitHub-Token (Fine-grained PAT, nur dieses Repository, Berechtigung „Contents: Read-only“). Reihenfolge: Umgebungsvariable `FOREVERDB_GITHUB_TOKEN` zur Laufzeit, sonst der beim Build einkompilierte Wert (`FOREVERDB_GITHUB_TOKEN=github_pat_… pnpm tauri build`), sonst `github_token` in `settings.json`. Das Token steht nie im Quellcode und wird dem Fenster nicht übergeben.
