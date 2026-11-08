//! Which published release is an update for this build, and which of its
//! archives this platform installs.
//!
//! Every name is built here from the version, never taken from GitHub's
//! answer: the archive's file name is the context its signature is bound
//! to, so an older signed archive cannot be passed off as a newer one.
use self_update::version::{bump_is_greater, cmp_versions};

/// The repository whose releases are read.
pub const OWNER: &str = "alandariva";
pub const REPOSITORY: &str = "jykre-studio";

/// A platform the release workflow builds an archive for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    LinuxX86_64,
    WindowsX86_64,
}

impl Platform {
    /// The platform this build runs on, when releases include it.
    pub fn current() -> Option<Self> {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Self::LinuxX86_64)
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some(Self::WindowsX86_64)
        } else {
            None
        }
    }

    fn os(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux",
            Self::WindowsX86_64 => "windows",
        }
    }

    /// The archive's extension.
    pub fn extension(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "tar.gz",
            Self::WindowsX86_64 => "zip",
        }
    }

    /// The executable's file name inside the archive's folder.
    pub fn executable(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "jykre",
            Self::WindowsX86_64 => "jykre.exe",
        }
    }
}

/// The archive of `version` for `platform`, as the release workflow names
/// it: `jykre-studio-v0.2.1-linux-x86_64.tar.gz`.
pub fn asset_name(version: &str, platform: Platform) -> String {
    format!(
        "jykre-studio-v{version}-{}-x86_64.{}",
        platform.os(),
        platform.extension()
    )
}

/// The release's page on GitHub, for its notes and a manual download.
pub fn release_page(version: &str) -> String {
    format!("https://github.com/{OWNER}/{REPOSITORY}/releases/tag/v{version}")
}

/// An archive of a release: its name and the API address it downloads from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetRef {
    pub name: String,
    pub url: String,
}

/// One release of the list, as GitHub described it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseEntry {
    /// Without the tag's leading `v`.
    pub version: String,
    pub assets: Vec<AssetRef>,
}

impl ReleaseEntry {
    pub fn from_release(release: &self_update::update::Release) -> Self {
        Self {
            version: release.version().trim_start_matches('v').to_owned(),
            assets: release
                .assets()
                .iter()
                .map(|asset| AssetRef {
                    name: asset.name().to_owned(),
                    url: asset.download_url().to_owned(),
                })
                .collect(),
        }
    }
}

/// A newer version than the running one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub version: String,
    /// The release's page ([`release_page`]).
    pub page: String,
    /// This platform's archive, when the release has one.
    pub asset: Option<AssetRef>,
}

/// The highest version above `current` among `releases`, pre-releases
/// included, with this platform's archive when there is one. A version
/// that is not semantic, or holds characters a file name or an address
/// should not, is ignored.
pub fn select_update(
    current: &str,
    releases: &[ReleaseEntry],
    platform: Option<Platform>,
) -> Option<Candidate> {
    let newest = releases
        .iter()
        .filter(|release| plain_version(&release.version))
        .filter(|release| bump_is_greater(current, &release.version).unwrap_or(false))
        .max_by(|a, b| cmp_versions(&a.version, &b.version).unwrap_or(std::cmp::Ordering::Equal))?;
    let asset = platform.and_then(|platform| {
        let name = asset_name(&newest.version, platform);
        newest
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .cloned()
    });
    Some(Candidate {
        version: newest.version.clone(),
        page: release_page(&newest.version),
        asset,
    })
}

/// Only what a semantic version may hold (digits, letters, `.`, `-`), so a
/// version can go into a file name and an address as it is. Build metadata
/// (`+…`) is refused: releases never carry it.
fn plain_version(version: &str) -> bool {
    !version.is_empty()
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && cmp_versions(version, version).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(version: &str, assets: &[&str]) -> ReleaseEntry {
        ReleaseEntry {
            version: version.to_owned(),
            assets: assets
                .iter()
                .map(|name| AssetRef {
                    name: (*name).to_owned(),
                    url: format!("https://api.github.com/x/{name}"),
                })
                .collect(),
        }
    }

    fn versions(current: &str, list: &[&str]) -> Option<String> {
        let releases: Vec<_> = list.iter().map(|v| entry(v, &[])).collect();
        select_update(current, &releases, None).map(|candidate| candidate.version)
    }

    #[test]
    fn names_follow_the_release_workflow() {
        assert_eq!(
            asset_name("0.2.0", Platform::LinuxX86_64),
            "jykre-studio-v0.2.0-linux-x86_64.tar.gz"
        );
        assert_eq!(
            asset_name("0.2.0-rc.1", Platform::WindowsX86_64),
            "jykre-studio-v0.2.0-rc.1-windows-x86_64.zip"
        );
        assert_eq!(
            release_page("0.2.1"),
            "https://github.com/alandariva/jykre-studio/releases/tag/v0.2.1"
        );
    }

    #[test]
    fn a_pre_release_above_the_running_version_is_an_update() {
        assert_eq!(
            versions("0.1.0", &["0.1.0", "0.2.0-rc.1"]).as_deref(),
            Some("0.2.0-rc.1")
        );
    }

    #[test]
    fn the_highest_version_wins_in_any_order() {
        assert_eq!(
            versions("0.1.0", &["0.2.0-rc.1", "0.2.0", "0.1.5"]).as_deref(),
            Some("0.2.0")
        );
        assert_eq!(
            versions("0.1.0", &["0.3.0-beta.2", "0.3.0-beta.10"]).as_deref(),
            Some("0.3.0-beta.10")
        );
    }

    #[test]
    fn equal_older_and_odd_versions_are_not_updates() {
        assert_eq!(versions("0.2.0", &["0.2.0", "0.1.0", "0.2.0-rc.2"]), None);
        assert_eq!(versions("0.2.0", &[]), None);
        assert_eq!(
            versions("0.2.0", &["nightly", "0.3.0+build.1", "0.3.0/../x"]),
            None
        );
    }

    #[test]
    fn the_archive_is_matched_by_its_exact_name() {
        let name = "jykre-studio-v0.2.1-linux-x86_64.tar.gz";
        let releases = [entry(
            "0.2.1",
            &[
                "jykre-studio-v0.2.1-linux-x86_64.tar.gz.sha256",
                name,
                "jykre-studio-v0.2.1-windows-x86_64.zip",
            ],
        )];
        let candidate = select_update("0.2.0", &releases, Some(Platform::LinuxX86_64));
        assert_eq!(
            candidate.and_then(|c| c.asset).map(|a| a.name).as_deref(),
            Some(name)
        );
    }

    #[test]
    fn a_release_without_this_platform_is_offered_without_an_archive() {
        let releases = [entry("0.2.1", &["jykre-studio-v0.2.1-windows-x86_64.zip"])];
        let candidate = select_update("0.2.0", &releases, Some(Platform::LinuxX86_64));
        assert_eq!(
            candidate,
            Some(Candidate {
                version: "0.2.1".to_owned(),
                page: release_page("0.2.1"),
                asset: None,
            })
        );
    }
}
