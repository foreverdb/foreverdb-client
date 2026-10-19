//! The uploader's own releases: whether GitHub has a newer version than the running one.
//! The uploader does not install it; it points to the release page, which serves every
//! package format (Windows installer, AppImage, deb, rpm).

use crate::addon;
use serde::Serialize;

const DEFAULT_REPO: &str = "foreverdb/foreverdb-client";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

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
    pub html_url: String,
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
        html_url: release.html_url,
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
            html_url: format!("https://example.test/v{version}"),
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
    fn only_a_higher_version_is_an_update() {
        assert!(!release(CURRENT_VERSION).is_update());
        assert!(release("999.0.0").is_update());
        assert!(!release("0.0.1").is_update());
    }
}
