//! Discovery: the `.sql` files of the library's directory, for the library
//! list and the palette. A scan only reads; it never writes.
use super::{FileError, names};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Directories deeper than this are not entered.
pub const MAX_DEPTH: usize = 8;
/// Listing stops after this many files.
pub const MAX_FILES: usize = 10_000;
/// SQL kept per file for the preview and the search.
pub const TEXT_PER_FILE: usize = 16 * 1024;
/// SQL kept for every file together.
pub const TEXT_TOTAL: usize = 32 * 1024 * 1024;

/// A file found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    /// Its file name without the extension.
    pub title: String,
    /// The start of its SQL (`\n` line endings), for the preview and the
    /// search; empty past the bounds or when the start is not UTF-8.
    pub text: String,
}

/// What a scan found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub files: Vec<Found>,
    /// A bound stopped the scan: deeper directories or more files exist.
    pub stopped: bool,
}

/// Lists the `.sql` files under `directory` (extension in any case),
/// skipping hidden directories (`.git`) and directory links, in name order.
/// `stop` is asked between files. A directory that does not exist is an
/// empty library.
pub fn scan(directory: &Path, stop: &dyn Fn() -> bool) -> Result<Scan, FileError> {
    let mut found = Scan::default();
    if fs::symlink_metadata(directory).is_err() {
        return Ok(found);
    }
    let mut text_left = TEXT_TOTAL;
    walk(directory, 0, stop, &mut found, &mut text_left)?;
    Ok(found)
}

fn walk(
    folder: &Path,
    depth: usize,
    stop: &dyn Fn() -> bool,
    found: &mut Scan,
    text_left: &mut usize,
) -> Result<(), FileError> {
    let mut children: Vec<(PathBuf, fs::FileType)> = match fs::read_dir(folder) {
        Ok(children) => children
            .filter_map(Result::ok)
            .filter_map(|child| Some((child.path(), child.file_type().ok()?)))
            .collect(),
        // A folder that cannot be read is skipped, except the library's own.
        Err(error) if depth == 0 => return Err(FileError::io(&error)),
        Err(_) => return Ok(()),
    };
    children.sort_by_key(|(path, _)| path.file_name().map(|name| name.to_ascii_lowercase()));
    for (path, kind) in children {
        if stop() {
            return Err(FileError::Cancelled);
        }
        let hidden = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'));
        if kind.is_dir() {
            if hidden {
                continue;
            }
            if depth + 1 >= MAX_DEPTH {
                found.stopped = true;
                continue;
            }
            walk(&path, depth + 1, stop, found, text_left)?;
        } else if kind.is_file() && names::is_sql(&path) {
            if found.files.len() >= MAX_FILES {
                found.stopped = true;
                return Ok(());
            }
            let text = start_of(&path, (*text_left).min(TEXT_PER_FILE));
            *text_left = text_left.saturating_sub(text.len());
            found.files.push(Found {
                title: names::title_of(&path),
                path,
                text,
            });
        }
        if found.stopped && found.files.len() >= MAX_FILES {
            return Ok(());
        }
    }
    Ok(())
}

/// The first `limit` bytes of a file as text, cut at a character boundary;
/// empty when they are not UTF-8 or cannot be read.
fn start_of(path: &Path, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let mut bytes = Vec::with_capacity(limit.min(4096));
    let read =
        fs::File::open(path).and_then(|file| file.take(limit as u64).read_to_end(&mut bytes));
    if read.is_err() {
        return String::new();
    }
    // The read stopped at the limit, so its last character may be cut.
    let cut = bytes.len() >= limit;
    let bytes = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
        .unwrap_or(&bytes);
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        // Cut inside a character at the limit: keep what is whole.
        Err(error) if cut && error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default()
        }
        Err(_) => return String::new(),
    };
    text.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::super::testing::Folder;
    use super::*;

    #[test]
    fn a_scan_lists_sql_files_and_skips_hidden_folders() {
        let folder = Folder::new();
        let root = folder.path();
        fs::create_dir_all(root.join("sales/2026")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("b.sql"), "SELECT 2;\r\n").unwrap();
        fs::write(root.join("A.SQL"), "\u{feff}SELECT 1;").unwrap();
        fs::write(root.join("notes.txt"), "not sql").unwrap();
        fs::write(root.join(".git/x.sql"), "hidden").unwrap();
        fs::write(root.join("sales/2026/q1.sql"), "SELECT 'é';").unwrap();
        fs::write(root.join("latin1.sql"), b"caf\xE9").unwrap();
        let scan = scan(root, &|| false).unwrap();
        let titles: Vec<&str> = scan
            .files
            .iter()
            .map(|found| found.title.as_str())
            .collect();
        assert_eq!(titles, ["A", "b", "latin1", "q1"]);
        assert_eq!(scan.files[1].text, "SELECT 2;\n");
        assert_eq!(scan.files[0].text, "SELECT 1;");
        // A file that is not UTF-8 is listed, with no text to search.
        assert_eq!(scan.files[2].text, "");
        assert!(!scan.stopped);
        // A missing directory is an empty library; stopping is an error.
        assert_eq!(
            super::scan(&root.join("none"), &|| false).unwrap(),
            Scan::default()
        );
        assert_eq!(
            super::scan(root, &|| true).unwrap_err(),
            FileError::Cancelled
        );
    }

    #[test]
    fn deep_folders_and_long_files_are_bounded() {
        let folder = Folder::new();
        let mut deep = folder.path().to_path_buf();
        for level in 0..MAX_DEPTH + 1 {
            deep = deep.join(format!("l{level}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("far.sql"), "SELECT 1;").unwrap();
        fs::write(folder.path().join("long.sql"), "é".repeat(TEXT_PER_FILE)).unwrap();
        let scan = scan(folder.path(), &|| false).unwrap();
        assert!(scan.stopped);
        assert_eq!(scan.files.len(), 1);
        // Cut at a character, within the bound.
        assert!(scan.files[0].text.len() <= TEXT_PER_FILE);
        assert!(scan.files[0].text.chars().all(|ch| ch == 'é'));
    }
}
