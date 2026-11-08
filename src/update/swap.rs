//! Replaces the entries of Jykre Studio's folder with a release's, all or
//! nothing.
//!
//! The release is unpacked into `<folder>/.jykre-update/staging-<id>/`,
//! on the same file system, so every step is a rename. Each top-level entry
//! of the archive's folder replaces the entry of the same name, the
//! executable last; what it replaces is moved into `trash-<id>/`. A failed
//! step undoes the earlier ones in reverse order. Entries the archive does
//! not hold are never touched, nor is anything outside the folder.
//!
//! A running executable can be renamed on Linux and Windows. On Windows it
//! cannot be removed while it runs, so the trash is removed at the next
//! start ([`cleanup_leftovers`]).
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::drivers::DriverResult;

use super::error;
use super::release::Platform;

/// The folder, next to the executable, that holds an update's files.
pub const WORK_DIR: &str = ".jykre-update";

/// A release unpacked and checked, ready to be swapped in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedRelease {
    /// The archive's single top-level folder.
    pub root: PathBuf,
    /// Its entries, the executable last.
    pub entries: Vec<OsString>,
}

/// Checks what `unpacked` holds: exactly one folder (the archive's), with
/// the executable and `catalog/` in it, no symbolic link anywhere, and no
/// entry named like the work folder. On Unix the executable is made
/// executable.
pub fn validate_staged(unpacked: &Path, platform: Platform) -> DriverResult<StagedRelease> {
    let invalid = |_| error::unexpected_layout();
    let mut top = std::fs::read_dir(unpacked).map_err(|e| error::from_io(&e))?;
    let root = match (top.next(), top.next()) {
        (Some(entry), None) => entry.map_err(invalid)?.path(),
        _ => return Err(error::unexpected_layout()),
    };
    if !std::fs::symlink_metadata(&root)
        .map_err(invalid)?
        .file_type()
        .is_dir()
    {
        return Err(error::unexpected_layout());
    }
    no_links(&root)?;
    let executable = OsString::from(platform.executable());
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&root).map_err(invalid)? {
        let name = entry.map_err(invalid)?.file_name();
        if name == WORK_DIR {
            return Err(error::unexpected_layout());
        }
        if name != executable {
            entries.push(name);
        }
    }
    let exe = root.join(&executable);
    if !exe.is_file() || !root.join("catalog").is_dir() {
        return Err(error::unexpected_layout());
    }
    make_executable(&exe)?;
    entries.sort();
    entries.push(executable);
    Ok(StagedRelease { root, entries })
}

fn no_links(dir: &Path) -> DriverResult<()> {
    for entry in std::fs::read_dir(dir).map_err(|e| error::from_io(&e))? {
        let entry = entry.map_err(|e| error::from_io(&e))?;
        let kind = entry.file_type().map_err(|e| error::from_io(&e))?;
        if kind.is_symlink() {
            return Err(error::unexpected_layout());
        }
        if kind.is_dir() {
            no_links(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(exe: &Path) -> DriverResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(exe)
        .map_err(|e| error::from_io(&e))?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    std::fs::set_permissions(exe, permissions).map_err(|e| error::from_io(&e))
}

#[cfg(not(unix))]
fn make_executable(_exe: &Path) -> DriverResult<()> {
    Ok(())
}

/// Swaps `staged` into `dir`, moving what it replaces into `trash`.
/// Returns how many entries were replaced or added.
pub fn apply(dir: &Path, staged: &StagedRelease, trash: &Path) -> DriverResult<usize> {
    apply_with(dir, staged, trash, &mut rename_retrying)
}

/// [`apply`] with the rename to use, so tests can make a step fail.
pub(crate) fn apply_with(
    dir: &Path,
    staged: &StagedRelease,
    trash: &Path,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> DriverResult<usize> {
    std::fs::create_dir_all(trash).map_err(|e| error::from_io(&e))?;
    let mut done: Vec<Step> = Vec::new();
    for name in &staged.entries {
        let target = dir.join(name);
        let incoming = staged.root.join(name);
        if std::fs::symlink_metadata(&target).is_ok() {
            let aside = trash.join(name);
            if let Err(failure) = rename(&target, &aside) {
                return Err(undo(done, rename, &failure));
            }
            done.push(Step::MovedAside {
                from: target.clone(),
                to: aside,
            });
        }
        if let Err(failure) = rename(&incoming, &target) {
            return Err(undo(done, rename, &failure));
        }
        done.push(Step::Placed {
            at: target,
            from: incoming,
        });
    }
    Ok(staged.entries.len())
}

/// One rename the swap made, to undo.
enum Step {
    MovedAside { from: PathBuf, to: PathBuf },
    Placed { at: PathBuf, from: PathBuf },
}

/// Undoes `done` in reverse order and returns the error to report: the
/// original failure, or that the folder could not be put back.
fn undo(
    done: Vec<Step>,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    failure: &io::Error,
) -> crate::drivers::DriverError {
    tracing::warn!(kind = ?failure.kind(), "update swap failed; restoring the folder");
    let mut restored = true;
    for step in done.into_iter().rev() {
        let (from, to) = match step {
            Step::Placed { at, from } => (at, from),
            Step::MovedAside { from, to } => (to, from),
        };
        if let Err(error) = rename(&from, &to) {
            tracing::error!(kind = ?error.kind(), "could not restore an entry after a failed update");
            restored = false;
        }
    }
    if restored {
        error::from_io(failure)
    } else {
        error::not_restored()
    }
}

/// A rename, retried a few times on Windows, where an antivirus or the
/// indexer may hold a new file for a moment.
fn rename_retrying(from: &Path, to: &Path) -> io::Result<()> {
    const ATTEMPTS: u32 = if cfg!(windows) { 5 } else { 1 };
    let mut attempt = 1;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < ATTEMPTS && error.kind() == io::ErrorKind::PermissionDenied => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Removes what earlier updates left in `dir`'s work folder: finished
/// trash (`done-*`) at any age, and an unfinished one or a staging folder
/// only when older than `older_than`, so an update running in another
/// window is left alone. A file still in use stays for the next start.
pub fn cleanup_leftovers(dir: &Path, older_than: Duration) {
    let work = dir.join(WORK_DIR);
    let Ok(entries) = std::fs::read_dir(&work) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= older_than);
        let remove = name.starts_with("done-")
            || (old && (name.starts_with("trash-") || name.starts_with("staging-")));
        if remove {
            let path = entry.path();
            let removed = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(error) = removed {
                tracing::debug!(kind = ?error.kind(), "update leftover kept for later");
            }
        }
    }
    // Only when empty.
    let _ = std::fs::remove_dir(&work);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::tests::{TempDir, tree};

    const PLATFORM: Platform = Platform::LinuxX86_64;

    /// An installed folder and a staged release of a newer version.
    fn fixture(temp: &TempDir) -> (PathBuf, StagedRelease, PathBuf) {
        let dir = temp.path().join("app");
        std::fs::create_dir_all(dir.join("catalog/postgresql")).unwrap();
        std::fs::write(dir.join("jykre"), b"old binary").unwrap();
        std::fs::write(dir.join("README.md"), b"old readme").unwrap();
        std::fs::write(dir.join("catalog/postgresql/manifest.json"), b"old").unwrap();
        std::fs::write(dir.join("notes.txt"), b"the user's").unwrap();
        let unpacked = dir.join(WORK_DIR).join("staging-1").join("files");
        let root = unpacked.join("jykre-studio-v0.2.1-linux-x86_64");
        std::fs::create_dir_all(root.join("catalog/postgresql")).unwrap();
        std::fs::write(root.join("jykre"), b"new binary").unwrap();
        std::fs::write(root.join("README.md"), b"new readme").unwrap();
        std::fs::write(root.join("LICENSE"), b"new license").unwrap();
        std::fs::write(root.join("catalog/postgresql/manifest.json"), b"new").unwrap();
        let staged = validate_staged(&unpacked, PLATFORM).unwrap();
        let trash = dir.join(WORK_DIR).join("trash-1");
        (dir, staged, trash)
    }

    fn outside_work(dir: &Path) -> Vec<(String, Vec<u8>)> {
        tree(dir)
            .into_iter()
            .filter(|(path, _)| !path.starts_with(WORK_DIR))
            .collect()
    }

    #[test]
    fn the_executable_is_swapped_last() {
        let temp = TempDir::new();
        let (_, staged, _) = fixture(&temp);
        let names: Vec<_> = staged
            .entries
            .iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["LICENSE", "README.md", "catalog", "jykre"]);
    }

    #[test]
    fn a_swap_replaces_the_release_entries_and_keeps_the_rest() {
        let temp = TempDir::new();
        let (dir, staged, trash) = fixture(&temp);
        assert_eq!(apply(&dir, &staged, &trash).unwrap(), 4);
        let files = outside_work(&dir);
        let content = |name: &str| {
            files
                .iter()
                .find(|(path, _)| path == name)
                .map(|(_, bytes)| bytes.clone())
        };
        assert_eq!(content("jykre").as_deref(), Some(&b"new binary"[..]));
        assert_eq!(content("LICENSE").as_deref(), Some(&b"new license"[..]));
        assert_eq!(
            content("catalog/postgresql/manifest.json").as_deref(),
            Some(&b"new"[..])
        );
        assert_eq!(content("notes.txt").as_deref(), Some(&b"the user's"[..]));
        assert_eq!(
            std::fs::read(trash.join("jykre")).unwrap(),
            b"old binary",
            "the replaced executable waits in the trash"
        );
    }

    #[test]
    fn a_failure_at_any_step_leaves_the_folder_as_it_was() {
        // 4 entries: 3 replaced (two renames each) and LICENSE added (one).
        for failing in 1..=7 {
            let temp = TempDir::new();
            let (dir, staged, trash) = fixture(&temp);
            let before = outside_work(&dir);
            let mut count = 0;
            let mut rename = |from: &Path, to: &Path| {
                count += 1;
                if count == failing {
                    Err(io::Error::other("injected"))
                } else {
                    std::fs::rename(from, to)
                }
            };
            let error = apply_with(&dir, &staged, &trash, &mut rename).unwrap_err();
            assert_eq!(error.code, crate::drivers::ErrorCode::Internal);
            assert_eq!(outside_work(&dir), before, "step {failing}");
        }
    }

    #[test]
    fn a_folder_that_cannot_be_restored_says_so() {
        let temp = TempDir::new();
        let (dir, staged, trash) = fixture(&temp);
        let mut count = 0;
        let mut rename = |from: &Path, to: &Path| {
            count += 1;
            if count >= 3 {
                Err(io::Error::other("injected"))
            } else {
                std::fs::rename(from, to)
            }
        };
        let error = apply_with(&dir, &staged, &trash, &mut rename).unwrap_err();
        assert!(error.message.contains("could not be put back"));
    }

    #[test]
    fn staged_folders_are_checked() {
        let temp = TempDir::new();
        let unpacked = temp.path().join("two");
        std::fs::create_dir_all(unpacked.join("a")).unwrap();
        std::fs::create_dir_all(unpacked.join("b")).unwrap();
        assert!(validate_staged(&unpacked, PLATFORM).is_err());

        let unpacked = temp.path().join("no-exe");
        std::fs::create_dir_all(unpacked.join("root/catalog")).unwrap();
        assert!(validate_staged(&unpacked, PLATFORM).is_err());

        let unpacked = temp.path().join("no-catalog");
        std::fs::create_dir_all(unpacked.join("root")).unwrap();
        std::fs::write(unpacked.join("root/jykre"), b"x").unwrap();
        assert!(validate_staged(&unpacked, PLATFORM).is_err());

        let unpacked = temp.path().join("work-dir");
        std::fs::create_dir_all(unpacked.join("root/catalog")).unwrap();
        std::fs::create_dir_all(unpacked.join("root").join(WORK_DIR)).unwrap();
        std::fs::write(unpacked.join("root/jykre"), b"x").unwrap();
        assert!(validate_staged(&unpacked, PLATFORM).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn links_are_refused_and_the_executable_runs() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new();
        let unpacked = temp.path().join("ok");
        std::fs::create_dir_all(unpacked.join("root/catalog")).unwrap();
        std::fs::write(unpacked.join("root/jykre"), b"x").unwrap();
        let staged = validate_staged(&unpacked, PLATFORM).unwrap();
        let mode = std::fs::metadata(staged.root.join("jykre"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o755, 0o755);

        std::os::unix::fs::symlink("/etc", unpacked.join("root/catalog/link")).unwrap();
        assert!(validate_staged(&unpacked, PLATFORM).is_err());
    }

    #[test]
    fn leftovers_are_removed_unless_in_use_elsewhere() {
        let temp = TempDir::new();
        let work = temp.path().join(WORK_DIR);
        std::fs::create_dir_all(work.join("done-1")).unwrap();
        std::fs::create_dir_all(work.join("staging-2")).unwrap();
        cleanup_leftovers(temp.path(), Duration::from_secs(3600));
        assert!(!work.join("done-1").exists());
        assert!(work.join("staging-2").exists(), "a recent update is kept");
        cleanup_leftovers(temp.path(), Duration::ZERO);
        assert!(!work.exists());
    }
}
