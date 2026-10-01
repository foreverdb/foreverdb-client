use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

mod addon;
mod lua;
use addon::AddonRelease;

// The user only picks a WoW client; installation directory, account, file and
// upload target are derived. A client is a flavour folder such as
// "_classic_beta_": its ".flavor.info" names the product, the "Wow*.exe" in it
// the process to watch, and the launcher's ".build.info" the installed version.

const DEFAULT_TARGET_URL: &str = "https://foreverdb-ingress.kube.alexbangert.dev/imports/forevercollect";

/// The only product the client works with: Forever runs in the "_classic_beta_" folder.
/// Classic, Classic Era and Retail are not supported yet.
const SUPPORTED_PRODUCT: &str = "wow_classic_beta";

fn is_supported(def: &ClientDef) -> bool {
    def.product == SUPPORTED_PRODUCT
}

fn unsupported_message(def: &ClientDef) -> String {
    format!(
        "'{}' is a {} client. Only Forever is supported; Classic, Classic Era and Retail are not supported yet.",
        def.id, def.label
    )
}

/// Human labels for Blizzard's product codes; unknown products show the code. The
/// other flavours stay listed so a rejection can name what the user picked.
fn product_label(product: &str) -> String {
    match product {
        "wow_classic_beta" => "Forever",
        "wow_classic_era" => "Classic Era",
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

#[derive(Serialize)]
struct ClientStatus {
    id: String,
    label: String,
    product: String,
    version: Option<String>,
    /// The file that would be uploaded: the SavedVariables file, or the game's .bak
    /// of it when the current file is an emptied database.
    file_path: Option<String>,
    file_exists: bool,
    /// The file holds catalogs; an emptied database is not worth uploading.
    file_has_data: bool,
    from_backup: bool,
    running: bool,
    /// Added by the user (as opposed to detected as installed); can be removed again.
    custom: bool,
    /// Version of the installed ForeverCollect addon, if any.
    addon_version: Option<String>,
    /// Newest release version when it is newer than the installed addon (or none is installed).
    addon_update: Option<String>,
    /// Unix time the SavedVariables file was last written (WoW writes it only on logout,
    /// exit or /reload).
    saved_at: Option<u64>,
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
#[derive(Serialize, Deserialize, Clone, Default)]
struct Settings {
    #[serde(default = "default_true")]
    auto_upload: bool,
    /// Flavour directories the user added on top of the installed clients.
    #[serde(default)]
    extra_clients: Vec<String>,
    /// WoW installation directories the user added; their installed clients are active.
    #[serde(default)]
    extra_installations: Vec<String>,
    /// Per client, the hash of the file uploaded last, so a file that could not
    /// be removed (game still running) is not sent twice.
    #[serde(default)]
    last_uploaded: HashMap<String, u64>,
    /// Per client, the Unix time of the last successful upload. The upload removes the
    /// SavedVariables file, so this stands in for its timestamp when judging crash dumps.
    #[serde(default)]
    last_upload_at: HashMap<String, u64>,
    /// GitHub token for the private addon repository (fallback after the environment
    /// and the value compiled in at build time). Never handed to the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github_token: Option<String>,
    /// Random ID created on the first start and kept from then on. Sent with every
    /// upload (`X-ForeverDB-User`) so the server can block a user and delete their data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
}

/// Creates the user ID if the settings have none yet; returns whether it did.
fn ensure_user_id(settings: &mut Settings) -> bool {
    if settings.user_id.is_some() {
        return false;
    }
    settings.user_id = Some(uuid::Uuid::new_v4().to_string());
    true
}

/// What the window may see of the settings.
#[derive(Serialize)]
struct SettingsView {
    auto_upload: bool,
    extra_clients: Vec<String>,
    extra_installations: Vec<String>,
    has_github_token: bool,
    repository: String,
}

impl Settings {
    fn view(&self) -> SettingsView {
        SettingsView {
            auto_upload: self.auto_upload,
            extra_clients: self.extra_clients.clone(),
            extra_installations: self.extra_installations.clone(),
            has_github_token: addon::github_token(self.github_token.as_deref()).is_some(),
            repository: addon::repository(),
        }
    }
}

/// Cached answer of the last release check.
#[derive(Clone)]
struct ReleaseCache {
    checked_at: std::time::Instant,
    result: Result<AddonRelease, String>,
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
    kind: String,
    message: String,
    result: Option<UploadResult>,
    import: Option<ImportStatus>,
}

struct AppState {
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    archive_dir: PathBuf,
    release: Mutex<Option<ReleaseCache>>,
}

impl AppState {
    fn load(path: PathBuf, archive_dir: PathBuf) -> Self {
        let settings = fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default();
        Self {
            settings: Mutex::new(settings),
            settings_path: path,
            archive_dir,
            release: Mutex::new(None),
        }
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

    /// The user ID sent with every upload; created here if the settings still lack one.
    fn user_id(&self) -> String {
        if let Some(id) = self.snapshot().user_id {
            return id;
        }
        self.update(|settings| {
            ensure_user_id(settings);
        })
        .user_id
        .expect("ensure_user_id sets the ID")
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

fn saved_variables_path(client_dir: &Path) -> Option<PathBuf> {
    let account_root = client_dir.join("WTF").join("Account");
    let mut accounts: Vec<PathBuf> = fs::read_dir(&account_root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().contains('#'))
        })
        .collect();
    accounts.sort();
    accounts
        .into_iter()
        .next()
        .map(|account| account.join("SavedVariables").join("ForeverCollect.lua"))
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

fn file_hash(path: &Path) -> Option<u64> {
    let content = fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    Some(hasher.finish())
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

fn snapshot_source(client_dir: &Path) -> Option<SnapshotSource> {
    let path = saved_variables_path(client_dir)?;
    if file_has_catalogs(&path) {
        return Some(SnapshotSource {
            path,
            from_backup: false,
        });
    }
    let backup = backup_path(&path);
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
    let main_path = saved_variables_path(client_dir);
    let source = snapshot_source(client_dir);
    let addon_version = addon::installed_version(client_dir);
    let addon_update = release
        .filter(|release| addon::is_newer(&release.version, addon_version.as_deref()))
        .map(|release| release.version.clone());
    let saved_at = main_path
        .as_ref()
        .and_then(|path| modified_at(path))
        .or_else(|| {
            main_path
                .as_ref()
                .and_then(|path| modified_at(&backup_path(path)))
        });
    let running_since = wow_process_started(&def.process).and_then(unix_seconds);
    // A crash only matters if it is younger than the last time the data was secured:
    // written by the game, or uploaded (which removes the file and its timestamp).
    let secured_at = saved_at.max(settings.last_upload_at.get(&def.id).copied());
    ClientStatus {
        id: def.id.clone(),
        label: def.label.clone(),
        product: def.product.clone(),
        version,
        file_exists: main_path.as_ref().is_some_and(|path| path.is_file()),
        file_has_data: source.is_some(),
        from_backup: source.as_ref().is_some_and(|source| source.from_backup),
        file_path: source
            .map(|source| source.path)
            .or(main_path)
            .map(|path| path.to_string_lossy().into_owned()),
        running: running_since.is_some(),
        custom,
        addon_version,
        addon_update,
        saved_at,
        running_since,
        crash_at: crash_after(client_dir, secured_at),
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
        // Entries from before the client was limited to Forever stay in the settings
        // but are ignored.
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
    let (wow_dirs, searched, clients) = active_clients(&state.snapshot(), release.as_ref());
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

/// Forever folders that are not active yet, from the detected installation and any
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
                has_data: saved_variables_path(&dir).is_some_and(|path| file_has_catalogs(&path)),
                version: versions.get(&def.product).cloned(),
                id: def.id,
                label: def.label,
                product: def.product,
            });
        }
    }
    candidates
}

/// Registers a folder chosen by the user: a Forever folder becomes an extra client, a
/// WoW installation folder ("World of Warcraft", holding .build.info or flavour
/// folders) an extra installation whose installed Forever client is active from then
/// on. Folders of other flavours are rejected.
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
    let forever: Vec<ClientDef> = flavours.into_iter().filter(is_supported).collect();
    if forever.is_empty() {
        return Err(format!(
            "'{}' contains no Forever client (_classic_beta_). Only Forever is supported; Classic, Classic Era and Retail are not supported yet.",
            path.display()
        ));
    }
    let root = normalize_dir(path).to_string_lossy().into_owned();
    if !settings.extra_installations.contains(&root) {
        settings.extra_installations.push(root.clone());
    }
    // Without a launcher file nothing counts as installed, so activate the flavours themselves.
    if !is_wow_dir(path) {
        for def in forever {
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
fn add_client(state: tauri::State<'_, AppState>, dir: String) -> Result<AddedFolder, String> {
    let path = PathBuf::from(dir.trim_end_matches(['/', '\\']));
    let mut outcome = Err(String::new());
    let settings = state.update(|settings| outcome = register_folder(settings, &path));
    outcome.map(|id| AddedFolder {
        id,
        settings: settings.view(),
    })
}

/// Removes an added client; when it was the last client of an added installation the
/// installation is dropped as well, and an installed client of an added installation
/// removes that installation.
#[tauri::command]
fn remove_client(state: tauri::State<'_, AppState>, id: String) -> SettingsView {
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
        .view()
}

fn client_by_id(settings: &Settings, id: &str) -> Result<ClientDef, String> {
    let (_, _, active) = active_clients(settings, None);
    if !active.iter().any(|client| client.id == id) {
        return Err(format!("Client '{id}' is not active."));
    }
    client_from_dir(Path::new(id))
        .ok_or_else(|| format!("Client folder '{id}' can no longer be read."))
}

/// Carries the user ID with every upload, so an admin can block a user and delete their data.
const USER_HEADER: &str = "X-ForeverDB-User";

/// Identifies the upload to the ingress; it ends up in `import_jobs.source`, so a
/// parser regression can be traced to a client release.
const CLIENT_TAG: &str = concat!("foreverdb-client/", env!("CARGO_PKG_VERSION"));

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
fn remember_upload(state: &AppState, client_id: &str, content_hash: u64, secured: bool) {
    state.update(|settings| {
        settings
            .last_uploaded
            .insert(client_id.to_string(), content_hash);
        if secured {
            if let Some(now) = unix_seconds(std::time::SystemTime::now()) {
                settings.last_upload_at.insert(client_id.to_string(), now);
            }
        }
    });
}

/// Uploads the client's SavedVariables (or their .bak, see `snapshot_source`), keeps an
/// archive copy, and removes the uploaded data so the next session starts empty.
async fn upload_client(state: &AppState, client_id: &str) -> Result<UploadResult, String> {
    let def = client_by_id(&state.snapshot(), client_id)?;
    let client_dir = Path::new(&def.id);
    let main_path = saved_variables_path(client_dir).ok_or_else(|| {
        format!(
            "No account folder found under '{}'.",
            client_dir.join("WTF").join("Account").display()
        )
    })?;
    let source = snapshot_source(client_dir).ok_or_else(|| {
        if main_path.is_file() {
            "The file holds no collected data yet (empty database since the last upload).".to_string()
        } else {
            format!("No ForeverCollect.lua found: {}", main_path.display())
        }
    })?;
    let content =
        fs::read(&source.path).map_err(|e| format!("Cannot read the file: {e}"))?;
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    let content_hash = hasher.finish();

    // The server only accepts JSON. If the conversion fails, the snapshot itself is
    // broken or too new: archive it, mark it as handled (otherwise the watcher retries
    // every few seconds) and leave the file in place.
    let snapshot = match lua::parse_forever_collect(&content) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            archive_snapshot(&state.archive_dir, &def.label, None, &content);
            remember_upload(state, client_id, content_hash, false);
            return Err(format!("Could not read the snapshot: {error}"));
        }
    };
    let payload = serde_json::to_vec(&snapshot)
        .map_err(|e| format!("Could not encode the snapshot as JSON: {e}"))?;
    let compressed = gzip(&payload).map_err(|e| format!("Could not prepare the upload: {e}"))?;

    let user_id = state.user_id();
    let response = reqwest::Client::new()
        .post(target_url())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::CONTENT_ENCODING, "gzip")
        .header("X-ForeverDB-Client", CLIENT_TAG)
        .header(USER_HEADER, user_id)
        .body(compressed)
        .send()
        .await
        .map_err(|e| format!("Upload failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // A permanent rejection always repeats itself for the same content.
        if is_permanent_rejection(status) {
            archive_snapshot(&state.archive_dir, &def.label, None, &content);
            remember_upload(state, client_id, content_hash, false);
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
    remember_upload(state, client_id, content_hash, true);
    archive_snapshot(
        &state.archive_dir,
        &def.label,
        import_id.as_deref(),
        &content,
    );

    let running = wow_is_running(&def.process);
    let deleted = delete_uploaded_snapshot(&source, &main_path, running);
    Ok(UploadResult {
        import_id,
        file_path: source.path.to_string_lossy().into_owned(),
        running,
        deleted,
    })
}

/// Removes an uploaded snapshot. WoW rewrites the SavedVariables on logout, so the current
/// file is only removed once the game is closed; an uploaded .bak is never read by the game
/// again. Returns whether the current file is gone.
fn delete_uploaded_snapshot(source: &SnapshotSource, main_path: &Path, running: bool) -> bool {
    if source.from_backup {
        let _ = fs::remove_file(&source.path);
        !running && !file_has_catalogs(main_path) && fs::remove_file(main_path).is_ok()
    } else {
        let removed = !running && fs::remove_file(&source.path).is_ok();
        if removed {
            let _ = fs::remove_file(backup_path(&source.path));
        }
        removed
    }
}

const ARCHIVE_LIMIT: usize = 30;

/// Keeps a copy of every uploaded snapshot so nothing is lost even if the server
/// rejects or loses an import; only the newest ARCHIVE_LIMIT copies are kept.
fn archive_snapshot(archive_dir: &Path, label: &str, import_id: Option<&str>, content: &[u8]) {
    if fs::create_dir_all(archive_dir).is_err() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!(
        "{stamp}-{}-{}.lua",
        label.replace(' ', "_"),
        import_id.unwrap_or("upload")
    );
    let _ = fs::write(archive_dir.join(name), content);
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
}

#[tauri::command]
async fn upload(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    client: String,
) -> Result<UploadResult, String> {
    let result = upload_client(&state, &client).await?;
    follow_import(&app, &client, &result);
    Ok(result)
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> SettingsView {
    state.snapshot().view()
}

#[tauri::command]
fn set_auto_upload(state: tauri::State<'_, AppState>, enabled: bool) -> SettingsView {
    state
        .update(|settings| settings.auto_upload = enabled)
        .view()
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
    let token = addon::github_token(state.snapshot().github_token.as_deref())
        .ok_or_else(|| "No GitHub token configured for addon updates.".to_string())?;
    let result = addon::fetch_latest_release(&token).await;
    *state.release.lock().unwrap() = Some(ReleaseCache {
        checked_at: std::time::Instant::now(),
        result: result.clone(),
    });
    result
}

#[tauri::command]
async fn check_addon_update(
    state: tauri::State<'_, AppState>,
    force: bool,
) -> Result<AddonRelease, String> {
    latest_release(&state, force).await
}

#[derive(Serialize)]
struct InstallResult {
    version: String,
    addon_dir: String,
    running: bool,
}

#[tauri::command]
async fn install_addon(
    state: tauri::State<'_, AppState>,
    client: String,
) -> Result<InstallResult, String> {
    let def = client_by_id(&state.snapshot(), &client)?;
    let release = latest_release(&state, false).await?;
    let token = addon::github_token(state.snapshot().github_token.as_deref())
        .ok_or_else(|| "No GitHub token configured for addon updates.".to_string())?;
    let zip_bytes = addon::download_asset(&token, &release).await?;
    let client_dir = PathBuf::from(&def.id);
    let version = release.version.clone();
    tauri::async_runtime::spawn_blocking(move || {
        addon::install_from_zip(&client_dir, &zip_bytes, &version)
    })
    .await
    .map_err(|e| format!("Installation abgebrochen: {e}"))??;
    Ok(InstallResult {
        version: release.version,
        addon_dir: addon::addon_dir(Path::new(&def.id))
            .to_string_lossy()
            .into_owned(),
        running: wow_is_running(&def.process),
    })
}

#[tauri::command]
fn set_github_token(state: tauri::State<'_, AppState>, token: Option<String>) -> SettingsView {
    let token = token
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    *state.release.lock().unwrap() = None;
    state
        .update(|settings| settings.github_token = token)
        .view()
}

/// Checks for a new addon release on start and every RELEASE_CHECK_INTERVAL and
/// reports it to the window.
async fn watch_releases(app: AppHandle) {
    let mut announced: Option<String> = None;
    loop {
        let state = app.state::<AppState>();
        match latest_release(&state, true).await {
            Ok(release) => {
                let (_, _, clients) = active_clients(&state.snapshot(), Some(&release));
                let outdated: Vec<String> = clients
                    .iter()
                    .filter(|client| client.addon_update.is_some())
                    .map(|client| client.label.clone())
                    .collect();
                if !outdated.is_empty() && announced.as_deref() != Some(&release.version) {
                    announced = Some(release.version.clone());
                    emit_activity(
                        &app,
                        "",
                        "update",
                        format!(
                            "ForeverCollect v{} is available (outdated in: {}).",
                            release.version,
                            outdated.join(", ")
                        ),
                        None,
                    );
                }
            }
            Err(error) => emit_activity(
                &app,
                "",
                "error",
                format!("Addon update check: {error}"),
                None,
            ),
        }
        tokio_sleep(RELEASE_CHECK_INTERVAL).await;
    }
}

const WATCH_INTERVAL: Duration = Duration::from_secs(3);
/// A file is only read once WoW has stopped writing it for this long.
const SETTLE_TIME: Duration = Duration::from_secs(3);

fn emit_activity(
    app: &AppHandle,
    client: &str,
    kind: &str,
    message: String,
    result: Option<UploadResult>,
) {
    let _ = app.emit(
        "activity",
        Activity {
            client: client.to_string(),
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
async fn poll_import(app: AppHandle, client_id: String, import_id: String) {
    let url = status_url(&target_url(), &import_id);
    let http = reqwest::Client::new();
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
                emit_activity(&app, &client_id, "error", format!("Cannot fetch the status of import {import_id}; please check again later."), None);
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

fn emit_import(app: &AppHandle, client: &str, import: ImportStatus) {
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
            kind: kind.to_string(),
            message,
            result: None,
            import: Some(import),
        },
    );
}

/// Starts following the import an upload produced, if the server returned one.
fn follow_import(app: &AppHandle, client_id: &str, result: &UploadResult) {
    if let Some(id) = &result.import_id {
        tauri::async_runtime::spawn(poll_import(app.clone(), client_id.to_string(), id.clone()));
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

/// Watches the SavedVariables of every active client and uploads each new write.
async fn watch_and_upload(app: AppHandle) {
    loop {
        tokio_sleep(WATCH_INTERVAL).await;
        let state = app.state::<AppState>();
        let settings = state.snapshot();
        if !settings.auto_upload {
            continue;
        }
        let (_, _, clients) = active_clients(&settings, None);
        for client in clients {
            let Some(path) = client.file_path.as_ref().map(PathBuf::from) else {
                continue;
            };
            let already_uploaded = file_hash(&path)
                .is_some_and(|hash| settings.last_uploaded.get(&client.id) == Some(&hash));
            if should_delete(already_uploaded, client.running) {
                let main_path = saved_variables_path(Path::new(&client.id));
                let source = SnapshotSource {
                    path: path.clone(),
                    from_backup: client.from_backup,
                };
                if let Some(main_path) = main_path {
                    if delete_uploaded_snapshot(&source, &main_path, false) {
                        emit_activity(
                            &app,
                            &client.id,
                            "cleaned",
                            format!(
                                "{}: uploaded data deleted after the game closed.",
                                client.label
                            ),
                            None,
                        );
                    }
                }
                continue;
            }
            if !should_upload(client.file_has_data, is_settled(&path), already_uploaded) {
                continue;
            }
            emit_activity(
                &app,
                &client.id,
                "pending",
                if client.from_backup {
                    format!("{}: found a backup with data not yet uploaded, starting upload.", client.label)
                } else {
                    format!("{}: new data written, starting upload.", client.label)
                },
                None,
            );
            match upload_client(&state, &client.id).await {
                Ok(result) => {
                    follow_import(&app, &client.id, &result);
                    emit_activity(
                        &app,
                        &client.id,
                        "uploaded",
                        match &result.import_id {
                            Some(id) => format!("Upload succeeded (import ID {id})."),
                            None => "Upload succeeded.".to_string(),
                        },
                        Some(result),
                    )
                }
                Err(error) => emit_activity(&app, &client.id, "error", error, None),
            }
        }
    }
}

async fn tokio_sleep(duration: Duration) {
    tauri::async_runtime::spawn_blocking(move || std::thread::sleep(duration))
        .await
        .ok();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let settings_path = app
                .path()
                .app_config_dir()
                .map(|dir| dir.join("settings.json"))
                .unwrap_or_else(|_| PathBuf::from("foreverdb-client-settings.json"));
            let archive_dir = app
                .path()
                .app_data_dir()
                .map(|dir| dir.join("archive"))
                .unwrap_or_else(|_| PathBuf::from("foreverdb-client-archive"));
            let state = AppState::load(settings_path, archive_dir);
            if state.snapshot().user_id.is_none() {
                state.update(|settings| {
                    ensure_user_id(settings);
                });
            }
            app.manage(state);
            tauri::async_runtime::spawn(watch_and_upload(app.handle().clone()));
            tauri::async_runtime::spawn(watch_releases(app.handle().clone()));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            detect_installation,
            list_client_candidates,
            add_client,
            remove_client,
            upload,
            get_settings,
            set_auto_upload,
            check_addon_update,
            install_addon,
            set_github_token
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
            std::env::temp_dir().join(format!("foreverdb-client-{name}-{}", std::process::id()));
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

        // a Forever folder becomes an extra client
        let mut settings = Settings::default();
        register_folder(&mut settings, &beta).unwrap();
        assert_eq!(
            settings.extra_clients,
            vec![beta.to_string_lossy().into_owned()]
        );
        assert!(settings.extra_installations.is_empty());

        // other flavours are rejected
        let mut settings = Settings::default();
        let error = register_folder(&mut settings, &era).unwrap_err();
        assert!(error.contains("Only Forever is supported"), "{error}");
        assert!(settings.extra_clients.is_empty());

        // an installation without launcher file activates only its Forever folder
        let mut settings = Settings::default();
        register_folder(&mut settings, &root).unwrap();
        assert_eq!(
            settings.extra_installations,
            vec![root.to_string_lossy().into_owned()]
        );
        assert_eq!(
            settings.extra_clients,
            vec![beta.to_string_lossy().into_owned()]
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
        let era = fake_flavour(&root, "_classic_era_", "wow_classic_era", "WowClassic.exe");
        fs::write(
            root.join(".build.info"),
            "Version!STRING:0|Product!STRING:0\n12.1.0.69814|wow\n1.15.7.61582|wow_classic_era\n",
        )
        .unwrap();

        // an installation with only other flavours cannot be added
        let error = register_folder(&mut Settings::default(), &root).unwrap_err();
        assert!(error.contains("no Forever client"), "{error}");

        // installed or previously added clients of other flavours are ignored
        let settings = Settings {
            extra_clients: vec![
                retail.to_string_lossy().into_owned(),
                era.to_string_lossy().into_owned(),
            ],
            ..Settings::default()
        };
        let (dirs, _, clients) = clients_among(&settings, vec![root.clone()], None);
        assert_eq!(dirs, vec![root.clone()]);
        assert!(clients.is_empty(), "{:?}", clients.iter().map(|c| &c.id).collect::<Vec<_>>());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn creates_the_user_id_once() {
        let mut settings = Settings::default();
        assert!(ensure_user_id(&mut settings));
        let id = settings.user_id.clone().unwrap();
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
        assert!(!ensure_user_id(&mut settings));
        assert_eq!(settings.user_id.as_deref(), Some(id.as_str()));

        let stored: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(stored.user_id.as_deref(), Some(id.as_str()));
        let legacy: Settings = serde_json::from_str(r#"{"auto_upload":true}"#).unwrap();
        assert!(legacy.user_id.is_none());
    }

    #[test]
    fn detects_empty_snapshots() {
        let empty = b"\nForeverCollectDB = {\n[\"catalogs\"] = {\n},\n[\"schemaVersion\"] = 9,\n[\"settings\"] = {\n},\n}\n";
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

        fs::write(&main, "x").unwrap();
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
        assert!(!main.exists() && !backup.exists());

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

        assert!(snapshot_source(&root).is_none(), "no files");
        fs::write(&main, empty).unwrap();
        assert!(snapshot_source(&root).is_none(), "empty database only");
        fs::write(&backup, filled).unwrap();
        let source = snapshot_source(&root).unwrap();
        assert!(
            source.from_backup && source.path == backup,
            "backup holds the last session"
        );
        fs::write(&main, filled).unwrap();
        let source = snapshot_source(&root).unwrap();
        assert!(
            !source.from_backup && source.path == main,
            "current file wins when it has data"
        );
        fs::remove_dir_all(&root).unwrap();
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
                "{} [{}] {:?} file={:?} exists={} running={} custom={}",
                client.label,
                client.product,
                client.version,
                client.file_path,
                client.file_exists,
                client.running,
                client.custom
            );
        }
        if !wow_dirs.is_empty() {
            assert!(
                !clients.is_empty(),
                "an installation should expose at least one client"
            );
        }
    }
}
