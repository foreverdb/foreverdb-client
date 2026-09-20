use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

mod addon;
use addon::AddonRelease;

// The user only picks a WoW client; installation directory, account, file and
// upload target are derived. A client is a flavour folder such as
// "_classic_beta_": its ".flavor.info" names the product, the "Wow*.exe" in it
// the process to watch, and the launcher's ".build.info" the installed version.

const DEFAULT_TARGET_URL: &str = "https://foreverdb.docker.alexbangert.dev/imports/forevercollect";

/// Human labels for Blizzard's product codes; unknown products show the code.
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
    /// GitHub token for the private addon repository (fallback after the environment
    /// and the value compiled in at build time). Never handed to the window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github_token: Option<String>,
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

/// Progress of the background watcher, sent to the window as "activity" events.
#[derive(Serialize, Clone)]
struct Activity {
    client: String,
    kind: String,
    message: String,
    result: Option<UploadResult>,
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
        crash_at: crash_after(client_dir, saved_at),
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
            let Some(def) = client_from_dir(&dir) else {
                continue;
            };
            let Some(version) = versions.get(&def.product).cloned() else {
                continue;
            };
            if seen.insert(def.id.clone()) {
                clients.push(client_status(
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
        let Some(def) = client_from_dir(Path::new(extra)) else {
            continue;
        };
        let version = client_version(&def);
        seen.insert(def.id.clone());
        clients.push(client_status(&def, version, true, release));
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

/// Flavour folders that are not active yet: the other flavours of the detected
/// installation and of any other installation found in the candidate directories.
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
            let Some(def) = client_from_dir(&dir) else {
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

/// Adds a folder chosen by the user: a flavour folder becomes an extra client, a WoW
/// installation folder ("World of Warcraft", holding .build.info or flavour folders)
/// becomes an extra installation whose installed clients are active from then on.
/// Registers a folder chosen by the user: a flavour folder becomes an extra client, a
/// WoW installation folder ("World of Warcraft", holding .build.info or flavour
/// folders) an extra installation whose installed clients are active from then on.
fn register_folder(settings: &mut Settings, path: &Path) -> Result<String, String> {
    if let Some(def) = client_from_dir(path) {
        if !settings.extra_clients.contains(&def.id) {
            settings.extra_clients.push(def.id.clone());
        }
        return Ok(def.id);
    }
    let flavours: Vec<PathBuf> = flavour_dirs(path)
        .into_iter()
        .filter(|dir| client_from_dir(dir).is_some())
        .collect();
    if !is_wow_dir(path) && flavours.is_empty() {
        return Err(format!(
            "'{}' ist weder ein WoW-Installationsordner (mit .build.info oder Ordnern wie _classic_era_) noch ein Client-Ordner (mit .flavor.info und Wow*.exe).",
            path.display()
        ));
    }
    let root = normalize_dir(path).to_string_lossy().into_owned();
    if !settings.extra_installations.contains(&root) {
        settings.extra_installations.push(root.clone());
    }
    // Without a launcher file nothing counts as installed, so activate the flavours themselves.
    if !is_wow_dir(path) {
        for def in flavours.iter().filter_map(|dir| client_from_dir(dir)) {
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
        return Err(format!("Client '{id}' ist nicht aktiv."));
    }
    client_from_dir(Path::new(id))
        .ok_or_else(|| format!("Client-Ordner '{id}' ist nicht mehr lesbar."))
}

/// Uploads the client's SavedVariables (or their .bak, see `snapshot_source`), keeps an
/// archive copy, and removes the uploaded data so the next session starts empty.
async fn upload_client(state: &AppState, client_id: &str) -> Result<UploadResult, String> {
    let def = client_by_id(&state.snapshot(), client_id)?;
    let client_dir = Path::new(&def.id);
    let main_path = saved_variables_path(client_dir).ok_or_else(|| {
        format!(
            "Kein Account-Verzeichnis unter '{}' gefunden.",
            client_dir.join("WTF").join("Account").display()
        )
    })?;
    let source = snapshot_source(client_dir).ok_or_else(|| {
        if main_path.is_file() {
            "Die Datei enthält noch keine gesammelten Daten (leere Datenbank nach dem letzten Upload).".to_string()
        } else {
            format!("Keine ForeverCollect.lua gefunden: {}", main_path.display())
        }
    })?;
    let content =
        fs::read(&source.path).map_err(|e| format!("Datei kann nicht gelesen werden: {e}"))?;
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    let content_hash = hasher.finish();
    let form = Form::new().part(
        "file",
        Part::bytes(content.clone())
            .file_name("ForeverCollect.lua")
            .mime_str("text/plain")
            .map_err(|e| format!("Upload konnte nicht vorbereitet werden: {e}"))?,
    );
    let response = reqwest::Client::new()
        .post(target_url())
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("Upload fehlgeschlagen: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Server lehnte den Upload ab ({}): {}",
            status,
            body.trim()
        ));
    }
    let import_id = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| json.get("id").and_then(|id| id.as_str()).map(str::to_owned));
    state.update(|settings| {
        settings
            .last_uploaded
            .insert(client_id.to_string(), content_hash);
    });
    archive_snapshot(
        &state.archive_dir,
        &def.label,
        import_id.as_deref(),
        &content,
    );

    // WoW rewrites the SavedVariables on logout, so the current file is only removed
    // once the game is closed; an uploaded .bak is never read by the game again.
    let running = wow_is_running(&def.process);
    let deleted = if source.from_backup {
        let _ = fs::remove_file(&source.path);
        !running && !file_has_catalogs(&main_path) && fs::remove_file(&main_path).is_ok()
    } else {
        let removed = !running && fs::remove_file(&source.path).is_ok();
        if removed {
            let _ = fs::remove_file(backup_path(&source.path));
        }
        removed
    };
    Ok(UploadResult {
        import_id,
        file_path: source.path.to_string_lossy().into_owned(),
        running,
        deleted,
    })
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
async fn upload(state: tauri::State<'_, AppState>, client: String) -> Result<UploadResult, String> {
    upload_client(&state, &client).await
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
        .ok_or_else(|| "Kein GitHub-Token für Addon-Updates hinterlegt.".to_string())?;
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
        .ok_or_else(|| "Kein GitHub-Token für Addon-Updates hinterlegt.".to_string())?;
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
                            "ForeverCollect v{} verfügbar (installiert: {}).",
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
                format!("Addon-Update-Prüfung: {error}"),
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
        },
    );
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
            if !should_upload(client.file_has_data, is_settled(&path), already_uploaded) {
                continue;
            }
            emit_activity(
                &app,
                &client.id,
                "pending",
                if client.from_backup {
                    format!("{}: Sicherungskopie mit nicht hochgeladenen Daten gefunden, Upload startet.", client.label)
                } else {
                    format!("{}: neue Daten geschrieben, Upload startet.", client.label)
                },
                None,
            );
            match upload_client(&state, &client.id).await {
                Ok(result) => emit_activity(
                    &app,
                    &client.id,
                    "uploaded",
                    match &result.import_id {
                        Some(id) => format!("Upload erfolgreich (Import-ID {id})."),
                        None => "Upload erfolgreich.".to_string(),
                    },
                    Some(result),
                ),
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
            app.manage(AppState::load(settings_path, archive_dir));
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

        // a flavour folder becomes an extra client
        let mut settings = Settings::default();
        register_folder(&mut settings, &era).unwrap();
        assert_eq!(
            settings.extra_clients,
            vec![era.to_string_lossy().into_owned()]
        );
        assert!(settings.extra_installations.is_empty());

        // an installation without launcher file activates all its flavours
        let mut settings = Settings::default();
        register_folder(&mut settings, &root).unwrap();
        assert_eq!(
            settings.extra_installations,
            vec![root.to_string_lossy().into_owned()]
        );
        assert_eq!(settings.extra_clients.len(), 2);

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
    fn detects_empty_snapshots() {
        let empty = b"\nForeverCollectDB = {\n[\"catalogs\"] = {\n},\n[\"schemaVersion\"] = 9,\n[\"settings\"] = {\n},\n}\n";
        assert!(!snapshot_has_catalogs(empty));
        let filled = b"ForeverCollectDB = {\n[\"catalogs\"] = {\n[\"1:16001:0:enUS:9:7:Alliance\"] = {\n[\"scannedAt\"] = 1,\n},\n},\n}\n";
        assert!(snapshot_has_catalogs(filled));
        assert!(!snapshot_has_catalogs(b"garbage"));
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
