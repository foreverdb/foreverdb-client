//! ForeverCollect addon releases: reads the installed version from the client's
//! AddOns folder, asks GitHub for the latest release and installs its zip.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const ADDON_FOLDER: &str = "ForeverCollect";
const DEFAULT_REPO: &str = "foreverdb/forevercollect-addon";
const USER_AGENT: &str = "foreverdb-client";

/// Token precedence: runtime environment, value compiled in at build time
/// (`FOREVERDB_GITHUB_TOKEN=... pnpm tauri build` or `src-tauri/github-token`),
/// stored settings.
pub fn github_token(stored: Option<&str>) -> Option<String> {
    std::env::var("FOREVERDB_GITHUB_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| option_env!("FOREVERDB_GITHUB_TOKEN").map(str::to_owned))
        .or_else(|| stored.map(str::to_owned))
        .filter(|value| !value.trim().is_empty())
}

pub fn repository() -> String {
    std::env::var("FOREVERDB_ADDON_REPO")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

pub fn addon_dir(client_dir: &Path) -> PathBuf {
    client_dir
        .join("Interface")
        .join("AddOns")
        .join(ADDON_FOLDER)
}

/// `## Version:` of a TOC file.
pub fn toc_version(toc: &str) -> Option<String> {
    toc.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("## Version:"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn installed_version(client_dir: &Path) -> Option<String> {
    let toc = fs::read_to_string(addon_dir(client_dir).join("ForeverCollect.toc")).ok()?;
    toc_version(&toc)
}

/// "v0.1.2" / "0.1.2" -> (0, 1, 2); missing parts count as 0.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('.')
        .map(|part| part.trim().parse::<u32>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

pub fn is_newer(candidate: &str, installed: Option<&str>) -> bool {
    match (parse_version(candidate), installed.and_then(parse_version)) {
        (Some(new), Some(current)) => new > current,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct AddonRelease {
    pub version: String,
    pub name: String,
    pub notes: String,
    pub published_at: String,
    pub html_url: String,
    pub asset_name: String,
    pub asset_size: u64,
    pub asset_url: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    html_url: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
    url: String,
}

fn github_client(token: &str) -> Result<reqwest::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    let auth = format!("Bearer {}", token.trim());
    headers.insert(
        reqwest::header::AUTHORIZATION,
        auth.parse()
            .map_err(|_| "Token enthält ungültige Zeichen.".to_string())?,
    );
    headers.insert("X-GitHub-Api-Version", "2022-11-28".parse().unwrap());
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .build()
        .map_err(|e| format!("HTTP-Client konnte nicht erstellt werden: {e}"))
}

fn explain_status(status: reqwest::StatusCode, what: &str) -> String {
    match status.as_u16() {
        401 => "GitHub-Token ungültig oder abgelaufen.".to_string(),
        403 => "GitHub-Token hat keinen Zugriff auf das Addon-Repository (Berechtigung Contents: Read nötig).".to_string(),
        404 => format!("{what} nicht gefunden (Repository oder Release fehlt, oder das Token hat keinen Zugriff)."),
        _ => format!("GitHub antwortete mit {status} für {what}."),
    }
}

pub async fn fetch_latest_release(token: &str) -> Result<AddonRelease, String> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        repository()
    );
    let response = github_client(token)?
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("Release-Abfrage fehlgeschlagen: {e}"))?;
    if !response.status().is_success() {
        return Err(explain_status(response.status(), "das neueste Release"));
    }
    let release: GithubRelease = response
        .json()
        .await
        .map_err(|e| format!("Release-Antwort unlesbar: {e}"))?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name.starts_with(ADDON_FOLDER) && asset.name.ends_with(".zip"))
        .ok_or_else(|| {
            format!(
                "Release {} enthält kein {ADDON_FOLDER}-Zip.",
                release.tag_name
            )
        })?;
    Ok(AddonRelease {
        version: release.tag_name.trim_start_matches(['v', 'V']).to_string(),
        name: release
            .name
            .clone()
            .unwrap_or_else(|| release.tag_name.clone()),
        notes: release.body.clone().unwrap_or_default(),
        published_at: release.published_at.clone().unwrap_or_default(),
        html_url: release.html_url.clone(),
        asset_name: asset.name.clone(),
        asset_size: asset.size,
        asset_url: asset.url.clone(),
    })
}

pub async fn download_asset(token: &str, release: &AddonRelease) -> Result<Vec<u8>, String> {
    let response = github_client(token)?
        .get(&release.asset_url)
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .send()
        .await
        .map_err(|e| format!("Download fehlgeschlagen: {e}"))?;
    if !response.status().is_success() {
        return Err(explain_status(response.status(), "das Addon-Zip"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Download abgebrochen: {e}"))?;
    if release.asset_size > 0 && bytes.len() as u64 != release.asset_size {
        return Err(format!(
            "Download unvollständig ({} von {} Bytes).",
            bytes.len(),
            release.asset_size
        ));
    }
    Ok(bytes.to_vec())
}

/// Unpacks the release zip into `target` (which must not exist yet). The zip has
/// to contain exactly the `ForeverCollect/` folder with a TOC of the expected
/// version; anything else is rejected before touching the client.
pub fn extract_addon(
    zip_bytes: &[u8],
    target: &Path,
    expected_version: &str,
) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(zip_bytes)).map_err(|e| format!("Zip unlesbar: {e}"))?;
    let prefix = format!("{ADDON_FOLDER}/");
    let mut saw_toc = false;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("Zip-Eintrag unlesbar: {e}"))?;
        let name = entry.name().to_string();
        let Some(relative) = name.strip_prefix(&prefix) else {
            return Err(format!(
                "Unerwarteter Zip-Eintrag außerhalb von {prefix}: {name}"
            ));
        };
        if relative.is_empty() || entry.is_dir() {
            continue;
        }
        let relative_path = Path::new(relative);
        if relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!("Unsicherer Pfad im Zip: {name}"));
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut content)
            .map_err(|e| format!("Zip-Eintrag {name} unlesbar: {e}"))?;
        if relative == "ForeverCollect.toc" {
            let found = toc_version(&String::from_utf8_lossy(&content));
            if found.as_deref() != Some(expected_version) {
                return Err(format!(
                    "Zip enthält Version {}, erwartet wurde {expected_version}.",
                    found.unwrap_or_else(|| "?".to_string())
                ));
            }
            saw_toc = true;
        }
        let destination = target.join(relative_path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Ordner {} nicht anlegbar: {e}", parent.display()))?;
        }
        fs::write(&destination, content)
            .map_err(|e| format!("Datei {} nicht schreibbar: {e}", destination.display()))?;
    }
    if !saw_toc {
        return Err("Zip enthält keine ForeverCollect.toc.".to_string());
    }
    Ok(())
}

const RENAME_ATTEMPTS: u32 = 5;
const RENAME_RETRY_DELAY: Duration = Duration::from_millis(200);

/// `fs::rename` that retries briefly: on Windows a folder cannot be moved while a
/// virus scanner or the Explorer still holds one of its files.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Err(error) if attempt < RENAME_ATTEMPTS && error.raw_os_error().is_some() => {
                attempt += 1;
                std::thread::sleep(RENAME_RETRY_DELAY);
            }
            result => return result,
        }
    }
}

/// Replaces the installed addon with the unpacked zip; the previous version is
/// kept as `ForeverCollect.old` until the new one is in place.
pub fn install_from_zip(
    client_dir: &Path,
    zip_bytes: &[u8],
    expected_version: &str,
) -> Result<(), String> {
    let addons = client_dir.join("Interface").join("AddOns");
    fs::create_dir_all(&addons).map_err(|e| format!("AddOns-Ordner nicht anlegbar: {e}"))?;
    let target = addon_dir(client_dir);
    let staging = addons.join(format!("{ADDON_FOLDER}.new"));
    let backup = addons.join(format!("{ADDON_FOLDER}.old"));
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_dir_all(&backup);

    if let Err(error) = extract_addon(zip_bytes, &staging, expected_version) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    let had_previous = target.is_dir();
    if had_previous {
        rename_with_retry(&target, &backup)
            .map_err(|e| format!("Bisheriges Addon nicht verschiebbar: {e}"))?;
    }
    if let Err(error) = rename_with_retry(&staging, &target) {
        if had_previous {
            let _ = rename_with_retry(&backup, &target);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("Neues Addon nicht installierbar: {error}"));
    }
    let _ = fs::remove_dir_all(&backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default();
            for (name, content) in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(content.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("foreverdb-addon-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        crate::normalize_dir(&dir)
    }

    #[test]
    fn parses_and_compares_versions() {
        assert_eq!(parse_version("v0.1.2"), Some((0, 1, 2)));
        assert_eq!(parse_version("1.60"), Some((1, 60, 0)));
        assert_eq!(parse_version("abc"), None);
        assert!(is_newer("0.1.2", Some("0.1.1")));
        assert!(!is_newer("0.1.2", Some("0.1.2")));
        assert!(!is_newer("0.1.2", Some("0.2.0")));
        assert!(is_newer("0.1.2", None), "missing addon counts as update");
        assert!(!is_newer("nope", None));
        assert_eq!(
            toc_version("## Interface: 16001\n## Version: 0.1.2\n"),
            Some("0.1.2".to_string())
        );
    }

    #[test]
    fn installs_and_replaces_addon() {
        let client = temp_dir("install");
        let old_dir = addon_dir(&client);
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("ForeverCollect.toc"), "## Version: 0.1.1\n").unwrap();
        fs::write(old_dir.join("stale.lua"), "").unwrap();
        assert_eq!(installed_version(&client).as_deref(), Some("0.1.1"));

        let good = make_zip(&[
            ("ForeverCollect/", ""),
            ("ForeverCollect/ForeverCollect.toc", "## Version: 0.1.2\n"),
            ("ForeverCollect/Core/Util.lua", "-- lua"),
        ]);
        install_from_zip(&client, &good, "0.1.2").unwrap();
        assert_eq!(installed_version(&client).as_deref(), Some("0.1.2"));
        assert!(old_dir.join("Core/Util.lua").is_file());
        assert!(!old_dir.join("stale.lua").exists(), "old files are gone");
        assert!(!client
            .join("Interface")
            .join("AddOns")
            .join("ForeverCollect.old")
            .exists());

        let wrong_version =
            make_zip(&[("ForeverCollect/ForeverCollect.toc", "## Version: 0.1.3\n")]);
        assert!(install_from_zip(&client, &wrong_version, "0.1.4").is_err());
        let escaping = make_zip(&[
            ("ForeverCollect/ForeverCollect.toc", "## Version: 0.1.4\n"),
            ("ForeverCollect/../evil.lua", ""),
        ]);
        assert!(install_from_zip(&client, &escaping, "0.1.4").is_err());
        let foreign = make_zip(&[("Other/ForeverCollect.toc", "## Version: 0.1.4\n")]);
        assert!(install_from_zip(&client, &foreign, "0.1.4").is_err());
        assert_eq!(
            installed_version(&client).as_deref(),
            Some("0.1.2"),
            "failed installs keep the current addon"
        );
        fs::remove_dir_all(&client).unwrap();
    }
}
