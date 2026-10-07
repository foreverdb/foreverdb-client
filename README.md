# ForeverDB Uploader

Desktop uploader for the ForeverCollect SavedVariables of **World of Warcraft: Forever**. Only the Forever client (the `_classic_beta_` folder, product `wow_classic_beta`) is supported; Classic, Classic Era and Retail are not supported yet. Their folders are ignored during detection and rejected when chosen by hand.

Development: `pnpm tauri dev`. Build: `pnpm tauri build`.

## Using the uploader

The window follows the order in which things have to happen:

1. **Game client.** The uploader finds the WoW installation through `.build.info`:
   - It looks in Faugus, Lutris, Wine, Bottles, Windows and macOS locations.
   - On Windows it also reads the launcher's registry entries (`HKLM\SOFTWARE\WOW6432Node\Blizzard Entertainment\World of Warcraft`) and checks `%ProgramFiles(x86)%\World of Warcraft`, `<drive>:\World of Warcraft` and `<drive>:\Games\World of Warcraft` on every fixed drive.

   It then shows the Forever client the launcher lists as installed, with its version. A client is a flavour folder: `.flavor.info` names the product, and `Wow*.exe` is the process to watch. If nothing is found, "+ Add Forever client" lets you:
   - activate Forever folders of the installations that were found, or
   - pick a whole WoW installation (the "World of Warcraft" folder) or its `_classic_beta_` folder with the native folder dialog.

   Your choice is stored in `settings.json`.
2. **ForeverCollect addon.** Shows the installed addon version next to the latest release and installs or updates it with one click (see "Addon updates").
3. **Upload.** Shows the state of `ForeverCollect.lua` and warns about the situations below, then offers "Upload now" for a manual upload next to the automatic one:
   - WoW is running.
   - The game has not saved for a long time.
   - The game crashed.

   The result of the upload and the server-side import status appear in the same card.

The "Activity" list below the cards shows what the background watcher did during this session.

Environment overrides: `FOREVERDB_WOW_DIR`, `FOREVERDB_INGRESS_URL`, `FOREVERDB_ADDON_REPO`, `FOREVERDB_GITHUB_TOKEN`.

## Automatic upload

WoW writes the SavedVariables only on logout, on `/reload` and on exit. The app checks the `ForeverCollect.lua` of every active client every 3 s. It uploads a new write once the file has been unchanged for 3 s and its content differs from the last upload (the hash is kept in `settings.json`). This happens whether or not the game is still running.

**Backup file:** If the current file is an emptied database (its state after an upload) but the game's `ForeverCollect.lua.bak` still holds a session, the `.bak` is uploaded instead.

**Cleanup:** After a successful upload the file is removed once the game is closed; an uploaded `.bak` is removed right away. This way the next session starts empty.

**Archive:** Every upload keeps a copy (the last 30):
- Linux: `~/.local/share/gg.foreverdb.client/archive/`
- Windows: `%APPDATA%\gg.foreverdb.client\archive\`

**Settings:** The switches and the hashes are stored in `settings.json`:
- Linux: `~/.config/gg.foreverdb.client/settings.json`
- Windows: `%APPDATA%\gg.foreverdb.client\settings.json`

**From 0.1 (ForeverDB Client):** Up to 0.1 the app used the identifier `com.alex.foreverdb-client`. If the new directory has no `settings.json` yet, the first start copies it from the old directory (same user ID, hashes and added clients) and moves the old archive over. The old directories stay and can be deleted by hand.

**Warnings:** The app warns when the game has run for more than 2 h without writing the file. In the game, `/fc save` or `/fc autosave 30` fixes that. It also warns when the client's `Errors/` folder holds a crash report newer than the last save; the data of that session was then never written.

## Tray and autostart

**Tray:** The uploader shows a tray icon; a left click or "Show ForeverDB Uploader" in its menu opens the window, "Quit" ends the app. With "Closing the window keeps the uploader running in the tray" (on by default) closing the window only hides it, and the watcher keeps uploading. Switched off, closing the window quits. On GNOME the tray icon needs the AppIndicator extension; without it, starting the app again brings the window back.

**Autostart:** "Start with the system, minimized to the tray" (off by default) creates the system's autostart entry, which starts the app with `--minimized`: no window, only the tray icon and the watcher. The switch reads the entry itself, so removing it outside the app shows up as off.
- Linux: `~/.config/autostart/ForeverDB Uploader.desktop`
- Windows: `ForeverDB Uploader` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`

**One instance:** Only one uploader runs at a time; starting it again shows the window of the running one instead of watching the same files twice.

## Upload format

The uploader converts the SavedVariables to JSON itself and uploads only that (`Content-Type: application/json`, `Content-Encoding: gzip`); the server no longer parses Lua. The parser reads the dump as data and never executes it.

**Rejected before upload:** If a file cannot be read or carries an unsupported `schemaVersion` (typically an outdated addon), the app reports it before uploading, leaves the file in place and still archives it.

**Rejected by the server:** The same applies when the server rejects the snapshot permanently (4xx except 408/429). The content is marked as handled so the watcher does not resend it every 3 s.

**What the archive keeps:** The archive keeps the raw `.lua`, not the JSON. Since the server no longer stores the original file, it is the only remaining source for reproducing a parser bug. The file name carries the import ID, so it can still be matched to `import_jobs.id`.

## Addon updates

**When it checks:** On start and every 6 hours, the uploader fetches the latest release of the addon repository (`foreverdb/forevercollect-addon`, overridable with `FOREVERDB_ADDON_REPO`). It compares that release with the `## Version:` of the installed `Interface/AddOns/ForeverCollect/ForeverCollect.toc`.

**Installing or updating:** If the release is newer, or the addon is missing, the addon card offers "Update" or "Install":
1. The release zip is downloaded.
2. It is verified: it may contain only `ForeverCollect/`, and the TOC version must match the release.
3. The addon folder is replaced atomically.

While the game is running, the new version takes effect on the next login or `/reload`.

**GitHub token:** The repository is private, so the check needs a GitHub token (a fine-grained PAT for this repository only, with the "Contents: Read-only" permission). The uploader uses the first one it finds:
1. the `FOREVERDB_GITHUB_TOKEN` environment variable at runtime
2. the value compiled in at build time
3. `github_token` in `settings.json`

The compiled-in value comes from `FOREVERDB_GITHUB_TOKEN` in the build environment (`FOREVERDB_GITHUB_TOKEN=github_pat_… pnpm tauri build`). If that is not set, it comes from the contents of `src-tauri/github-token`: store the token there once and every build picks it up, including the Arch package. The file is excluded by `.gitignore`. Note that the token ends up in plain text inside the binary; it is never handed to the window.

## User ID

On its first start the uploader creates a random ID (UUID v4) and stores it as `user_id` in `settings.json`. Later starts keep it. Every upload carries it in the `X-ForeverDB-User` header, and the server records it as the uploader of the import and of the observations it contributed first.

**What this allows:** An admin can block an uploader, after which the ingress answers `403` and the uploader shows the rejection without retrying the same file. An admin can also delete everything that uploader contributed.

**Limits:** The ID is created by the uploader itself, so it identifies an installation, not a person. Deleting `settings.json` produces a new one. Uploads without the header are rejected with `400`, so uploaders older than this version can no longer upload.

## Linux

The uploader runs natively (GTK 3 + WebKitGTK). The tray icon needs `libayatana-appindicator` (Arch), `libayatana-appindicator3-1` (Debian/Ubuntu) or `libayatana-appindicator-gtk3` (Fedora); the packages depend on it. It finds WoW in the Wine prefixes of Faugus, Lutris, Bottles and `~/.wine`, and detects the running game through `/proc`. That is why there is no Flatpak: inside the sandbox neither the prefixes under `~/.var/app/…` nor the host's `Wow*.exe` processes would be visible.

**Building** needs Rust, Node + pnpm and the WebKitGTK development packages:

- Arch/CachyOS: `webkit2gtk-4.1 gtk3 libsoup3 libayatana-appindicator base-devel` (plus `cargo pnpm nodejs`)
- Debian/Ubuntu: `libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev librsvg2-dev libayatana-appindicator3-dev build-essential`
- Fedora: `webkit2gtk4.1-devel gtk3-devel libsoup3-devel librsvg2-devel libayatana-appindicator-gtk3-devel`

Then run `pnpm install` and `pnpm tauri build`. On Arch-based systems use `NO_STRIP=true pnpm tauri build`: the old `strip` bundled with `linuxdeploy` does not know the `.relr.dyn` sections of current libraries and otherwise breaks the AppImage step.

`src-tauri/tauri.linux.conf.json` selects the targets. The packages land in `src-tauri/target/release/bundle/`, named after the `productName`:

- `deb/ForeverDB Uploader_<version>_amd64.deb`: install with `sudo apt install "./ForeverDB Uploader_<version>_amd64.deb"`. The package is named `forever-db-uploader` and depends on `libwebkit2gtk-4.1-0`, `libgtk-3-0` and `libayatana-appindicator3-1`.
- `rpm/ForeverDB Uploader-<version>-1.x86_64.rpm`: install with `sudo dnf install "./ForeverDB Uploader-<version>-1.x86_64.rpm"`.
- `appimage/ForeverDB Uploader_<version>_amd64.AppImage`: make it executable and start it.
  - It runs without installation but needs a glibc at least as new as the build system's.
  - Tauri downloads `linuxdeploy` while building, so network access is required.
  - To build only this target: `pnpm tauri build --bundles appimage`.

**Arch/CachyOS package:** `cd packaging/arch && makepkg -sfic` builds from the checkout without downloading anything. It installs the binary, the `.desktop` file and the icons; `pacman -Rns foreverdb-uploader` removes them again. The package replaces the old `foreverdb-client` package. `FOREVERDB_GITHUB_TOKEN` or `src-tauri/github-token` is compiled in just like with `pnpm tauri build` (see "Addon updates").

For data paths see "Automatic upload". If the window stays white or empty (typically Wayland with NVIDIA), start it with `WEBKIT_DISABLE_DMABUF_RENDERER=1 foreverdb-uploader`.

## Windows

The uploader runs natively on Windows 10/11 and needs the WebView2 runtime, which is preinstalled there; the installer downloads it otherwise. The build produces an NSIS installer that installs for the current user without administrator rights (`src-tauri/tauri.windows.conf.json`).

**Building** needs Rust (MSVC toolchain, Visual Studio Build Tools with C++) and Node + pnpm. Then run `pnpm install` and `pnpm tauri build`; the installer lands in `src-tauri/target/release/bundle/nsis/ForeverDB Uploader_<version>_x64-setup.exe`. The old "ForeverDB Client" is a separate app for Windows; uninstall it after the first start of the uploader. On Windows, `cargo test` in `src-tauri` also covers the process and path detection (ToolHelp32, canonical paths).

**Detection:** The running game is detected through the process list. Paths from the folder dialog are normalized to their on-disk spelling, so the same client never shows up twice.

## Recommended IDE setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
