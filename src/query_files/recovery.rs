//! Recovery files: the text of each tab that a crash must not lose (a draft,
//! or a file tab's edits not saved yet), apart from the user's files.
//!
//! Each is `<data folder>/recovery/<name>.sql`, plain UTF-8 with `\n` line
//! endings, so it can be opened by hand. Writing one never touches the last
//! explicitly saved script.
use super::{FILE_LIMIT, FileError, TextFormat, read, write};
use std::fs;
use std::path::{Path, PathBuf};

/// The folder of recovery files in the data folder.
pub fn folder(data_dir: &Path) -> PathBuf {
    data_dir.join("recovery")
}

/// A name for a tab's recovery file.
pub fn new_name() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Whether `name` is one this module gives (so a name read from the state
/// can never point outside the folder).
pub fn valid_name(name: &str) -> bool {
    name.len() == 32 && name.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn path(data_dir: &Path, name: &str) -> Option<PathBuf> {
    valid_name(name).then(|| folder(data_dir).join(format!("{name}.sql")))
}

/// Writes the recovery file `name` with `text`, atomically.
pub fn store(data_dir: &Path, name: &str, text: &str) -> Result<(), FileError> {
    let path = path(data_dir, name).ok_or(FileError::Missing)?;
    fs::create_dir_all(folder(data_dir)).map_err(|error| FileError::io(&error))?;
    write(&path, text, TextFormat::default(), true).map(|_| ())
}

/// The text of the recovery file `name`, within [`FILE_LIMIT`].
pub fn load(data_dir: &Path, name: &str) -> Result<String, FileError> {
    let path = path(data_dir, name).ok_or(FileError::Missing)?;
    read(&path).map(|loaded| loaded.text)
}

/// Where the recovery file `name` is, to name it in a notice.
pub fn location(data_dir: &Path, name: &str) -> Option<PathBuf> {
    path(data_dir, name)
}

/// Removes the recovery file `name`; one already gone is fine.
pub fn remove(data_dir: &Path, name: &str) {
    if let Some(path) = path(data_dir, name) {
        // Best effort: a recovery file left behind reopens as a draft.
        let _ = fs::remove_file(path);
    }
}

/// The names of every recovery file, to find those no tab names.
pub fn names(data_dir: &Path) -> Vec<String> {
    let Ok(children) = fs::read_dir(folder(data_dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = children
        .filter_map(Result::ok)
        .filter_map(|child| {
            let name = child.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".sql")
                .filter(|stem| valid_name(stem))
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

/// The size limit applies as for files: a larger recovery file is kept and
/// reported, never cut.
pub const LIMIT: u64 = FILE_LIMIT;

#[cfg(test)]
mod tests {
    use super::super::testing::Folder;
    use super::*;

    #[test]
    fn recovery_files_round_trip_and_stay_in_their_folder() {
        let data = Folder::new();
        let name = new_name();
        store(data.path(), &name, "SELECT 1;\nSELECT 'é';").unwrap();
        assert_eq!(load(data.path(), &name).unwrap(), "SELECT 1;\nSELECT 'é';");
        store(data.path(), &name, "SELECT 2;").unwrap();
        assert_eq!(load(data.path(), &name).unwrap(), "SELECT 2;");
        assert_eq!(names(data.path()), std::slice::from_ref(&name));
        // Names from the state can never reach another folder.
        assert!(!valid_name("../config"));
        assert_eq!(
            load(data.path(), "../config").unwrap_err(),
            FileError::Missing
        );
        remove(data.path(), &name);
        remove(data.path(), &name);
        assert!(names(data.path()).is_empty());
        assert_eq!(load(data.path(), &name).unwrap_err(), FileError::Missing);
    }
}
