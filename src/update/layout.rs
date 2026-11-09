//! Whether this copy of Jykre Studio can replace itself: a release build,
//! in the folder the release workflow ships, that the user can write to.
//! Anything else (a development build, a package another tool installed, a
//! read-only folder) is offered the release's page instead.
use std::path::{Path, PathBuf};

use super::release::Platform;
use super::swap::WORK_DIR;

/// The folder an update replaces entries of, and the executable in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallDir {
    pub dir: PathBuf,
    pub exe: PathBuf,
}

/// Why an update is offered as a download only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManualReason {
    /// A development build (`cargo run`) never replaces itself.
    DevelopmentBuild,
    /// Releases have no archive for this platform.
    Platform,
    /// The executable is not in a folder as the release ships it.
    NotReleaseLayout,
    /// The folder cannot be written to.
    NotWritable,
    /// The release has no archive for this platform.
    NoAsset,
}

/// Whether the update can be installed here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installability {
    SelfInstall(InstallDir),
    ManualOnly(ManualReason),
}

/// Decides for the executable `exe`. Touches the disk: it creates and
/// removes a file in `<folder>/.jykre-update/` to learn whether the folder
/// can be written to, so it runs off the UI thread.
pub fn detect(
    exe: &Path,
    platform: Option<Platform>,
    has_asset: bool,
    release_build: bool,
) -> Installability {
    use Installability::ManualOnly;
    if !release_build {
        return ManualOnly(ManualReason::DevelopmentBuild);
    }
    let Some(platform) = platform else {
        return ManualOnly(ManualReason::Platform);
    };
    if !has_asset {
        return ManualOnly(ManualReason::NoAsset);
    }
    let Some(dir) = exe.parent() else {
        return ManualOnly(ManualReason::NotReleaseLayout);
    };
    let named_as_released = exe
        .file_name()
        .is_some_and(|name| name == platform.executable());
    let catalogs = dir.join("catalog").join("postgresql").join("manifest.json");
    if !named_as_released || !catalogs.is_file() {
        return ManualOnly(ManualReason::NotReleaseLayout);
    }
    if !writable(dir) {
        return ManualOnly(ManualReason::NotWritable);
    }
    Installability::SelfInstall(InstallDir {
        dir: dir.to_path_buf(),
        exe: exe.to_path_buf(),
    })
}

/// Whether a file can be created in `dir`'s work folder (and the folder
/// itself, when it is missing). Leaves the folder as it found it.
fn writable(dir: &Path) -> bool {
    let work = dir.join(WORK_DIR);
    let created = !work.exists();
    if std::fs::create_dir_all(&work).is_err() {
        return false;
    }
    let probe = work.join(format!("probe-{}", uuid::Uuid::new_v4().simple()));
    let written = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    if created {
        let _ = std::fs::remove_dir(&work);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::tests::TempDir;

    fn release_folder(root: &Path, platform: Platform) -> PathBuf {
        let catalog = root.join("catalog").join("postgresql");
        std::fs::create_dir_all(&catalog).unwrap();
        std::fs::write(catalog.join("manifest.json"), b"{}").unwrap();
        let exe = root.join(platform.executable());
        std::fs::write(&exe, b"binary").unwrap();
        exe
    }

    #[test]
    fn a_release_folder_installs_itself() {
        let temp = TempDir::new();
        let platform = Platform::LinuxX86_64;
        let exe = release_folder(temp.path(), platform);
        assert_eq!(
            detect(&exe, Some(platform), true, true),
            Installability::SelfInstall(InstallDir {
                dir: temp.path().to_path_buf(),
                exe: exe.clone(),
            })
        );
        assert!(
            !temp.path().join(WORK_DIR).exists(),
            "the probe leaves nothing behind"
        );
    }

    #[test]
    fn other_copies_are_offered_a_download() {
        let temp = TempDir::new();
        let platform = Platform::LinuxX86_64;
        let exe = release_folder(temp.path(), platform);
        assert_eq!(
            detect(&exe, Some(platform), true, false),
            Installability::ManualOnly(ManualReason::DevelopmentBuild)
        );
        assert_eq!(
            detect(&exe, None, true, true),
            Installability::ManualOnly(ManualReason::Platform)
        );
        assert_eq!(
            detect(&exe, Some(platform), false, true),
            Installability::ManualOnly(ManualReason::NoAsset)
        );
        let renamed = temp.path().join("jykre-dev");
        assert_eq!(
            detect(&renamed, Some(platform), true, true),
            Installability::ManualOnly(ManualReason::NotReleaseLayout)
        );
        std::fs::remove_dir_all(temp.path().join("catalog")).unwrap();
        assert_eq!(
            detect(&exe, Some(platform), true, true),
            Installability::ManualOnly(ManualReason::NotReleaseLayout)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_folder_is_offered_a_download() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new();
        let platform = Platform::LinuxX86_64;
        let exe = release_folder(temp.path(), platform);
        let mode = |mode| {
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        };
        mode(0o555);
        // Root writes anywhere, so the check proves nothing there.
        let probe = temp.path().join("root-probe");
        let is_root = std::fs::write(&probe, b"").is_ok();
        let result = detect(&exe, Some(platform), true, true);
        mode(0o755);
        if !is_root {
            assert_eq!(
                result,
                Installability::ManualOnly(ManualReason::NotWritable)
            );
        }
    }
}
