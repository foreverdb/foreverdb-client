use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

mod addon;
mod lua;
mod update;
use addon::AddonRelease;
use update::UploaderRelease;

// The user only picks a WoW client; installation directory, account, file and
// upload target are derived. A client is a flavour folder such as
// "_classic_beta_" or "_classic_era_": its ".flavor.info" names the product, the "Wow*.exe" in it
// the process to watch, and the launcher's ".build.info" the installed version.

const DEFAULT_TARGET_URL: &str = "https://upload.foreverdb.app/imports/forevercollect";
/// The website's API; the uploader only calls it to link itself to an account.
const DEFAULT_API_URL: &str = "https://api.foreverdb.app";

/// The products the client works with: Forever runs in the "_classic_beta_" folder,
/// Classic Era and Hardcore share the "_classic_era_" folder (the season tells them
/// apart on the server). Retail and the other Classic versions are not supported yet.
const SUPPORTED_PRODUCTS: &[&str] = &["wow_classic_beta", "wow_classic_era"];

fn is_supported(def: &ClientDef) -> bool {
    SUPPORTED_PRODUCTS.contains(&def.product.as_str())
}

fn unsupported_message(def: &ClientDef) -> String {
    format!(
        "'{}' is a {} client. Only Forever and Classic Era / Hardcore are supported; Retail and the other Classic versions are not supported yet.",
        def.id, def.label
    )
}

/// Human labels for Blizzard's product codes; unknown products show the code. The
/// other flavours stay listed so a rejection can name what the user picked.
fn product_label(product: &str) -> String {
    match product {
        "wow_classic_beta" => "Forever",
        "wow_classic_era" => "Classic Era & Hardcore",
        "wow_classic_era_ptr" => "Classic Era PTR",
        "wow_classic" => "Classic",
        "wow_classic_ptr" => "Classic PTR",
        "wow" => "Retail",
        "wowt" => "Retail PTR",
        "wowxptr" => "Retail PTR 2",
        "wow_beta" => "Retail Beta",
        other => other,
    }
    .to_string()
}

/// A flavour folder that can act as a client.
#[derive(Serialize, Clone, Debug, PartialEq)]
struct ClientDef {
    /// The flavour directory; also the client's identity.
    id: String,
    label: String,
    product: String,
    process: String,
}

/// A WoW account folder (`WTF/Account/<name>`) of a client; each one has its own
/// SavedVariables and is uploaded on its own.
#[derive(Serialize, Clone, Debug)]
struct AccountStatus {
    /// The account folder; identifies the account in the per-upload state.
    key: String,
    name: String,
    /// The user the account belongs to, see `AccountDir::bnet_id`.
    bnet_id: String,
    /// "Character-Realm" of the first character folders, so people sharing an
    /// installation recognise their account.
    characters: Vec<String>,
    /// The file that would be uploaded: the SavedVariables file, or the game's .bak
    /// of it when the current file is an emptied database.
    file_path: String,
    file_exists: bool,
    /// The file holds catalogs; an emptied database is not worth uploading.
    file_has_data: bool,
    from_backup: bool,
    /// Unix time the SavedVariables file was last written (WoW writes it only on logout,
    /// exit or /reload).
    saved_at: Option<u64>,
    /// The ForeverDB account this user's uploader ID was linked to, for display.
    linked_account: Option<String>,
}

#[derive(Serialize)]
struct ClientStatus {
    id: String,
    label: String,
    product: String,
    version: Option<String>,
    accounts: Vec<AccountStatus>,
    running: bool,
    /// Added by the user (as opposed to detected as installed); can be removed again.
    custom: bool,
    /// Version of the installed ForeverCollect addon, if any.
    addon_version: Option<String>,
    /// Newest release version when it is newer than the installed addon (or none is installed).
    addon_update: Option<String>,
    /// Unix time the running game process was started, when it is running.
    running_since: Option<u64>,
    /// Unix time of the newest crash dump in the client's Errors folder that is younger than
    /// the last save: the session that crashed never wrote its data.
    crash_at: Option<u64>,
}

#[derive(Serialize)]
struct Installation {
    /// First installation found; None when there is none at all.
    wow_dir: Option<String>,
    installations: Vec<String>,
    searched: Vec<String>,
    clients: Vec<ClientStatus>,
}

/// A flavour folder offered in the "add client" dialog.
#[derive(Serialize)]
struct ClientCandidate {
    id: String,
    label: String,
    product: String,
    version: Option<String>,
    has_data: bool,
}

#[derive(Serialize, Clone)]
struct UploadResult {
    import_id: Option<String>,
    file_path: String,
    running: bool,
    deleted: bool,
}

/// Persisted in the app's config directory.
#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    #[serde(default = "default_true")]
    auto_upload: bool,
    /// Closing the window hides it; the watcher keeps running behind the tray icon.
    #[serde(default = "default_true")]
    close_to_tray: bool,
    /// Flavour directories the user added on top of the installed clients.
    #[serde(default)]
    extra_clients: Vec<String>,
    /// WoW installation directories the user added; their installed clients are active.
    #[serde(default)]
    extra_installations: Vec<String>,
    /// Per account folder, the SHA-256 of the file uploaded last, so a file that could not
    /// be removed (game still running) is not sent twice.
    #[serde(default)]
    last_upload_hash: HashMap<String, String>,
    /// Per account folder, the Unix time of the last successful upload. The upload removes
    /// the SavedVariables file, so this stands in for its timestamp when judging crash dumps.
    #[serde(default)]
    last_upload_at: HashMap<String, u64>,
    /// Per user (Battle.net account, see `AccountDir::bnet_id`) the uploader identity, so
    /// people sharing one installation upload and link as different uploaders.
    #[serde(default)]
    identities: HashMap<String, Identity>,
    /// The single user ID of versions that knew only one account. The first user found
    /// takes it over (with its link), so that account keeps its uploader.
    #[serde(default, rename = "user_id", skip_serializing_if = "Option::is_none")]
    legacy_user_id: Option<String>,
    #[serde(
        default,
        rename = "linked_account",
        skip_serializing_if = "Option::is_none"
    )]
    legacy_linked_account: Option<String>,
    /// The newest uploader and addon versions a desktop notification was shown for, so
    /// each release is notified once and not on every start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notified_uploader_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notified_addon_version: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Identity {
    /// Random ID sent with every upload of this user (`X-ForeverDB-User`) so the server
    /// can block a user and delete their data.
    user_id: String,
    /// The ForeverDB account the user ID was last linked to with a pairing code, for display
    /// only: the link itself lives on the server and can be removed there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    linked_account: Option<String>,
}

/// A fresh install gets the same values as a settings.json that lacks the fields.
impl Default for Settings {
    fn default() -> Self {
        serde_json::from_str("{}").expect("every field has a default")
    }
}

/// Settings from the content of settings.json, and whether it was unusable. An unusable file
/// falls back to the defaults but keeps the user IDs it can still read, so a damaged file
/// never gives an uploader a new identity (the server blocks and deletes by that ID).
fn parse_settings(content: &str) -> (Settings, bool) {
    if let Ok(settings) = serde_json::from_str(content) {
        return (settings, false);
    }
    let value = serde_json::from_str::<serde_json::Value>(content).ok();
    let legacy_user_id = value
        .as_ref()
        .and_then(|value| value.get("user_id")?.as_str().map(str::to_string))
        .filter(|id| !id.trim().is_empty());
    let identities = value
        .as_ref()
        .and_then(|value| value.get("identities")?.as_object())
        .into_iter()
        .flatten()
        .filter_map(|(user, identity)| {
            serde_json::from_value::<Identity>(identity.clone())
                .ok()
                .filter(|identity| !identity.user_id.trim().is_empty())
                .map(|identity| (user.clone(), identity))
        })
        .collect();
    (
        Settings {
            legacy_user_id,
            identities,
            ..Settings::default()
        },
        true,
    )
}

/// SHA-256 of a file's content as hex: stable across builds, unlike `DefaultHasher`.
fn content_hash(content: &[u8]) -> String {
    Sha256::digest(content)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Creates the identities of the given users that the settings lack yet, in the given
/// order; returns whether it created one. The first new one takes over the legacy user ID.
fn ensure_identities<'a>(
    settings: &mut Settings,
    users: impl IntoIterator<Item = &'a str>,
) -> bool {
    let mut created = false;
    for user in users {
        if settings.identities.contains_key(user) {
            continue;
        }
        let identity = match settings.legacy_user_id.take() {
            Some(user_id) => Identity {
                user_id,
                linked_account: settings.legacy_linked_account.take(),
            },
            None => Identity {
                user_id: uuid::Uuid::new_v4().to_string(),
                linked_account: None,
            },
        };
        settings.identities.insert(user.to_string(), identity);
        created = true;
    }
    created
}

/// The users of the given clients in the order they get their identities: Battle.net
/// account folders first, as only those were uploaded before there was more than one
/// identity, and the legacy user ID belongs to the first of them.
fn users_of(clients: &[ClientStatus]) -> Vec<String> {
    let accounts = || clients.iter().flat_map(|client| &client.accounts);
    let mut users: Vec<String> = Vec::new();
    for account in accounts()
        .filter(|account| account.name.contains('#'))
        .chain(accounts().filter(|account| !account.name.contains('#')))
    {
        if !users.contains(&account.bnet_id) {
            users.push(account.bnet_id.clone());
        }
    }
    users
}

/// What the window may see of the settings.
#[derive(Serialize)]
struct SettingsView {
    auto_upload: bool,
    close_to_tray: bool,
    /// Read from the system's autostart entry, not stored in settings.json.
    autostart: bool,
    extra_clients: Vec<String>,
    extra_installations: Vec<String>,
    repository: String,
}

impl Settings {
    fn view(&self, app: &AppHandle) -> SettingsView {
        SettingsView {
            auto_upload: self.auto_upload,
            close_to_tray: self.close_to_tray,
            autostart: app.autolaunch().is_enabled().unwrap_or(false),
            extra_clients: self.extra_clients.clone(),
            extra_installations: self.extra_installations.clone(),
            repository: addon::repository(),
        }
    }
}

/// Cached answer of the last release check.
#[derive(Clone)]
struct ReleaseCache<T> {
    checked_at: std::time::Instant,
    result: Result<T, String>,
}

fn default_true() -> bool {
    true
}

/// Server-side state of an import, read from the ingress while it is polled.
#[derive(Serialize, Clone)]
struct ImportStatus {
    import_id: String,
    status: String,
    error: Option<String>,
}

/// Progress of the background watcher, sent to the window as "activity" events.
#[derive(Serialize, Clone)]
struct Activity {
    client: String,
    /// Key of the account folder the event is about, if it is about one.
    account: Option<String>,
    kind: String,
    message: String,
    result: Option<UploadResult>,
    import: Option<ImportStatus>,
}

struct AppState {
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    archive_dir: PathBuf,
    release: Mutex<Option<ReleaseCache<AddonRelease>>>,
    uploader_release: Mutex<Option<ReleaseCache<UploaderRelease>>>,
    /// Accounts whose last upload could not be archived; their files must not be deleted.
    unarchived: Mutex<HashSet<String>>,
    /// Accounts with an upload in flight.
    uploading: Mutex<HashSet<String>>,
    /// Accounts whose last upload failed transiently: when the watcher may try again and
    /// how many attempts failed in a row.
    retry: Mutex<HashMap<String, RetryState>>,
}

#[derive(Clone, Copy)]
struct RetryState {
    attempts: u32,
    not_before: std::time::Instant,
}

/// Held for the length of one upload so the button and the watcher never send the same
/// account's file twice.
struct UploadSlot<'a> {
    state: &'a AppState,
    account: String,
}

impl Drop for UploadSlot<'_> {
    fn drop(&mut self) {
        self.state.uploading.lock().unwrap().remove(&self.account);
    }
}

impl AppState {
    fn try_start_upload(&self, account: &str) -> Option<UploadSlot<'_>> {
        let mut uploading = self.uploading.lock().unwrap();
        uploading.insert(account.to_string()).then(|| UploadSlot {
            state: self,
            account: account.to_string(),
        })
    }

    fn load(path: PathBuf, archive_dir: PathBuf) -> Self {
        let settings = match fs::read(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            read => {
                let content = read
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .unwrap_or_default();
                let (settings, corrupt) = parse_settings(&content);
                if corrupt {
                    // Kept so nothing is lost when the next change rewrites settings.json.
                    let backup = path.with_extension("json.corrupt");
                    let _ = fs::copy(&path, &backup);
                    let kept =
                        if settings.legacy_user_id.is_some() || !settings.identities.is_empty() {
                            " (user IDs kept)"
                        } else {
                            ""
                        };
                    eprintln!(
                        "settings.json could not be read; kept as {} and started with defaults{kept}",
                        backup.display()
                    );
                }
                settings
            }
        };
        Self {
            settings: Mutex::new(settings),
            settings_path: path,
            archive_dir,
            release: Mutex::new(None),
            uploader_release: Mutex::new(None),
            unarchived: Mutex::new(HashSet::new()),
            uploading: Mutex::new(HashSet::new()),
            retry: Mutex::new(HashMap::new()),
        }
    }

    /// Holds the watcher back after a transient failure, honouring the server's
    /// Retry-After when it sent one.
    fn defer_retry(&self, account: &str, retry_after: Option<Duration>) {
        let mut retry = self.retry.lock().unwrap();
        let attempts = retry.get(account).map_or(0, |state| state.attempts) + 1;
        let delay = retry_after.unwrap_or_else(|| retry_delay(attempts));
        retry.insert(
            account.to_string(),
            RetryState {
                attempts,
                not_before: std::time::Instant::now() + delay,
            },
        );
    }

    fn clear_retry(&self, account: &str) {
        self.retry.lock().unwrap().remove(account);
    }

    fn retry_due(&self, account: &str) -> bool {
        self.retry
            .lock()
            .unwrap()
            .get(account)
            .is_none_or(|state| std::time::Instant::now() >= state.not_before)
    }

    fn cached_release(&self) -> Option<AddonRelease> {
        self.release
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|cache| cache.result.clone().ok())
    }

    fn update(&self, change: impl FnOnce(&mut Settings)) -> Settings {
        let mut settings = self.settings.lock().unwrap();
        change(&mut settings);
        if let Some(parent) = self.settings_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        // Written to a temporary file first so a crash mid-write never leaves an
        // empty settings.json behind.
        if let Ok(content) = serde_json::to_string_pretty(&*settings) {
            let temp = self.settings_path.with_extension("json.tmp");
            if fs::write(&temp, content).is_ok() {
                let _ = fs::rename(&temp, &self.settings_path);
            }
        }
        settings.clone()
    }

    /// The user ID a user's uploads are sent with; created here if the settings still lack it.
    fn user_id(&self, user: &str) -> String {
        if let Some(identity) = self.snapshot().identities.get(user) {
            return identity.user_id.clone();
        }
        self.update(|settings| {
            ensure_identities(settings, [user]);
        })
        .identities[user]
            .user_id
            .clone()
    }

    /// Gives every user of the clients an identity; returns whether one was created.
    fn ensure_identities(&self, clients: &[ClientStatus]) -> bool {
        let users = users_of(clients);
        let snapshot = self.snapshot();
        if users
            .iter()
            .all(|user| snapshot.identities.contains_key(user))
        {
            return false;
        }
        let mut created = false;
        self.update(|settings| {
            created = ensure_identities(settings, users.iter().map(String::as_str))
        });
        created
    }

    fn snapshot(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }
}

fn target_url() -> String {
    std::env::var("FOREVERDB_INGRESS_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TARGET_URL.to_string())
}

fn api_url() -> String {
    std::env::var("FOREVERDB_API_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API_URL.to_string())
}

/// The ingress exposes every import next to the upload endpoint: the upload goes to
/// `.../imports/forevercollect`, its status is read from `.../imports/{id}`.
fn status_url(upload_url: &str, import_id: &str) -> String {
    let base = upload_url.trim_end_matches('/');
    let base = base.strip_suffix("/forevercollect").unwrap_or(base);
    format!("{base}/{import_id}")
}

/// Directories a WoW installation is looked for in, most likely first. An
/// explicit FOREVERDB_WOW_DIR always wins, then the installations the user added.
fn candidate_wow_dirs(settings: &Settings) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(explicit) = std::env::var_os("FOREVERDB_WOW_DIR") {
        candidates.push(PathBuf::from(explicit));
    }
    candidates.extend(settings.extra_installations.iter().map(PathBuf::from));
    candidates.extend(system_wow_dirs());
    candidates
}

/// Where the launcher installs on this platform: Wine prefixes (Faugus, Lutris,
/// plain Wine, Bottles) and the macOS default.
#[cfg(not(target_os = "windows"))]
fn system_wow_dirs() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let program_files = "drive_c/Program Files (x86)/World of Warcraft";
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for prefix in [
            "Faugus/battlenet",
            "Games/battlenet",
            "Games/battle-net",
            ".wine",
            "Games/world-of-warcraft",
        ] {
            candidates.push(home.join(prefix).join(program_files));
        }
        // Bottles (Flatpak and native) keep one prefix per bottle.
        for bottles in [
            home.join(".var/app/com.usebottles.bottles/data/bottles/bottles"),
            home.join(".local/share/bottles/bottles"),
        ] {
            if let Ok(entries) = fs::read_dir(&bottles) {
                for entry in entries.flatten() {
                    candidates.push(entry.path().join(program_files));
                }
            }
        }
    }
    candidates.push(PathBuf::from("C:/Program Files (x86)/World of Warcraft"));
    candidates.push(PathBuf::from("/Applications/World of Warcraft"));
    candidates
}

/// Where the launcher installs on Windows: the registry entries Battle.net writes,
/// the Program Files folders, and the usual spots on every fixed drive.
#[cfg(target_os = "windows")]
fn system_wow_dirs() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for value in registry_install_paths() {
        candidates.extend(install_path_candidates(&value));
    }
    for var in ["ProgramFiles(x86)", "ProgramW6432", "ProgramFiles"] {
        if let Some(root) = std::env::var_os(var) {
            candidates.push(PathBuf::from(root).join("World of Warcraft"));
        }
    }
    for drive in fixed_drives() {
        for sub in [
            "World of Warcraft",
            "Games\\World of Warcraft",
            "Program Files (x86)\\World of Warcraft",
            "Blizzard\\World of Warcraft",
        ] {
            candidates.push(drive.join(sub));
        }
    }
    candidates
}

/// InstallPath values Battle.net writes to HKLM (32-bit view). The value may name the
/// installation or a flavour folder inside it, and the subkeys vary between launcher
/// versions, so every value found is offered and `is_wow_dir` decides.
#[cfg(target_os = "windows")]
fn registry_install_paths() -> Vec<String> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY};
    use winreg::RegKey;
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let flags = KEY_READ | KEY_WOW64_32KEY;
    let mut paths = Vec::new();
    if let Ok(key) =
        hklm.open_subkey_with_flags(r"SOFTWARE\Blizzard Entertainment\World of Warcraft", flags)
    {
        paths.extend(key.get_value::<String, _>("InstallPath"));
        for name in key.enum_keys().flatten() {
            if let Ok(sub) = key.open_subkey_with_flags(&name, flags) {
                paths.extend(sub.get_value::<String, _>("InstallPath"));
            }
        }
    }
    if let Ok(key) = hklm.open_subkey_with_flags(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\World of Warcraft",
        flags,
    ) {
        paths.extend(key.get_value::<String, _>("InstallLocation"));
    }
    paths
}

/// A registry value and its parent, so both "…\World of Warcraft" and
/// "…\World of Warcraft\_retail_\" lead to the installation.
#[cfg(target_os = "windows")]
fn install_path_candidates(value: &str) -> Vec<PathBuf> {
    let path = PathBuf::from(value.trim().trim_end_matches(['\\', '/']));
    let mut candidates = vec![path.clone()];
    candidates.extend(path.parent().map(Path::to_path_buf));
    candidates
}

/// Roots of the local fixed drives ("C:\", "D:\"); removable, optical and network
/// drives are skipped because probing them can block.
#[cfg(target_os = "windows")]
fn fixed_drives() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;
    // SAFETY: plain Win32 queries; the root path is NUL-terminated.
    let mask = unsafe { GetLogicalDrives() };
    (b'C'..=b'Z')
        .filter(|letter| mask & (1u32 << (letter - b'A')) != 0)
        .map(|letter| format!("{}:\\", letter as char))
        .filter(|root| {
            let wide: Vec<u16> = root.encode_utf16().chain([0]).collect();
            let kind = unsafe { GetDriveTypeW(wide.as_ptr()) };
            kind == DRIVE_FIXED
        })
        .map(PathBuf::from)
        .collect()
}

/// The form a directory is identified by. On Windows the on-disk spelling with
/// backslashes and no "\\?\" prefix, so a probed "C:/…" and a picked "C:\…" (or an
/// 8.3 short name) are the same client; elsewhere the path as given.
pub(crate) fn normalize_dir(dir: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
    }
    #[cfg(not(target_os = "windows"))]
    {
        dir.to_path_buf()
    }
}

fn is_wow_dir(dir: &Path) -> bool {
    dir.join(".build.info").is_file()
}

/// All installations among the candidates, in candidate order.
fn find_wow_dirs(candidates: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in candidates {
        if !is_wow_dir(dir) {
            continue;
        }
        let dir = normalize_dir(dir);
        if !found.contains(&dir) {
            found.push(dir);
        }
    }
    found
}

/// Product -> version from the launcher's .build.info (pipe-separated, header
/// columns named like "Version!STRING:0").
fn read_build_info(wow_dir: &Path) -> HashMap<String, String> {
    let mut versions = HashMap::new();
    let Ok(content) = fs::read_to_string(wow_dir.join(".build.info")) else {
        return versions;
    };
    let mut lines = content.lines();
    let Some(header) = lines.next() else {
        return versions;
    };
    let columns: Vec<&str> = header
        .split('|')
        .map(|column| column.split('!').next().unwrap_or(""))
        .collect();
    let product_index = columns.iter().position(|name| *name == "Product");
    let version_index = columns.iter().position(|name| *name == "Version");
    let (Some(product_index), Some(version_index)) = (product_index, version_index) else {
        return versions;
    };
    for line in lines {
        let fields: Vec<&str> = line.split('|').collect();
        if let (Some(product), Some(version)) =
            (fields.get(product_index), fields.get(version_index))
        {
            versions.insert((*product).to_string(), (*version).to_string());
        }
    }
    versions
}

/// Product code from a flavour folder's .flavor.info ("Product Flavor!STRING:0\nwow_classic_beta").
fn read_flavor(dir: &Path) -> Option<String> {
    let content = fs::read_to_string(dir.join(".flavor.info")).ok()?;
    content
        .lines()
        .skip(1)
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The game executable inside a flavour folder ("WowB.exe", "WowClassic.exe", "Wow.exe").
fn find_game_executable(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            lower.starts_with("wow") && lower.ends_with(".exe") && !lower.contains("launcher")
        })
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Reads a flavour folder as a client, or None when it is not one.
fn client_from_dir(dir: &Path) -> Option<ClientDef> {
    if !dir.is_dir() {
        return None;
    }
    let product = read_flavor(dir)?;
    let process = find_game_executable(dir)?;
    Some(ClientDef {
        id: normalize_dir(dir).to_string_lossy().into_owned(),
        label: product_label(&product),
        product,
        process,
    })
}

/// Every flavour folder ("_classic_beta_", "_retail_", ...) inside a WoW installation.
fn flavour_dirs(wow_dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(wow_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with('_') && name.ends_with('_')
                })
        })
        .collect();
    dirs.sort();
    dirs
}

/// A WoW account folder of a client.
#[derive(Debug, Clone, PartialEq)]
struct AccountDir {
    name: String,
    /// The person behind the account: Battle.net names the folders "<account id>#<n>",
    /// one per WoW licence of the same Battle.net account. Folders without "#" (old
    /// account names) stand for themselves.
    bnet_id: String,
    dir: PathBuf,
}

impl AccountDir {
    fn key(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }

    fn file(&self) -> PathBuf {
        self.dir.join("SavedVariables").join("ForeverCollect.lua")
    }
}

fn bnet_id(account_name: &str) -> &str {
    account_name
        .split_once('#')
        .map_or(account_name, |(id, _)| id)
}

/// Every account folder of a client (`WTF/Account/<name>` with its own SavedVariables),
/// sorted. `WTF/Account/SavedVariables` holds the account-wide variables of all accounts
/// and is none.
fn accounts(client_dir: &Path) -> Vec<AccountDir> {
    let mut accounts: Vec<AccountDir> = fs::read_dir(client_dir.join("WTF").join("Account"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("SavedVariables").is_dir())
        .filter_map(|dir| {
            let name = dir.file_name()?.to_string_lossy().into_owned();
            (name != "SavedVariables").then(|| AccountDir {
                bnet_id: bnet_id(&name).to_string(),
                name,
                dir,
            })
        })
        .collect();
    accounts.sort_by(|a, b| a.name.cmp(&b.name));
    accounts
}

const CHARACTER_HINTS: usize = 6;

/// "Character-Realm" of the character folders (`<account>/<realm>/<character>`) the game
/// keeps per account, at most CHARACTER_HINTS of them.
fn account_characters(account_dir: &Path) -> Vec<String> {
    let subdirs = |dir: &Path| -> Vec<(String, PathBuf)> {
        let mut dirs: Vec<(String, PathBuf)> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .filter_map(|path| Some((path.file_name()?.to_string_lossy().into_owned(), path)))
            .collect();
        dirs.sort();
        dirs
    };
    subdirs(account_dir)
        .into_iter()
        .filter(|(realm, _)| realm != "SavedVariables")
        .flat_map(|(realm, path)| {
            // Newer clients file characters as "Character-Realm" under a numeric folder.
            let numbered = realm.chars().all(|c| c.is_ascii_digit());
            subdirs(&path).into_iter().map(move |(character, _)| {
                if numbered {
                    character
                } else {
                    format!("{character}-{realm}")
                }
            })
        })
        .take(CHARACTER_HINTS)
        .collect()
}

/// True when a process whose executable name is `process_name` runs. Only the
/// executable's file name is compared ("WowB.exe" must not match "Wow.exe").
fn wow_is_running(process_name: &str) -> bool {
    wow_process_started(process_name).is_some()
}

/// Start time of the running game process (the /proc entry's creation), if any. The
/// name is taken from the process name and from the first command-line argument.
#[cfg(target_os = "linux")]
fn wow_process_started(process_name: &str) -> Option<SystemTime> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name.chars().all(|character| character.is_ascii_digit()))
        .find(|pid| {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if comm.trim().eq_ignore_ascii_case(process_name) {
                return true;
            }
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
            executable_name(&String::from_utf8_lossy(argv0)).eq_ignore_ascii_case(process_name)
        })
        .map(|pid| {
            fs::metadata(format!("/proc/{pid}"))
                .and_then(|meta| meta.modified())
                .unwrap_or_else(|_| SystemTime::now())
        })
}

/// Start time of the running game process from a ToolHelp32 snapshot, if any. The
/// creation time needs a query handle, which any process of the same user grants.
#[cfg(target_os = "windows")]
fn wow_process_started(process_name: &str) -> Option<SystemTime> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: plain Win32 calls; the entry is zeroed with dwSize set, every handle is closed.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut pid = None;
        let mut more = Process32FirstW(snapshot, &mut entry);
        while more != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if executable_name(&name).eq_ignore_ascii_case(process_name) {
                pid = Some(entry.th32ProcessID);
                break;
            }
            more = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        let pid = pid?;
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Some(SystemTime::now());
        }
        let mut created: FILETIME = std::mem::zeroed();
        let mut exited: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        CloseHandle(process);
        Some(if ok != 0 {
            filetime_to_system_time(created.dwLowDateTime, created.dwHighDateTime)
        } else {
            SystemTime::now()
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn wow_process_started(_process_name: &str) -> Option<SystemTime> {
    None
}

/// Windows FILETIME (100 ns ticks since 1601-01-01) as a SystemTime.
#[cfg(any(target_os = "windows", test))]
fn filetime_to_system_time(low: u32, high: u32) -> SystemTime {
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let ticks = ((high as u64) << 32) | low as u64;
    UNIX_EPOCH + Duration::from_nanos(ticks.saturating_sub(UNIX_EPOCH_TICKS) * 100)
}

/// File name of a Windows or Unix path ("C:\\Games\\WowB.exe" -> "WowB.exe").
fn executable_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path).trim()
}

fn unix_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn modified_at(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(unix_seconds)
}

/// Newest crash dump (`Errors/*.txt`, written by the game's error handler) younger than
/// `since`. A crash ends the session without writing SavedVariables.
fn crash_after(client_dir: &Path, since: Option<u64>) -> Option<u64> {
    fs::read_dir(client_dir.join("Errors"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .filter_map(|path| modified_at(&path))
        .filter(|at| since.is_none_or(|saved| *at > saved))
        .max()
}

/// Whether a SavedVariables file holds at least one catalog. Right after an upload the
/// game writes an empty database (`["catalogs"] = {\n},`), which is nothing to send.
fn snapshot_has_catalogs(source: &[u8]) -> bool {
    let text = String::from_utf8_lossy(source);
    let Some(index) = text.find("[\"catalogs\"] = {") else {
        return false;
    };
    let rest = &text[index + "[\"catalogs\"] = {".len()..];
    !rest.trim_start().starts_with('}')
}

fn file_has_catalogs(path: &Path) -> bool {
    fs::read(path).is_ok_and(|source| snapshot_has_catalogs(&source))
}

fn file_hash(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|content| content_hash(&content))
}

/// Version of a flavour folder: the launcher entry for its product in the
/// installation the folder belongs to.
fn client_version(def: &ClientDef) -> Option<String> {
    Path::new(&def.id)
        .parent()
        .map(read_build_info)
        .and_then(|versions| versions.get(&def.product).cloned())
}

/// The file worth uploading for a client. WoW rotates the previous SavedVariables into
/// ".bak" on every write, so when the current file is an emptied database (the state
/// right after an upload) the backup still holds the last session.
struct SnapshotSource {
    path: PathBuf,
    from_backup: bool,
}

fn backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bak", path.display()))
}

fn snapshot_source(path: &Path) -> Option<SnapshotSource> {
    if file_has_catalogs(path) {
        return Some(SnapshotSource {
            path: path.to_path_buf(),
            from_backup: false,
        });
    }
    let backup = backup_path(path);
    if file_has_catalogs(&backup) {
        return Some(SnapshotSource {
            path: backup,
            from_backup: true,
        });
    }
    None
}

fn client_status(
    settings: &Settings,
    def: &ClientDef,
    version: Option<String>,
    custom: bool,
    release: Option<&AddonRelease>,
) -> ClientStatus {
    let client_dir = Path::new(&def.id);
    let addon_version = addon::installed_version(client_dir);
    let addon_update = release
        .filter(|release| addon::is_newer(&release.version, addon_version.as_deref()))
        .map(|release| release.version.clone());
    let accounts: Vec<AccountStatus> = accounts(client_dir)
        .iter()
        .map(|account| account_status(settings, account))
        .collect();
    let running_since = wow_process_started(&def.process).and_then(unix_seconds);
    // A crash only matters if it is younger than the last time the data was secured:
    // written by the game, or uploaded (which removes the file and its timestamp). The
    // crash dump does not tell which account played, so the latest one counts.
    let secured_at = accounts
        .iter()
        .filter_map(|account| {
            account
                .saved_at
                .max(settings.last_upload_at.get(&account.key).copied())
        })
        .max();
    ClientStatus {
        id: def.id.clone(),
        label: def.label.clone(),
        product: def.product.clone(),
        version,
        accounts,
        running: running_since.is_some(),
        custom,
        addon_version,
        addon_update,
        running_since,
        crash_at: crash_after(client_dir, secured_at),
    }
}

fn account_status(settings: &Settings, account: &AccountDir) -> AccountStatus {
    let main_path = account.file();
    let source = snapshot_source(&main_path);
    AccountStatus {
        key: account.key(),
        name: account.name.clone(),
        bnet_id: account.bnet_id.clone(),
        characters: account_characters(&account.dir),
        file_exists: main_path.is_file(),
        file_has_data: source.is_some(),
        from_backup: source.as_ref().is_some_and(|source| source.from_backup),
        saved_at: modified_at(&main_path).or_else(|| modified_at(&backup_path(&main_path))),
        file_path: source
            .map(|source| source.path)
            .unwrap_or(main_path)
            .to_string_lossy()
            .into_owned(),
        linked_account: settings
            .identities
            .get(&account.bnet_id)
            .and_then(|identity| identity.linked_account.clone()),
    }
}

/// Active clients: flavour folders the launchers of all known installations list as
/// installed, plus the ones the user added.
fn active_clients(
    settings: &Settings,
    release: Option<&AddonRelease>,
) -> (Vec<PathBuf>, Vec<String>, Vec<ClientStatus>) {
    clients_among(settings, candidate_wow_dirs(settings), release)
}

/// `active_clients` restricted to the given candidate directories.
fn clients_among(
    settings: &Settings,
    candidates: Vec<PathBuf>,
    release: Option<&AddonRelease>,
) -> (Vec<PathBuf>, Vec<String>, Vec<ClientStatus>) {
    let searched = candidates
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let wow_dirs = find_wow_dirs(&candidates);

    let mut seen = HashSet::new();
    let mut clients = Vec::new();
    for root in &wow_dirs {
        let versions = read_build_info(root);
        let added_installation = settings
            .extra_installations
            .iter()
            .any(|entry| Path::new(entry) == root.as_path());
        for dir in flavour_dirs(root) {
            let Some(def) = client_from_dir(&dir).filter(is_supported) else {
                continue;
            };
            let Some(version) = versions.get(&def.product).cloned() else {
                continue;
            };
            if seen.insert(def.id.clone()) {
                clients.push(client_status(
                    settings,
                    &def,
                    Some(version),
                    added_installation,
                    release,
                ));
            }
        }
    }
    for extra in &settings.extra_clients {
        if seen.contains(extra) {
            continue;
        }
        // Entries of unsupported flavours stay in the settings but are ignored.
        let Some(def) = client_from_dir(Path::new(extra)).filter(is_supported) else {
            continue;
        };
        let version = client_version(&def);
        seen.insert(def.id.clone());
        clients.push(client_status(settings, &def, version, true, release));
    }
    (wow_dirs, searched, clients)
}

#[tauri::command]
fn detect_installation(state: tauri::State<'_, AppState>) -> Installation {
    let release = state.cached_release();
    let (mut wow_dirs, mut searched, mut clients) =
        active_clients(&state.snapshot(), release.as_ref());
    // New users show up with their (inherited) link right away.
    if state.ensure_identities(&clients) {
        (wow_dirs, searched, clients) = active_clients(&state.snapshot(), release.as_ref());
    }
    Installation {
        wow_dir: wow_dirs
            .first()
            .map(|dir| dir.to_string_lossy().into_owned()),
        installations: wow_dirs
            .iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect(),
        searched,
        clients,
    }
}

/// Supported client folders that are not active yet, from the detected installation and any
/// other installation found in the candidate directories.
#[tauri::command]
fn list_client_candidates(state: tauri::State<'_, AppState>) -> Vec<ClientCandidate> {
    let settings = state.snapshot();
    let (_, _, active) = active_clients(&settings, None);
    let active_ids: HashSet<String> = active.into_iter().map(|client| client.id).collect();
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for root in candidate_wow_dirs(&settings)
        .into_iter()
        .filter(|dir| dir.is_dir())
    {
        let versions = read_build_info(&root);
        for dir in flavour_dirs(&root) {
            let Some(def) = client_from_dir(&dir).filter(is_supported) else {
                continue;
            };
            if active_ids.contains(&def.id) || !seen.insert(def.id.clone()) {
                continue;
            }
            candidates.push(ClientCandidate {
                has_data: accounts(&dir)
                    .iter()
                    .any(|account| file_has_catalogs(&account.file())),
                version: versions.get(&def.product).cloned(),
                id: def.id,
                label: def.label,
                product: def.product,
            });
        }
    }
    candidates
}

/// Registers a folder chosen by the user: a supported client folder becomes an extra
/// client, a WoW installation folder ("World of Warcraft", holding .build.info or
/// flavour folders) an extra installation whose installed supported clients are active
/// from then on. Folders of other flavours are rejected.
fn register_folder(settings: &mut Settings, path: &Path) -> Result<String, String> {
    if let Some(def) = client_from_dir(path) {
        if !is_supported(&def) {
            return Err(unsupported_message(&def));
        }
        if !settings.extra_clients.contains(&def.id) {
            settings.extra_clients.push(def.id.clone());
        }
        return Ok(def.id);
    }
    let flavours: Vec<ClientDef> = flavour_dirs(path)
        .iter()
        .filter_map(|dir| client_from_dir(dir))
        .collect();
    if !is_wow_dir(path) && flavours.is_empty() {
        return Err(format!(
            "'{}' is neither a WoW installation folder (with .build.info or folders such as _classic_beta_) nor a client folder (with .flavor.info and Wow*.exe).",
            path.display()
        ));
    }
    let supported: Vec<ClientDef> = flavours.into_iter().filter(is_supported).collect();
    if supported.is_empty() {
        return Err(format!(
            "'{}' contains no supported client (_classic_beta_ or _classic_era_). Only Forever and Classic Era / Hardcore are supported; Retail and the other Classic versions are not supported yet.",
            path.display()
        ));
    }
    let root = normalize_dir(path).to_string_lossy().into_owned();
    if !settings.extra_installations.contains(&root) {
        settings.extra_installations.push(root.clone());
    }
    // Without a launcher file nothing counts as installed, so activate the flavours themselves.
    if !is_wow_dir(path) {
        for def in supported {
            if !settings.extra_clients.contains(&def.id) {
                settings.extra_clients.push(def.id);
            }
        }
    }
    Ok(root)
}

/// What `add_client` registered: the folder's identity (which may differ in spelling
/// from what the dialog returned) and the settings as the window may see them.
#[derive(Serialize)]
struct AddedFolder {
    id: String,
    settings: SettingsView,
}

#[tauri::command]
fn add_client(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    dir: String,
) -> Result<AddedFolder, String> {
    let path = PathBuf::from(dir.trim_end_matches(['/', '\\']));
    let mut outcome = Err(String::new());
    let settings = state.update(|settings| outcome = register_folder(settings, &path));
    outcome.map(|id| AddedFolder {
        id,
        settings: settings.view(&app),
    })
}

/// Removes an added client; when it was the last client of an added installation the
/// installation is dropped as well, and an installed client of an added installation
/// removes that installation.
#[tauri::command]
fn remove_client(app: AppHandle, state: tauri::State<'_, AppState>, id: String) -> SettingsView {
    let parent = Path::new(&id)
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned());
    state
        .update(|settings| {
            settings.extra_clients.retain(|entry| entry != &id);
            if let Some(parent) = parent {
                let siblings_left = settings.extra_clients.iter().any(|entry| {
                    Path::new(entry)
                        .parent()
                        .map(|dir| dir.to_string_lossy().into_owned())
                        == Some(parent.clone())
                });
                if !siblings_left {
                    settings
                        .extra_installations
                        .retain(|entry| entry != &parent);
                }
            }
        })
        .view(&app)
}

fn client_by_id(settings: &Settings, id: &str) -> Result<ClientDef, String> {
    let (_, _, active) = active_clients(settings, None);
    if !active.iter().any(|client| client.id == id) {
        return Err(format!("Client '{id}' is not active."));
    }
    client_from_dir(Path::new(id))
        .ok_or_else(|| format!("Client folder '{id}' can no longer be read."))
}

/// reqwest has no default timeout: a stalled upload would block the watcher for good.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Generous for 10 MiB on a slow line; the ingress rejects anything larger anyway.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(180);
const POLL_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Carries the user ID with every upload, so an admin can block a user and delete their data.
const USER_HEADER: &str = "X-ForeverDB-User";

/// SHA-256 of the body as sent: the ingress refuses a body that does not match, and
/// recognises a repeated upload of the same content by it.
const CONTENT_HASH_HEADER: &str = "X-ForeverDB-Content-SHA256";

const RETRY_BASE: Duration = Duration::from_secs(3);
const RETRY_MAX: Duration = Duration::from_secs(300);

/// Exponential backoff after `attempts` transient failures in a row: 3 s, 6 s, 12 s, … up
/// to five minutes.
fn retry_delay(attempts: u32) -> Duration {
    RETRY_BASE
        .saturating_mul(1u32 << attempts.saturating_sub(1).min(16))
        .min(RETRY_MAX)
}

/// Reads Retry-After in its seconds form; the HTTP-date form is not sent by the ingress.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|seconds| Duration::from_secs(seconds).min(RETRY_MAX))
}

/// Identifies the upload to the ingress; it ends up in `import_jobs.source`, so a
/// parser regression can be traced to a client release.
const CLIENT_TAG: &str = concat!("foreverdb-uploader/", env!("CARGO_PKG_VERSION"));

fn gzip(payload: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload)?;
    encoder.finish()
}

/// Whether a rejection would repeat itself with the same content. 408 and 429 ask to
/// try again later and therefore do not count.
fn is_permanent_rejection(status: reqwest::StatusCode) -> bool {
    status.is_client_error()
        && status != reqwest::StatusCode::REQUEST_TIMEOUT
        && status != reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Remembers the content as handled so the watcher does not touch it again. `secured`
/// tells a real upload from a permanent rejection: only the former moves the point in
/// time from which the file counts as secured.
fn remember_upload(state: &AppState, account: &str, content_hash: String, secured: bool) {
    state.update(|settings| {
        settings
            .last_upload_hash
            .insert(account.to_string(), content_hash);
        if secured {
            if let Some(now) = unix_seconds(std::time::SystemTime::now()) {
                settings.last_upload_at.insert(account.to_string(), now);
            }
        }
    });
}

/// Uploads an account's SavedVariables (or their .bak, see `snapshot_source`), keeps an
/// archive copy, and removes the uploaded data so the next session starts empty.
async fn upload_client(
    state: &AppState,
    client_id: &str,
    account_key: &str,
) -> Result<UploadResult, String> {
    let Some(_slot) = state.try_start_upload(account_key) else {
        return Err("An upload for this account is already running.".to_string());
    };
    let def = client_by_id(&state.snapshot(), client_id)?;
    let client_dir = Path::new(&def.id);
    let accounts = accounts(client_dir);
    if accounts.is_empty() {
        return Err(format!(
            "No account folder found under '{}'.",
            client_dir.join("WTF").join("Account").display()
        ));
    }
    let account = accounts
        .into_iter()
        .find(|account| account.key() == account_key)
        .ok_or_else(|| format!("The account folder '{account_key}' no longer exists."))?;
    let main_path = account.file();
    let label = archive_label(&def.label, &account.name);
    let source = snapshot_source(&main_path).ok_or_else(|| {
        if main_path.is_file() {
            "The file holds no collected data yet (empty database since the last upload)."
                .to_string()
        } else {
            format!("No ForeverCollect.lua found: {}", main_path.display())
        }
    })?;
    let content = fs::read(&source.path).map_err(|e| format!("Cannot read the file: {e}"))?;
    let content_hash = content_hash(&content);

    // The server only accepts JSON. If the conversion fails, the snapshot itself is
    // broken or too new: archive it, mark it as handled (otherwise the watcher retries
    // every few seconds) and leave the file in place.
    let snapshot = match lua::parse_forever_collect(&content) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = archive_snapshot(&state.archive_dir, &label, None, &content);
            remember_upload(state, account_key, content_hash, false);
            return Err(format!("Could not read the snapshot: {error}"));
        }
    };
    let payload = serde_json::to_vec(&snapshot)
        .map_err(|e| format!("Could not encode the snapshot as JSON: {e}"))?;
    let compressed = gzip(&payload).map_err(|e| format!("Could not prepare the upload: {e}"))?;

    let user_id = state.user_id(&account.bnet_id);
    let http = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(UPLOAD_TIMEOUT)
        .build()
        .map_err(|e| format!("Could not create the HTTP client: {e}"))?;
    let response = http
        .post(target_url())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::CONTENT_ENCODING, "gzip")
        .header("X-ForeverDB-Client", CLIENT_TAG)
        .header(USER_HEADER, user_id)
        .header(CONTENT_HASH_HEADER, self::content_hash(&compressed))
        .body(compressed)
        .send()
        .await
        .map_err(|e| {
            state.defer_retry(account_key, None);
            format!("Upload failed: {e}")
        })?;
    let status = response.status();
    let wait = retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // A permanent rejection always repeats itself for the same content.
        if is_permanent_rejection(status) {
            state.clear_retry(account_key);
            let _ = archive_snapshot(&state.archive_dir, &label, None, &content);
            remember_upload(state, account_key, content_hash, false);
        } else {
            state.defer_retry(account_key, wait);
        }
        return Err(format!(
            "The server rejected the upload ({}): {}",
            status,
            body.trim()
        ));
    }
    let import_id = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| json.get("id").and_then(|id| id.as_str()).map(str::to_owned));
    state.clear_retry(account_key);
    remember_upload(state, account_key, content_hash, true);
    let archived = archive_snapshot(&state.archive_dir, &label, import_id.as_deref(), &content);
    {
        let mut unarchived = state.unarchived.lock().unwrap();
        if archived.is_ok() {
            unarchived.remove(account_key);
        } else {
            unarchived.insert(account_key.to_string());
        }
    }

    let running = wow_is_running(&def.process);
    // Without an archive copy the SavedVariables are the only copy left if the server later
    // fails the import, so they stay until a later upload archives successfully.
    let deleted = archived.is_ok() && delete_uploaded_snapshot(&source, &main_path, running);
    Ok(UploadResult {
        import_id,
        file_path: source.path.to_string_lossy().into_owned(),
        running,
        deleted,
    })
}

/// Removes an uploaded snapshot. WoW rewrites the SavedVariables on logout, so the current
/// file is only cleared once the game is closed; an uploaded .bak is never read by the game
/// again. Returns whether the uploaded data is gone from the current file.
fn delete_uploaded_snapshot(source: &SnapshotSource, main_path: &Path, running: bool) -> bool {
    if source.from_backup {
        let _ = fs::remove_file(&source.path);
        !running && !file_has_catalogs(main_path) && strip_uploaded_data(main_path)
    } else {
        let removed = !running && strip_uploaded_data(&source.path);
        if removed {
            let _ = fs::remove_file(backup_path(&source.path));
        }
        removed
    }
}

/// Clears the collected data but keeps the addon settings: only the `ForeverCollectDB`
/// global goes, every other one (`ForeverCollectSettings`) stays as written. A file with
/// nothing else in it is removed; one that does not parse is left alone.
fn strip_uploaded_data(path: &Path) -> bool {
    let Ok(rest) = fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|source| lua::without_forever_collect(&source))
    else {
        return false;
    };
    if rest.is_empty() {
        return fs::remove_file(path).is_ok();
    }
    let temp = PathBuf::from(format!("{}.tmp", path.display()));
    if fs::write(&temp, &rest)
        .and_then(|()| fs::rename(&temp, path))
        .is_err()
    {
        let _ = fs::remove_file(&temp);
        return false;
    }
    true
}

const ARCHIVE_LIMIT: usize = 30;

/// Keeps a copy of every uploaded snapshot so nothing is lost even if the server
/// rejects or loses an import; only the newest ARCHIVE_LIMIT copies are kept.
fn archive_snapshot(
    archive_dir: &Path,
    label: &str,
    import_id: Option<&str>,
    content: &[u8],
) -> Result<PathBuf, String> {
    fs::create_dir_all(archive_dir)
        .map_err(|e| format!("Could not create the archive folder: {e}"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("{stamp}-{label}-{}.lua", archive_id(import_id));
    let archived = archive_dir.join(name);
    fs::write(&archived, content).map_err(|e| format!("Could not write the archive copy: {e}"))?;
    let mut files: Vec<PathBuf> = fs::read_dir(archive_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "lua"))
        .collect();
    files.sort();
    for old in files.iter().take(files.len().saturating_sub(ARCHIVE_LIMIT)) {
        let _ = fs::remove_file(old);
    }
    Ok(archived)
}

/// Client and account in a form fit for a file name ("Classic Era", "1234#1" ->
/// "Classic_Era-1234_1").
fn archive_label(client_label: &str, account_name: &str) -> String {
    let clean = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("{}-{}", clean(client_label), clean(account_name))
}

/// The import id comes from the server and ends up in a file name, so anything but a
/// plain id is replaced.
fn archive_id(import_id: Option<&str>) -> &str {
    match import_id {
        Some(id)
            if !id.is_empty()
                && id.len() <= 64
                && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') =>
        {
            id
        }
        _ => "upload",
    }
}

#[tauri::command]
async fn upload(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    client: String,
    account: String,
) -> Result<UploadResult, String> {
    let result = upload_client(&state, &client, &account).await?;
    follow_import(&app, &client, &account, &result);
    Ok(result)
}

#[tauri::command]
fn get_settings(app: AppHandle, state: tauri::State<'_, AppState>) -> SettingsView {
    state.snapshot().view(&app)
}

#[tauri::command]
fn set_auto_upload(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> SettingsView {
    state
        .update(|settings| settings.auto_upload = enabled)
        .view(&app)
}

#[tauri::command]
fn set_close_to_tray(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> SettingsView {
    state
        .update(|settings| settings.close_to_tray = enabled)
        .view(&app)
}

/// What the user reads when the API refuses a pairing code.
fn link_rejection(status: reqwest::StatusCode) -> String {
    match status.as_u16() {
        400 | 404 => "The code is wrong, used or expired. Create a new one in your profile.",
        403 => "This uploader is blocked.",
        409 => {
            "This uploader is linked to another account. Unlink it in that account's profile first."
        }
        503 => "Accounts are not available on the server right now.",
        _ => return format!("The server could not link the uploader ({status})."),
    }
    .to_string()
}

/// Redeems a pairing code from the profile page, which links the user ID of `user` (a
/// Battle.net account, see `AccountDir::bnet_id`) to that ForeverDB account.
#[tauri::command]
async fn link_account(
    state: tauri::State<'_, AppState>,
    code: String,
    user: String,
) -> Result<String, String> {
    let code = code.trim();
    if code.is_empty() {
        return Err("Enter the code from your profile on foreverdb.app.".to_string());
    }
    let http = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(POLL_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("Could not create the HTTP client: {e}"))?;
    let response = http
        .post(format!(
            "{}/v1/uploader/link",
            api_url().trim_end_matches('/')
        ))
        .header("X-ForeverDB-Client", CLIENT_TAG)
        .json(&serde_json::json!({ "uploaderId": state.user_id(&user), "code": code }))
        .send()
        .await
        .map_err(|e| format!("Could not reach ForeverDB: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(link_rejection(status));
    }
    let account = response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|json| json.get("account")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "your account".to_string());
    state.update(|settings| {
        if let Some(identity) = settings.identities.get_mut(&user) {
            identity.linked_account = Some(account.clone());
        }
    });
    Ok(account)
}

/// Adds or removes the system's autostart entry (Linux: ~/.config/autostart, Windows:
/// the Run key); it starts the app with --minimized.
#[tauri::command]
fn set_autostart(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<SettingsView, String> {
    let autolaunch = app.autolaunch();
    let changed = if enabled {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };
    changed.map_err(|error| format!("Could not change the autostart entry: {error}"))?;
    Ok(state.snapshot().view(&app))
}

/// How long a release check stays valid before the watcher asks GitHub again.
const RELEASE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Asks GitHub for the latest addon release, or returns the cached answer.
async fn latest_release(state: &AppState, force: bool) -> Result<AddonRelease, String> {
    if !force {
        if let Some(cache) = state.release.lock().unwrap().clone() {
            if cache.checked_at.elapsed() < RELEASE_CHECK_INTERVAL {
                return cache.result;
            }
        }
    }
    let result = addon::fetch_latest_release().await;
    *state.release.lock().unwrap() = Some(ReleaseCache {
        checked_at: std::time::Instant::now(),
        result: result.clone(),
    });
    result
}

/// Asks GitHub for the latest uploader release, or returns the cached answer.
async fn latest_uploader_release(state: &AppState, force: bool) -> Result<UploaderRelease, String> {
    if !force {
        if let Some(cache) = state.uploader_release.lock().unwrap().clone() {
            if cache.checked_at.elapsed() < RELEASE_CHECK_INTERVAL {
                return cache.result;
            }
        }
    }
    let result = update::fetch_latest_release().await;
    *state.uploader_release.lock().unwrap() = Some(ReleaseCache {
        checked_at: std::time::Instant::now(),
        result: result.clone(),
    });
    result
}

#[tauri::command]
async fn check_addon_update(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    force: bool,
) -> Result<AddonRelease, String> {
    let result = latest_release(&state, force).await;
    refresh_tray(&app);
    result
}

/// The running version and the latest release, for the window.
#[derive(Serialize)]
struct UploaderUpdate {
    current: String,
    latest: UploaderRelease,
    available: bool,
}

#[tauri::command]
async fn check_uploader_update(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    force: bool,
) -> Result<UploaderUpdate, String> {
    let result = latest_uploader_release(&state, force).await;
    refresh_tray(&app);
    result.map(|latest| UploaderUpdate {
        current: update::CURRENT_VERSION.to_string(),
        available: latest.is_update(),
        latest,
    })
}

#[derive(Serialize)]
struct InstallResult {
    version: String,
    addon_dir: String,
    running: bool,
}

#[tauri::command]
async fn install_addon(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    client: String,
) -> Result<InstallResult, String> {
    let def = client_by_id(&state.snapshot(), &client)?;
    let release = latest_release(&state, false).await?;
    let zip_bytes = addon::download_asset(&release).await?;
    let client_dir = PathBuf::from(&def.id);
    let version = release.version.clone();
    tauri::async_runtime::spawn_blocking(move || {
        addon::install_from_zip(&client_dir, &zip_bytes, &version)
    })
    .await
    .map_err(|e| format!("Installation aborted: {e}"))??;
    refresh_tray(&app);
    Ok(InstallResult {
        version: release.version,
        addon_dir: addon::addon_dir(Path::new(&def.id))
            .to_string_lossy()
            .into_owned(),
        running: wow_is_running(&def.process),
    })
}

/// Labels of the clients whose installed addon is older than `release`.
fn outdated_clients(settings: &Settings, release: &AddonRelease) -> Vec<String> {
    let (_, _, clients) = active_clients(settings, Some(release));
    clients
        .into_iter()
        .filter(|client| client.addon_update.is_some())
        .map(|client| client.label)
        .collect()
}

/// What a release check found and whether it is news, see `announce_updates`.
struct Updates {
    uploader: Result<UploaderRelease, String>,
    addon: Result<AddonRelease, String>,
}

async fn check_updates(app: &AppHandle, force: bool) -> Updates {
    let state = app.state::<AppState>();
    let updates = Updates {
        uploader: latest_uploader_release(&state, force).await,
        addon: latest_release(&state, force).await,
    };
    refresh_tray(app);
    updates
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Reports new releases to the window (once per session) and as a desktop notification
/// (once per version, remembered in the settings). A check by hand reports everything,
/// including that nothing is new.
fn announce_updates(
    app: &AppHandle,
    updates: &Updates,
    by_hand: bool,
    announced: &mut HashSet<String>,
) {
    let state = app.state::<AppState>();
    let settings = state.snapshot();
    let mut news = false;

    match &updates.uploader {
        Ok(release) if release.is_update() => {
            news = true;
            let message = format!(
                "ForeverDB Uploader v{} is available (installed: v{}).",
                release.version,
                update::CURRENT_VERSION
            );
            if by_hand || announced.insert(format!("uploader {}", release.version)) {
                emit_activity(app, "", None, "update", message, None);
            }
            if by_hand || settings.notified_uploader_version.as_ref() != Some(&release.version) {
                notify(
                    app,
                    &format!("ForeverDB Uploader v{} is available", release.version),
                    &format!(
                        "Installed: v{}. Download it from the tray menu or the uploader window.",
                        update::CURRENT_VERSION
                    ),
                );
                let version = release.version.clone();
                state.update(|settings| settings.notified_uploader_version = Some(version));
            }
        }
        Ok(_) => {}
        Err(error) => emit_activity(
            app,
            "",
            None,
            "error",
            format!("Uploader update check: {error}"),
            None,
        ),
    }

    match &updates.addon {
        Ok(release) => {
            let outdated = outdated_clients(&settings, release);
            if !outdated.is_empty() {
                news = true;
                let message = format!(
                    "ForeverCollect v{} is available (outdated in: {}).",
                    release.version,
                    outdated.join(", ")
                );
                if by_hand || announced.insert(format!("addon {}", release.version)) {
                    emit_activity(app, "", None, "update", message, None);
                }
                if by_hand || settings.notified_addon_version.as_ref() != Some(&release.version) {
                    notify(
                        app,
                        &format!("ForeverCollect v{} is available", release.version),
                        &format!(
                            "Outdated in: {}. Install it in the ForeverDB Uploader.",
                            outdated.join(", ")
                        ),
                    );
                    let version = release.version.clone();
                    state.update(|settings| settings.notified_addon_version = Some(version));
                }
            }
        }
        Err(error) => emit_activity(
            app,
            "",
            None,
            "error",
            format!("Addon update check: {error}"),
            None,
        ),
    }

    if by_hand && !news {
        let failed = updates.uploader.is_err() || updates.addon.is_err();
        notify(
            app,
            if failed {
                "Update check failed"
            } else {
                "No updates found"
            },
            &if failed {
                "See the activity list in the uploader window.".to_string()
            } else {
                format!(
                    "ForeverDB Uploader v{} and the installed ForeverCollect are up to date.",
                    update::CURRENT_VERSION
                )
            },
        );
    }
}

/// Checks for new uploader and addon releases on start and every RELEASE_CHECK_INTERVAL.
async fn watch_releases(app: AppHandle) {
    let mut announced = HashSet::new();
    loop {
        let updates = check_updates(&app, true).await;
        announce_updates(&app, &updates, false, &mut announced);
        tokio_sleep(RELEASE_CHECK_INTERVAL).await;
    }
}

const WATCH_INTERVAL: Duration = Duration::from_secs(3);
/// A file is only read once WoW has stopped writing it for this long.
const SETTLE_TIME: Duration = Duration::from_secs(3);

fn emit_activity(
    app: &AppHandle,
    client: &str,
    account: Option<&str>,
    kind: &str,
    message: String,
    result: Option<UploadResult>,
) {
    let _ = app.emit(
        "activity",
        Activity {
            client: client.to_string(),
            account: account.map(str::to_owned),
            kind: kind.to_string(),
            message,
            result,
            import: None,
        },
    );
}

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// The worker redelivers an import after 15 minutes without an ack; give it a bit
/// longer than that before the client stops asking.
const POLL_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Network errors in a row after which the poll gives up.
const POLL_MAX_FAILURES: u32 = 10;

/// Follows an accepted import on the server until it is completed or failed and
/// reports every status change to the window.
async fn poll_import(app: AppHandle, client_id: String, account: String, import_id: String) {
    let url = status_url(&target_url(), &import_id);
    let Ok(http) = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(POLL_REQUEST_TIMEOUT)
        .build()
    else {
        return;
    };
    let started = std::time::Instant::now();
    let mut last_status = String::from("queued");
    let mut failures = 0;
    loop {
        tokio_sleep(POLL_INTERVAL).await;
        let json = match http.get(&url).send().await {
            Ok(response) if response.status().is_success() => {
                response.json::<serde_json::Value>().await.ok()
            }
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                emit_import(
                    &app,
                    &client_id,
                    &account,
                    ImportStatus {
                        import_id,
                        status: "failed".into(),
                        error: Some("The server no longer knows this import.".into()),
                    },
                );
                return;
            }
            _ => None,
        };
        let Some(json) = json else {
            failures += 1;
            if failures >= POLL_MAX_FAILURES {
                emit_activity(
                    &app,
                    &client_id,
                    Some(&account),
                    "error",
                    format!(
                        "Cannot fetch the status of import {import_id}; please check again later."
                    ),
                    None,
                );
                return;
            }
            continue;
        };
        failures = 0;
        let status = json
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("queued")
            .to_string();
        let error = json
            .get("error")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        if status != last_status {
            emit_import(
                &app,
                &client_id,
                &account,
                ImportStatus {
                    import_id: import_id.clone(),
                    status: status.clone(),
                    error,
                },
            );
            last_status = status.clone();
        }
        if status == "completed" || status == "failed" {
            return;
        }
        if started.elapsed() >= POLL_TIMEOUT {
            emit_activity(
                &app,
                &client_id,
                Some(&account),
                "error",
                format!(
                    "Import {import_id} has not finished after {} minutes.",
                    POLL_TIMEOUT.as_secs() / 60
                ),
                None,
            );
            return;
        }
    }
}

fn emit_import(app: &AppHandle, client: &str, account: &str, import: ImportStatus) {
    let (kind, message) = match import.status.as_str() {
        "processing" => (
            "processing",
            format!("Import {} is being processed.", import.import_id),
        ),
        "completed" => (
            "completed",
            format!("Import {} completed.", import.import_id),
        ),
        "failed" => (
            "failed",
            format!(
                "Import {} failed: {}",
                import.import_id,
                import.error.as_deref().unwrap_or("unknown error")
            ),
        ),
        _ => (
            "pending",
            format!("Import {} is waiting in the queue.", import.import_id),
        ),
    };
    let _ = app.emit(
        "activity",
        Activity {
            client: client.to_string(),
            account: Some(account.to_string()),
            kind: kind.to_string(),
            message,
            result: None,
            import: Some(import),
        },
    );
}

/// Starts following the import an upload produced, if the server returned one.
fn follow_import(app: &AppHandle, client_id: &str, account: &str, result: &UploadResult) {
    if let Some(id) = &result.import_id {
        tauri::async_runtime::spawn(poll_import(
            app.clone(),
            client_id.to_string(),
            account.to_string(),
            id.clone(),
        ));
    }
}

fn is_settled(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= SETTLE_TIME)
}

/// Whether a snapshot should be uploaded now: it must hold catalogs, WoW must have
/// finished writing it, and it must differ from the last uploaded content. The game
/// process state does not matter: WoW writes the file on every logout and /reload,
/// and each write is worth saving right away.
fn should_upload(has_data: bool, settled: bool, already_uploaded: bool) -> bool {
    has_data && settled && !already_uploaded
}

/// Whether an already uploaded snapshot can be removed now. The upload right after a
/// logout usually still sees the game process (it is shutting down, or only the character
/// screen was reached), so the watcher removes the unchanged file once the game is closed.
fn should_delete(already_uploaded: bool, running: bool) -> bool {
    already_uploaded && !running
}

/// Watches the SavedVariables of every account of every active client and uploads each
/// new write.
async fn watch_and_upload(app: AppHandle) {
    loop {
        tokio_sleep(WATCH_INTERVAL).await;
        let state = app.state::<AppState>();
        if !state.snapshot().auto_upload {
            continue;
        }
        let (_, _, clients) = active_clients(&state.snapshot(), None);
        state.ensure_identities(&clients);
        let settings = state.snapshot();
        for client in &clients {
            for account in &client.accounts {
                watch_account(&app, &state, &settings, client, account).await;
            }
        }
    }
}

async fn watch_account(
    app: &AppHandle,
    state: &AppState,
    settings: &Settings,
    client: &ClientStatus,
    account: &AccountStatus,
) {
    let key = account.key.as_str();
    let name = format!("{} · {}", client.label, account.name);
    let path = PathBuf::from(&account.file_path);
    let already_uploaded =
        file_hash(&path).is_some_and(|hash| settings.last_upload_hash.get(key) == Some(&hash));
    if should_delete(already_uploaded, client.running) {
        let main_path = Path::new(key)
            .join("SavedVariables")
            .join("ForeverCollect.lua");
        let source = SnapshotSource {
            path: path.clone(),
            from_backup: account.from_backup,
        };
        // No archive copy yet: retry it, and keep the file if that fails too.
        if state.unarchived.lock().unwrap().contains(key) {
            let archived = fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|content| {
                    archive_snapshot(
                        &state.archive_dir,
                        &archive_label(&client.label, &account.name),
                        None,
                        &content,
                    )
                });
            if archived.is_err() {
                return;
            }
            state.unarchived.lock().unwrap().remove(key);
        }
        if delete_uploaded_snapshot(&source, &main_path, false) {
            emit_activity(
                app,
                &client.id,
                Some(key),
                "cleaned",
                format!("{name}: uploaded data cleared after the game closed; settings kept."),
                None,
            );
        }
        return;
    }
    if !should_upload(account.file_has_data, is_settled(&path), already_uploaded) {
        return;
    }
    if state.uploading.lock().unwrap().contains(key) || !state.retry_due(key) {
        return;
    }
    emit_activity(
        app,
        &client.id,
        Some(key),
        "pending",
        if account.from_backup {
            format!("{name}: found a backup with data not yet uploaded, starting upload.")
        } else {
            format!("{name}: new data written, starting upload.")
        },
        None,
    );
    match upload_client(state, &client.id, key).await {
        Ok(result) => {
            follow_import(app, &client.id, key, &result);
            emit_activity(
                app,
                &client.id,
                Some(key),
                "uploaded",
                match &result.import_id {
                    Some(id) => format!("{name}: upload succeeded (import ID {id})."),
                    None => format!("{name}: upload succeeded."),
                },
                Some(result),
            )
        }
        Err(error) => emit_activity(
            app,
            &client.id,
            Some(key),
            "error",
            format!("{name}: {error}"),
            None,
        ),
    }
}

async fn tokio_sleep(duration: Duration) {
    tauri::async_runtime::spawn_blocking(move || std::thread::sleep(duration))
        .await
        .ok();
}

/// Identifier of the releases before 0.2; their settings and archive are taken over once.
const LEGACY_IDENTIFIER: &str = "com.alex.foreverdb-client";
/// Passed by the autostart entry: start in the tray without showing the window.
const MINIMIZED_ARG: &str = "--minimized";

/// Copies `file` from the legacy directory next to `dir` unless `dir` already has one.
fn migrate_legacy_file(dir: &Path, file: &str) {
    let target = dir.join(file);
    let Some(legacy) = dir
        .parent()
        .map(|parent| parent.join(LEGACY_IDENTIFIER).join(file))
    else {
        return;
    };
    if target.exists() || !legacy.is_file() {
        return;
    }
    let _ = fs::create_dir_all(dir);
    let _ = fs::copy(&legacy, &target);
}

/// Moves the legacy archive next to `dir` into `dir` unless `dir` already has one.
fn migrate_legacy_archive(dir: &Path, name: &str) {
    let target = dir.join(name);
    let Some(legacy) = dir
        .parent()
        .map(|parent| parent.join(LEGACY_IDENTIFIER).join(name))
    else {
        return;
    };
    if target.exists() || !legacy.is_dir() {
        return;
    }
    let _ = fs::create_dir_all(dir);
    if fs::rename(&legacy, &target).is_err() {
        // Different file systems: copy instead, the legacy copy stays.
        let _ = copy_dir(&legacy, &target);
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// The update entries of the tray menu, rewritten after every release check.
struct TrayMenu {
    uploader: MenuItem<tauri::Wry>,
    addon: MenuItem<tauri::Wry>,
}

/// Texts of the tray's update entries and whether they can be clicked (only an
/// available update leads somewhere).
fn tray_texts(
    uploader: Option<&Result<UploaderRelease, String>>,
    addon: Option<&Result<AddonRelease, String>>,
    outdated: bool,
) -> ((String, bool), (String, bool)) {
    let current = update::CURRENT_VERSION;
    let uploader = match uploader {
        Some(Ok(release)) if release.is_update() => (
            format!("Uploader v{} available – download …", release.version),
            true,
        ),
        Some(Ok(_)) => (format!("Uploader v{current} – up to date"), false),
        Some(Err(_)) => (format!("Uploader v{current} – update check failed"), false),
        None => (format!("Uploader v{current}"), false),
    };
    let addon = match addon {
        Some(Ok(release)) if outdated => (
            format!("ForeverCollect v{} available – install …", release.version),
            true,
        ),
        Some(Ok(release)) => (
            format!("ForeverCollect v{} – up to date", release.version),
            false,
        ),
        Some(Err(_)) => ("ForeverCollect – update check failed".to_string(), false),
        None => ("ForeverCollect – not checked yet".to_string(), false),
    };
    (uploader, addon)
}

/// Shows the cached release checks in the tray menu.
fn refresh_tray(app: &AppHandle) {
    let Some(menu) = app.try_state::<TrayMenu>() else {
        return;
    };
    let state = app.state::<AppState>();
    let uploader = state
        .uploader_release
        .lock()
        .unwrap()
        .clone()
        .map(|cache| cache.result);
    let addon = state
        .release
        .lock()
        .unwrap()
        .clone()
        .map(|cache| cache.result);
    let outdated = addon
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .is_some_and(|release| !outdated_clients(&state.snapshot(), release).is_empty());
    let ((uploader_text, uploader_enabled), (addon_text, addon_enabled)) =
        tray_texts(uploader.as_ref(), addon.as_ref(), outdated);
    let _ = menu.uploader.set_text(uploader_text);
    let _ = menu.uploader.set_enabled(uploader_enabled);
    let _ = menu.addon.set_text(addon_text);
    let _ = menu.addon.set_enabled(addon_enabled);
}

/// Tray icon with "Show", the update state of uploader and addon, "Check for updates" and
/// "Quit"; a left click shows the window as well.
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let ((uploader_text, _), (addon_text, _)) = tray_texts(None, None, false);
    let show = MenuItem::with_id(app, "show", "Show ForeverDB Uploader", true, None::<&str>)?;
    let uploader = MenuItem::with_id(app, "uploader_update", uploader_text, false, None::<&str>)?;
    let addon = MenuItem::with_id(app, "addon_update", addon_text, false, None::<&str>)?;
    let check = MenuItem::with_id(
        app,
        "check_updates",
        "Check for updates",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &PredefinedMenuItem::separator(app)?,
            &uploader,
            &addon,
            &check,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    app.manage(TrayMenu { uploader, addon });
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("ForeverDB Uploader")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "uploader_update" => {
                let state = app.state::<AppState>();
                let url = state
                    .uploader_release
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|cache| cache.result.as_ref().ok())
                    .map(|release| release.html_url.clone());
                if let Some(url) = url {
                    let _ = app.opener().open_url(url, None::<&str>);
                }
            }
            // The addon is installed per client, which the window offers.
            "addon_update" => show_main_window(app),
            "check_updates" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let updates = check_updates(&app, true).await;
                    announce_updates(&app, &updates, true, &mut HashSet::new());
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // First, so a second start only shows the running instance and never spawns a second watcher.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_main_window(app)
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![MINIMIZED_ARG]),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir().ok();
            let data_dir = app.path().app_data_dir().ok();
            if let Some(dir) = &config_dir {
                migrate_legacy_file(dir, "settings.json");
            }
            if let Some(dir) = &data_dir {
                migrate_legacy_archive(dir, "archive");
            }
            let settings_path = config_dir
                .map(|dir| dir.join("settings.json"))
                .unwrap_or_else(|| PathBuf::from("foreverdb-uploader-settings.json"));
            let archive_dir = data_dir
                .map(|dir| dir.join("archive"))
                .unwrap_or_else(|| PathBuf::from("foreverdb-uploader-archive"));
            let state = AppState::load(settings_path, archive_dir);
            app.manage(state);
            build_tray(app)?;
            // The window starts hidden (tauri.conf.json) so an autostart never flashes it.
            if !std::env::args().any(|arg| arg == MINIMIZED_ARG) {
                show_main_window(app.handle());
            }
            tauri::async_runtime::spawn(watch_and_upload(app.handle().clone()));
            tauri::async_runtime::spawn(watch_releases(app.handle().clone()));
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                if window.label() == "main" && state.snapshot().close_to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            detect_installation,
            list_client_candidates,
            add_client,
            remove_client,
            upload,
            get_settings,
            set_auto_upload,
            set_close_to_tray,
            set_autostart,
            link_account,
            check_addon_update,
            check_uploader_update,
            install_addon
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("foreverdb-uploader-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        normalize_dir(&dir)
    }

    #[test]
    fn parses_build_info_columns_by_name() {
        let dir = temp_dir("build-info");
        fs::write(
            dir.join(".build.info"),
            "Branch!STRING:0|Version!STRING:0|Product!STRING:0\neu|1.60.1.69913|wow_classic_beta\neu|12.1.0.69814|wow\n",
        )
        .unwrap();
        let versions = read_build_info(&dir);
        assert_eq!(
            versions.get("wow_classic_beta").map(String::as_str),
            Some("1.60.1.69913")
        );
        assert_eq!(
            versions.get("wow").map(String::as_str),
            Some("12.1.0.69814")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_flavour_folders_as_clients() {
        let root = temp_dir("flavours");
        for (name, product, exe) in [
            ("_classic_beta_", "wow_classic_beta", "WowB.exe"),
            ("_retail_", "wow", "Wow.exe"),
        ] {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(".flavor.info"),
                format!("Product Flavor!STRING:0\n{product}\n"),
            )
            .unwrap();
            fs::write(dir.join(exe), "").unwrap();
            fs::write(dir.join("BlizzardError.exe"), "").unwrap();
        }
        fs::create_dir_all(root.join("Data")).unwrap();
        let dirs = flavour_dirs(&root);
        assert_eq!(dirs.len(), 2, "only _x_ folders count: {dirs:?}");
        let forever = client_from_dir(&root.join("_classic_beta_")).unwrap();
        assert_eq!(
            (forever.label.as_str(), forever.process.as_str()),
            ("Forever", "WowB.exe")
        );
        let retail = client_from_dir(&root.join("_retail_")).unwrap();
        assert_eq!(
            (retail.label.as_str(), retail.process.as_str()),
            ("Retail", "Wow.exe")
        );
        assert!(client_from_dir(&root.join("Data")).is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn gzips_the_request_body() {
        let payload = br#"{"schemaVersion":9}"#;
        let compressed = gzip(payload).unwrap();
        assert_eq!(&compressed[..2], &[0x1f, 0x8b], "gzip magic");
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&compressed[..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn only_permanent_rejections_mark_the_content_as_handled() {
        use reqwest::StatusCode;
        assert!(is_permanent_rejection(StatusCode::BAD_REQUEST));
        assert!(is_permanent_rejection(StatusCode::UNSUPPORTED_MEDIA_TYPE));
        assert!(is_permanent_rejection(StatusCode::PAYLOAD_TOO_LARGE));
        assert!(!is_permanent_rejection(StatusCode::REQUEST_TIMEOUT));
        assert!(!is_permanent_rejection(StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_permanent_rejection(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!is_permanent_rejection(StatusCode::INTERNAL_SERVER_ERROR));
    }

    #[test]
    fn status_url_sits_next_to_the_upload_endpoint() {
        assert_eq!(
            status_url("https://host/imports/forevercollect", "abc"),
            "https://host/imports/abc"
        );
        assert_eq!(
            status_url("https://host/imports/", "abc"),
            "https://host/imports/abc"
        );
        assert_eq!(
            status_url("https://host/imports", "abc"),
            "https://host/imports/abc"
        );
    }

    #[test]
    fn executable_name_takes_the_last_path_component() {
        assert_eq!(
            executable_name("C:\\Program Files (x86)\\World of Warcraft\\_classic_beta_\\WowB.exe"),
            "WowB.exe"
        );
        assert_eq!(
            executable_name("/home/alex/wine/drive_c/WowB.exe"),
            "WowB.exe"
        );
        assert_eq!(executable_name("WowB.exe"), "WowB.exe");
        assert!(!executable_name("WowB.exe").eq_ignore_ascii_case("Wow.exe"));
    }

    fn fake_flavour(root: &Path, name: &str, product: &str, exe: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(".flavor.info"),
            format!("Product Flavor!STRING:0\n{product}\n"),
        )
        .unwrap();
        fs::write(dir.join(exe), "").unwrap();
        dir
    }

    #[test]
    fn register_folder_accepts_clients_and_installations() {
        let root = temp_dir("register");
        let era = fake_flavour(&root, "_classic_era_", "wow_classic_era", "WowClassic.exe");
        let beta = fake_flavour(&root, "_classic_beta_", "wow_classic_beta", "WowB.exe");
        let retail = fake_flavour(&root, "_retail_", "wow", "Wow.exe");

        // a Forever or Classic Era / Hardcore folder becomes an extra client
        for dir in [&beta, &era] {
            let mut settings = Settings::default();
            register_folder(&mut settings, dir).unwrap();
            assert_eq!(
                settings.extra_clients,
                vec![dir.to_string_lossy().into_owned()]
            );
            assert!(settings.extra_installations.is_empty());
        }

        // other flavours are rejected
        let mut settings = Settings::default();
        let error = register_folder(&mut settings, &retail).unwrap_err();
        assert!(error.contains("Only Forever and Classic Era"), "{error}");
        assert!(settings.extra_clients.is_empty());

        // an installation without launcher file activates its supported folders
        let mut settings = Settings::default();
        register_folder(&mut settings, &root).unwrap();
        assert_eq!(
            settings.extra_installations,
            vec![root.to_string_lossy().into_owned()]
        );
        assert_eq!(
            settings.extra_clients,
            vec![
                beta.to_string_lossy().into_owned(),
                era.to_string_lossy().into_owned()
            ]
        );

        // with a launcher file only the installed products count, via the installation
        fs::write(
            root.join(".build.info"),
            "Version!STRING:0|Product!STRING:0\n1.60.1.69913|wow_classic_beta\n",
        )
        .unwrap();
        let mut settings = Settings::default();
        let id = register_folder(&mut settings, &root).unwrap();
        assert_eq!(id, root.to_string_lossy());
        assert!(settings.extra_clients.is_empty());
        let (dirs, _, clients) = clients_among(&settings, vec![root.clone()], None);
        assert_eq!(dirs, vec![root.clone()]);
        assert_eq!(
            clients.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![beta.to_string_lossy().as_ref()]
        );
        assert!(
            clients[0].custom,
            "clients of an added installation can be removed"
        );

        // anything else is rejected
        let junk = root.join("Data");
        fs::create_dir_all(&junk).unwrap();
        assert!(register_folder(&mut Settings::default(), &junk).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ignores_and_rejects_other_flavours() {
        let root = temp_dir("other-flavours");
        let retail = fake_flavour(&root, "_retail_", "wow", "Wow.exe");
        let classic = fake_flavour(&root, "_classic_", "wow_classic", "WowClassic.exe");
        fs::write(
            root.join(".build.info"),
            "Version!STRING:0|Product!STRING:0\n12.1.0.69814|wow\n5.5.1.63538|wow_classic\n",
        )
        .unwrap();

        // an installation with only other flavours cannot be added
        let error = register_folder(&mut Settings::default(), &root).unwrap_err();
        assert!(error.contains("no supported client"), "{error}");

        // installed or previously added clients of other flavours are ignored
        let settings = Settings {
            extra_clients: vec![
                retail.to_string_lossy().into_owned(),
                classic.to_string_lossy().into_owned(),
            ],
            ..Settings::default()
        };
        let (dirs, _, clients) = clients_among(&settings, vec![root.clone()], None);
        assert_eq!(dirs, vec![root.clone()]);
        assert!(
            clients.is_empty(),
            "{:?}",
            clients.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn detects_installed_classic_era_clients() {
        let root = temp_dir("classic-era");
        let era = fake_flavour(&root, "_classic_era_", "wow_classic_era", "WowClassic.exe");
        fs::write(
            root.join(".build.info"),
            "Version!STRING:0|Product!STRING:0\n1.15.7.61582|wow_classic_era\n",
        )
        .unwrap();
        let def = client_from_dir(&era).unwrap();
        assert_eq!(
            (def.label.as_str(), def.process.as_str()),
            ("Classic Era & Hardcore", "WowClassic.exe")
        );
        let (_, _, clients) = clients_among(&Settings::default(), vec![root.clone()], None);
        assert_eq!(
            clients
                .iter()
                .map(|c| (c.id.as_str(), c.version.as_deref()))
                .collect::<Vec<_>>(),
            vec![(era.to_string_lossy().as_ref(), Some("1.15.7.61582"))]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn creates_each_identity_once() {
        let mut settings = Settings::default();
        assert!(ensure_identities(&mut settings, ["1", "2"]));
        let first = settings.identities["1"].user_id.clone();
        let second = settings.identities["2"].user_id.clone();
        assert!(uuid::Uuid::parse_str(&first).is_ok(), "{first}");
        assert_ne!(first, second);
        assert!(!ensure_identities(&mut settings, ["2", "1"]));
        assert_eq!(settings.identities["1"].user_id, first);

        let stored: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(stored.identities, settings.identities);
    }

    #[test]
    fn the_first_user_takes_over_the_legacy_identity() {
        let content = r#"{"user_id": "u-1", "linked_account": "Alex"}"#;
        let mut settings: Settings = serde_json::from_str(content).unwrap();
        assert!(ensure_identities(&mut settings, ["111", "222"]));
        assert_eq!(
            settings.identities["111"],
            Identity {
                user_id: "u-1".into(),
                linked_account: Some("Alex".into())
            }
        );
        assert_ne!(settings.identities["222"].user_id, "u-1");
        assert_eq!(settings.identities["222"].linked_account, None);
        let stored = serde_json::to_value(&settings).unwrap();
        assert!(stored.get("user_id").is_none() && stored.get("linked_account").is_none());
    }

    #[test]
    fn battle_net_accounts_get_their_identity_first() {
        let account = |name: &str| AccountStatus {
            key: format!("/wow/WTF/Account/{name}"),
            name: name.into(),
            bnet_id: bnet_id(name).into(),
            characters: vec![],
            file_path: String::new(),
            file_exists: false,
            file_has_data: false,
            from_backup: false,
            saved_at: None,
            linked_account: None,
        };
        let client = ClientStatus {
            id: "/wow/_classic_era_".into(),
            label: "Classic Era".into(),
            product: "wow_classic_era".into(),
            version: None,
            accounts: vec![
                account("ALEX"),
                account("222#1"),
                account("111#1"),
                account("222#2"),
            ],
            running: false,
            custom: false,
            addon_version: None,
            addon_update: None,
            running_since: None,
            crash_at: None,
        };
        assert_eq!(users_of(&[client]), vec!["222", "111", "ALEX"]);
    }

    #[test]
    fn fresh_and_legacy_settings_close_to_tray_and_upload() {
        let fresh = Settings::default();
        assert!(fresh.auto_upload && fresh.close_to_tray);
        let legacy: Settings = serde_json::from_str(r#"{"auto_upload":false}"#).unwrap();
        assert!(!legacy.auto_upload && legacy.close_to_tray);
    }

    #[test]
    fn migrates_legacy_settings_and_archive_once() {
        let root = temp_dir("migrate");
        let legacy = root.join(LEGACY_IDENTIFIER);
        fs::create_dir_all(legacy.join("archive/sub")).unwrap();
        fs::write(legacy.join("settings.json"), "old").unwrap();
        fs::write(legacy.join("archive/sub/a.lua"), "a").unwrap();
        let current = root.join("gg.foreverdb.client");

        migrate_legacy_file(&current, "settings.json");
        migrate_legacy_archive(&current, "archive");
        assert_eq!(
            fs::read_to_string(current.join("settings.json")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(current.join("archive/sub/a.lua")).unwrap(),
            "a"
        );

        // Existing settings are never overwritten.
        fs::write(current.join("settings.json"), "new").unwrap();
        fs::write(legacy.join("settings.json"), "older").unwrap();
        migrate_legacy_file(&current, "settings.json");
        assert_eq!(
            fs::read_to_string(current.join("settings.json")).unwrap(),
            "new"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn detects_empty_snapshots() {
        let empty = b"\nForeverCollectDB = {\n[\"catalogs\"] = {\n},\n[\"schemaVersion\"] = 9,\n}\nForeverCollectSettings = {\n[\"verbose\"] = false,\n}\n";
        assert!(!snapshot_has_catalogs(empty));
        let filled = b"ForeverCollectDB = {\n[\"catalogs\"] = {\n[\"1:16001:0:enUS:9:7:Alliance\"] = {\n[\"scannedAt\"] = 1,\n},\n},\n}\n";
        assert!(snapshot_has_catalogs(filled));
        assert!(!snapshot_has_catalogs(b"garbage"));
    }

    #[test]
    fn should_delete_only_uploaded_data_of_a_closed_game() {
        assert!(should_delete(true, false));
        assert!(
            !should_delete(true, true),
            "WoW rewrites the file on logout"
        );
        assert!(!should_delete(false, false), "not uploaded yet");
    }

    #[test]
    fn deleting_an_uploaded_snapshot_keeps_newer_data() {
        let dir = temp_dir("delete-snapshot");
        let main = dir.join("ForeverCollect.lua");
        let backup = backup_path(&main);
        let settings = "ForeverCollectSettings = {\n[\"armory\"] = true,\n}";
        let uploaded = format!(
            "\nForeverCollectDB = {{\n[\"catalogs\"] = {{\n[\"k\"] = {{}},\n}},\n}}\n{settings}\n"
        );

        fs::write(&main, &uploaded).unwrap();
        fs::write(&backup, "x").unwrap();
        let current = SnapshotSource {
            path: main.clone(),
            from_backup: false,
        };
        assert!(
            !delete_uploaded_snapshot(&current, &main, true),
            "game still running"
        );
        assert!(main.exists() && backup.exists());
        assert!(delete_uploaded_snapshot(&current, &main, false));
        assert_eq!(fs::read_to_string(&main).unwrap(), format!("{settings}\n"));
        assert!(!backup.exists());

        // without other globals the file goes entirely
        fs::write(&main, "ForeverCollectDB = {\n}\n").unwrap();
        assert!(delete_uploaded_snapshot(&current, &main, false));
        assert!(!main.exists());

        // an unparsable file is left alone
        fs::write(&main, "x").unwrap();
        assert!(!delete_uploaded_snapshot(&current, &main, false));
        assert!(main.exists());

        // an uploaded .bak goes; a current file with new catalogs stays for its own upload
        fs::write(
            &main,
            "ForeverCollectDB = {\n[\"catalogs\"] = {\n[\"k\"] = {},\n},\n}\n",
        )
        .unwrap();
        fs::write(&backup, "x").unwrap();
        let from_backup = SnapshotSource {
            path: backup.clone(),
            from_backup: true,
        };
        assert!(!delete_uploaded_snapshot(&from_backup, &main, false));
        assert!(main.exists() && !backup.exists());

        // an emptied database next to the uploaded .bak loses only its data global
        fs::write(
            &main,
            format!("ForeverCollectDB = {{\n[\"catalogs\"] = {{\n}},\n}}\n{settings}\n"),
        )
        .unwrap();
        fs::write(&backup, &uploaded).unwrap();
        assert!(delete_uploaded_snapshot(&from_backup, &main, false));
        assert_eq!(fs::read_to_string(&main).unwrap(), format!("{settings}\n"));
        assert!(!backup.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn should_upload_only_new_settled_data() {
        assert!(should_upload(true, true, false));
        assert!(!should_upload(false, true, false), "empty database");
        assert!(!should_upload(true, false, false), "still being written");
        assert!(
            !should_upload(true, true, true),
            "same content as last upload"
        );
    }

    #[test]
    fn snapshot_source_falls_back_to_backup() {
        let root = temp_dir("snapshot-source");
        let dir = root
            .join("WTF")
            .join("Account")
            .join("1234#1")
            .join("SavedVariables");
        fs::create_dir_all(&dir).unwrap();
        let main = dir.join("ForeverCollect.lua");
        let backup = dir.join("ForeverCollect.lua.bak");
        let empty = "ForeverCollectDB = {\n[\"catalogs\"] = {\n},\n[\"schemaVersion\"] = 9,\n}\n";
        let filled = "ForeverCollectDB = {\n[\"catalogs\"] = {\n[\"1:16001:0:enUS:9:7:Alliance\"] = {\n},\n},\n}\n";

        assert!(snapshot_source(&main).is_none(), "no files");
        fs::write(&main, empty).unwrap();
        assert!(snapshot_source(&main).is_none(), "empty database only");
        fs::write(&backup, filled).unwrap();
        let source = snapshot_source(&main).unwrap();
        assert!(
            source.from_backup && source.path == backup,
            "backup holds the last session"
        );
        fs::write(&main, filled).unwrap();
        let source = snapshot_source(&main).unwrap();
        assert!(
            !source.from_backup && source.path == main,
            "current file wins when it has data"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn finds_every_account_of_a_client() {
        let root = temp_dir("accounts");
        let account_root = root.join("WTF").join("Account");
        for name in ["222#1", "111#2", "111#1", "LEGACY", "SavedVariables"] {
            fs::create_dir_all(account_root.join(name).join("SavedVariables")).unwrap();
        }
        fs::create_dir_all(account_root.join("333#1")).unwrap();
        fs::create_dir_all(
            account_root
                .join("111#1")
                .join("Wild Growth")
                .join("Strokkur"),
        )
        .unwrap();
        fs::create_dir_all(account_root.join("111#1").join("Lucifron").join("Vizh")).unwrap();
        fs::create_dir_all(
            account_root
                .join("222#1")
                .join("70")
                .join("Dipling-Strokkur"),
        )
        .unwrap();

        let found = accounts(&root);
        let names: Vec<&str> = found.iter().map(|account| account.name.as_str()).collect();
        assert_eq!(names, vec!["111#1", "111#2", "222#1", "LEGACY"]);
        let users: Vec<&str> = found
            .iter()
            .map(|account| account.bnet_id.as_str())
            .collect();
        assert_eq!(users, vec!["111", "111", "222", "LEGACY"]);
        assert_eq!(
            found[0].file(),
            account_root.join("111#1/SavedVariables/ForeverCollect.lua")
        );
        assert_eq!(
            account_characters(&found[0].dir),
            vec!["Vizh-Lucifron", "Strokkur-Wild Growth"]
        );
        assert_eq!(account_characters(&found[2].dir), vec!["Dipling-Strokkur"]);
        assert!(accounts(&root.join("missing")).is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn remembers_uploads_per_account() {
        let dir = temp_dir("remember-upload");
        let state = AppState::load(dir.join("settings.json"), dir.join("archive"));
        remember_upload(&state, "/wow/WTF/Account/1#1", "a".into(), true);
        remember_upload(&state, "/wow/WTF/Account/2#1", "b".into(), false);
        let settings = state.snapshot();
        assert_eq!(settings.last_upload_hash["/wow/WTF/Account/1#1"], "a");
        assert_eq!(settings.last_upload_hash["/wow/WTF/Account/2#1"], "b");
        assert!(settings.last_upload_at.contains_key("/wow/WTF/Account/1#1"));
        assert!(!settings.last_upload_at.contains_key("/wow/WTF/Account/2#1"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tray_entries_lead_somewhere_only_for_updates() {
        let newer = UploaderRelease {
            version: "999.0.0".into(),
            published_at: String::new(),
            html_url: String::new(),
        };
        let current = UploaderRelease {
            version: update::CURRENT_VERSION.into(),
            ..newer.clone()
        };
        let addon = AddonRelease {
            version: "0.6".into(),
            name: String::new(),
            notes: String::new(),
            published_at: String::new(),
            html_url: String::new(),
            asset_name: String::new(),
            asset_size: 0,
            asset_url: String::new(),
        };

        let ((uploader, uploader_enabled), (addon_text, addon_enabled)) =
            tray_texts(Some(&Ok(newer)), Some(&Ok(addon.clone())), true);
        assert_eq!(uploader, "Uploader v999.0.0 available – download …");
        assert!(uploader_enabled);
        assert_eq!(addon_text, "ForeverCollect v0.6 available – install …");
        assert!(addon_enabled);

        let ((uploader, uploader_enabled), (addon_text, addon_enabled)) =
            tray_texts(Some(&Ok(current)), Some(&Ok(addon)), false);
        assert!(uploader.ends_with("up to date") && !uploader_enabled);
        assert_eq!(addon_text, "ForeverCollect v0.6 – up to date");
        assert!(!addon_enabled);

        let ((uploader, _), (addon_text, _)) = tray_texts(Some(&Err("x".into())), None, false);
        assert!(uploader.ends_with("update check failed"));
        assert_eq!(addon_text, "ForeverCollect – not checked yet");
    }

    #[test]
    fn archive_label_fits_a_file_name() {
        assert_eq!(archive_label("Classic Era", "1234#1"), "Classic_Era-1234_1");
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    fn finds_the_current_process() {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_str().unwrap();
        assert!(wow_is_running(name), "the test binary itself runs: {name}");
        let started = wow_process_started(name).unwrap();
        let now = SystemTime::now();
        assert!(
            started <= now + Duration::from_secs(1),
            "start time in the future"
        );
        assert!(
            started >= now - Duration::from_secs(60 * 60),
            "start time too old"
        );
        assert!(!wow_is_running("not-running-4711.exe"));
    }

    #[test]
    fn converts_filetime_to_unix_time() {
        assert_eq!(filetime_to_system_time(0, 0), UNIX_EPOCH);
        // 2024-01-01T00:00:00Z
        let ticks: u64 = 133_485_408_000_000_000;
        let time = filetime_to_system_time(ticks as u32, (ticks >> 32) as u32);
        assert_eq!(unix_seconds(time), Some(1_704_067_200));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn install_path_candidates_include_the_parent() {
        assert_eq!(
            install_path_candidates("C:\\Program Files (x86)\\World of Warcraft\\_retail_\\"),
            vec![
                PathBuf::from("C:\\Program Files (x86)\\World of Warcraft\\_retail_"),
                PathBuf::from("C:\\Program Files (x86)\\World of Warcraft"),
            ]
        );
    }

    #[test]
    fn detects_local_installation_when_present() {
        let settings = Settings::default();
        let (wow_dirs, _, clients) = active_clients(&settings, None);
        println!("wow_dirs = {wow_dirs:?}");
        for client in &clients {
            println!(
                "{} [{}] {:?} running={} custom={}",
                client.label, client.product, client.version, client.running, client.custom
            );
            for account in &client.accounts {
                println!(
                    "  {} ({}) file={} exists={} characters={:?}",
                    account.name,
                    account.bnet_id,
                    account.file_path,
                    account.file_exists,
                    account.characters
                );
            }
        }
        if !wow_dirs.is_empty() {
            assert!(
                !clients.is_empty(),
                "an installation should expose at least one client"
            );
        }
    }

    #[test]
    fn archive_snapshot_reports_write_failure() {
        let dir = temp_dir("archive-failure");
        let blocker = dir.join("not-a-dir");
        fs::write(&blocker, b"x").unwrap();
        assert!(archive_snapshot(&blocker, "Client", None, b"data").is_err());
    }

    #[test]
    fn archive_snapshot_writes_and_sanitizes_id() {
        let dir = temp_dir("archive-id");
        let path = archive_snapshot(&dir, "Client", Some("../../evil"), b"data").unwrap();
        assert_eq!(path.parent().unwrap(), dir.as_path());
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-upload.lua"));
        let path = archive_snapshot(&dir, "Client", Some("0f8c-12ab"), b"data").unwrap();
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-0f8c-12ab.lua"));
    }

    #[test]
    fn parses_valid_settings() {
        let content = r#"{"auto_upload": false, "user_id": "u-1", "extra_clients": ["x"]}"#;
        let (settings, corrupt) = parse_settings(content);
        assert!(!corrupt);
        assert!(!settings.auto_upload);
        assert_eq!(settings.legacy_user_id.as_deref(), Some("u-1"));
        assert_eq!(settings.extra_clients, vec!["x".to_string()]);
    }

    #[test]
    fn keeps_the_user_ids_of_settings_with_a_wrong_type() {
        let (settings, corrupt) = parse_settings(
            r#"{"auto_upload": "yes", "user_id": "u-1", "identities": {"1": {"user_id": "u-2"}, "2": {"user_id": 5}}}"#,
        );
        assert!(corrupt);
        assert_eq!(settings.legacy_user_id.as_deref(), Some("u-1"));
        assert_eq!(settings.identities.len(), 1);
        assert_eq!(settings.identities["1"].user_id, "u-2");
        assert!(settings.auto_upload);
    }

    #[test]
    fn falls_back_to_defaults_for_unreadable_settings() {
        for content in ["", "not json", r#"{"user_id": 5}"#] {
            let (settings, corrupt) = parse_settings(content);
            assert!(corrupt, "{content:?}");
            assert!(settings.legacy_user_id.is_none(), "{content:?}");
        }
    }

    #[test]
    fn keeps_corrupt_settings_and_their_user_id() {
        let dir = temp_dir("corrupt-settings");
        let path = dir.join("settings.json");
        let content = r#"{"auto_upload": 1, "user_id": "u-1"}"#;
        fs::write(&path, content).unwrap();
        let state = AppState::load(path, dir.join("archive"));
        assert_eq!(state.snapshot().legacy_user_id.as_deref(), Some("u-1"));
        let backup = fs::read_to_string(dir.join("settings.json.corrupt")).unwrap();
        assert_eq!(backup, content);
    }

    #[test]
    fn hashes_content_with_sha256() {
        assert_eq!(
            content_hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn upload_slot_is_exclusive_per_client() {
        let dir = temp_dir("upload-slot");
        let state = AppState::load(dir.join("settings.json"), dir.join("archive"));
        let first = state.try_start_upload("a");
        assert!(first.is_some());
        assert!(state.try_start_upload("a").is_none());
        assert!(state.try_start_upload("b").is_some());
        drop(first);
        assert!(state.try_start_upload("a").is_some());
    }

    #[test]
    fn retry_delay_doubles_up_to_five_minutes() {
        assert_eq!(retry_delay(1), Duration::from_secs(3));
        assert_eq!(retry_delay(2), Duration::from_secs(6));
        assert_eq!(retry_delay(5), Duration::from_secs(48));
        assert_eq!(retry_delay(8), RETRY_MAX);
        assert_eq!(retry_delay(u32::MAX), RETRY_MAX);
    }

    #[test]
    fn reads_retry_after_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(30)));
        headers.insert(reqwest::header::RETRY_AFTER, "86400".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(RETRY_MAX));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&headers), None);
    }

    #[test]
    fn transient_failures_hold_the_watcher_back() {
        let dir = temp_dir("retry");
        let state = AppState::load(dir.join("settings.json"), dir.join("archive"));
        assert!(state.retry_due("a"));
        state.defer_retry("a", None);
        assert!(!state.retry_due("a"));
        assert!(state.retry_due("b"));
        state.defer_retry("a", Some(Duration::ZERO));
        assert!(state.retry_due("a"));
        assert_eq!(state.retry.lock().unwrap()["a"].attempts, 2);
        state.clear_retry("a");
        assert!(!state.retry.lock().unwrap().contains_key("a"));
    }
}
