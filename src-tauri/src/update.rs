//! The uploader's own releases: whether GitHub has a newer version than the running one.
//! The Windows installer and the AppImage install it themselves (tauri-plugin-updater, signed
//! `latest.json` of the release); deb, rpm and Arch packages point to the download page on
//! foreverdb.app, which offers every package format.

use crate::addon;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

const DEFAULT_REPO: &str = "foreverdb/foreverdb-client";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Where a new version is downloaded: the platform list on the uploader page, tagged for the
/// site's analytics (the system browser opens it without a referrer).
pub const DOWNLOAD_URL: &str =
    "https://www.foreverdb.app/en/info/uploader?utm_source=foreverdb-client#download-platforms";

/// Whether this build can replace itself: the Windows installer and the AppImage can, a
/// package manager's install (deb, rpm, Arch) belongs to the package manager.
pub fn can_self_update() -> bool {
    cfg!(target_os = "windows")
        || (cfg!(target_os = "linux")
            && std::env::var_os("APPIMAGE").is_some_and(|path| !path.is_empty()))
}

/// The updater manifest attached to the release `v<version>` by the release workflow.
fn manifest_url(repo: &str, version: &str) -> String {
    format!("https://github.com/{repo}/releases/download/v{version}/latest.json")
}

static INSTALLING: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Clone)]
struct Progress {
    downloaded: u64,
    total: Option<u64>,
}

/// Downloads, verifies and installs `version`, then restarts. Progress goes to the window as
/// `uploader-update-progress`. Only one installation runs at a time.
pub async fn install(app: &AppHandle, version: &str) -> Result<(), String> {
    if !can_self_update() {
        return Err("This installation is updated by its package manager.".to_string());
    }
    if INSTALLING.swap(true, Ordering::SeqCst) {
        return Err("The update is already being installed.".to_string());
    }
    let result = download_and_install(app, version).await;
    INSTALLING.store(false, Ordering::SeqCst);
    result?;
    app.restart();
}

async fn download_and_install(app: &AppHandle, version: &str) -> Result<(), String> {
    let url = manifest_url(&repository(), version)
        .parse()
        .map_err(|e| format!("Update address invalid: {e}"))?;
    let update = app
        .updater_builder()
        .endpoints(vec![url])
        .and_then(|builder| builder.build())
        .map_err(|e| format!("Update failed: {e}"))?
        .check()
        .await
        .map_err(|e| format!("Update check failed: {e}"))?
        .ok_or_else(|| format!("v{version} has no update for this system."))?;
    let mut downloaded = 0u64;
    update
        .download_and_install(
            |chunk, total| {
                downloaded += chunk as u64;
                let _ = app.emit("uploader-update-progress", Progress { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(|e| format!("Update failed: {e}"))
}

pub fn repository() -> String {
    std::env::var("FOREVERDB_UPLOADER_REPO")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

#[derive(Serialize, Clone, Debug)]
pub struct UploaderRelease {
    pub version: String,
    pub published_at: String,
}

impl UploaderRelease {
    /// Newer than the running build.
    pub fn is_update(&self) -> bool {
        addon::is_newer(&self.version, Some(CURRENT_VERSION))
    }
}

/// The highest version among the published releases. The uploader is released as a
/// pre-release as well, which GitHub's "latest release" leaves out.
pub async fn fetch_latest_release() -> Result<UploaderRelease, String> {
    let releases = addon::fetch_github_releases(&repository(), "the uploader releases").await?;
    newest(releases.into_iter().map(|release| UploaderRelease {
        version: release.tag_name.trim_start_matches(['v', 'V']).to_string(),
        published_at: release.published_at.unwrap_or_default(),
    }))
    .ok_or_else(|| "The uploader repository has no release yet.".to_string())
}

fn newest(releases: impl Iterator<Item = UploaderRelease>) -> Option<UploaderRelease> {
    releases
        .filter_map(|release| Some((addon::parse_version(&release.version)?, release)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, release)| release)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(version: &str) -> UploaderRelease {
        UploaderRelease {
            version: version.to_string(),
            published_at: String::new(),
        }
    }

    #[test]
    fn picks_the_highest_version() {
        let found = newest(
            [
                release("0.3.0"),
                release("0.10.0"),
                release("garbage"),
                release("0.4.1"),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(found.version, "0.10.0");
        assert!(newest(std::iter::empty()).is_none());
    }

    #[test]
    fn manifest_belongs_to_the_release() {
        assert_eq!(
            manifest_url("foreverdb/foreverdb-client", "0.8.0"),
            "https://github.com/foreverdb/foreverdb-client/releases/download/v0.8.0/latest.json"
        );
    }

    #[test]
    fn only_a_higher_version_is_an_update() {
        assert!(!release(CURRENT_VERSION).is_update());
        assert!(release("999.0.0").is_update());
        assert!(!release("0.0.1").is_update());
    }
}
