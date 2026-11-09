//! The run's temporary folder: a new folder under the system's temporary
//! directory, named for this process, that every generated file and
//! database goes into and that is removed at the end. The run never writes
//! in the user's data folder.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The start of every folder's name.
pub const PREFIX: &str = "jykre-performance-";

/// A folder the run made; dropping it removes it, as a last resort.
#[derive(Debug)]
pub struct TempFolder {
    path: PathBuf,
    removed: bool,
}

impl TempFolder {
    /// Makes a new, empty folder; never reuses one that exists.
    pub fn create() -> Result<Self, String> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{PREFIX}{}-{stamp}", std::process::id()));
        std::fs::create_dir(&path)
            .map_err(|error| format!("Could not create a temporary folder: {error}"))?;
        Ok(Self {
            path,
            removed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the folder and everything in it, trying again for a while
    /// when a file is still held (a database the driver is closing).
    pub fn remove(mut self) -> Result<(), String> {
        self.removed = true;
        remove(&self.path, 20, Duration::from_millis(250))
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        if !self.removed {
            let _ = remove(&self.path, 1, Duration::ZERO);
        }
    }
}

/// Removes `path`, one of the run's folders, trying `attempts` times.
/// Refuses any other path, so a mistake can never remove a user's folder.
pub fn remove(path: &Path, attempts: usize, wait: Duration) -> Result<(), String> {
    if !is_ours(path) {
        return Err(format!(
            "{} is not a performance run's temporary folder",
            path.display()
        ));
    }
    let mut last = String::new();
    for attempt in 0..attempts.max(1) {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => last = error.to_string(),
        }
        if attempt + 1 < attempts {
            std::thread::sleep(wait);
        }
    }
    Err(format!(
        "The temporary folder {} could not be removed: {last}",
        path.display()
    ))
}

/// Whether `path` is a folder this module names: directly under the
/// temporary directory, with the run's prefix.
fn is_ours(path: &Path) -> bool {
    path.parent() == Some(std::env::temp_dir().as_path())
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(PREFIX) && name.len() > PREFIX.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_made_new_and_removed_with_its_files() {
        let folder = TempFolder::create().unwrap();
        let path = folder.path().to_path_buf();
        std::fs::create_dir(path.join("inner")).unwrap();
        std::fs::write(path.join("inner").join("file.csv"), b"a,b\n").unwrap();
        folder.remove().unwrap();
        assert!(!path.exists());
        // Dropping removes it too.
        let dropped = TempFolder::create().unwrap();
        let path = dropped.path().to_path_buf();
        drop(dropped);
        assert!(!path.exists());
    }

    #[test]
    fn nothing_but_a_run_folder_is_ever_removed() {
        assert!(remove(&std::env::temp_dir(), 1, Duration::ZERO).is_err());
        assert!(remove(&std::env::temp_dir().join(PREFIX), 1, Duration::ZERO).is_err());
        assert!(remove(Path::new("/home"), 1, Duration::ZERO).is_err());
        let nested = std::env::temp_dir()
            .join("other")
            .join(format!("{PREFIX}1"));
        assert!(remove(&nested, 1, Duration::ZERO).is_err());
    }
}
