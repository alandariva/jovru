//! The restored tabs' text at start, read off the UI thread: from each
//! tab's recovery file, otherwise from its `.sql` file (ADR-0031).
use super::{FileError, OnDisk, check, read, recovery};
use crate::workspace_state::WorkspaceState;
use std::path::{Path, PathBuf};

/// What a restored tab must tell the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The tab has edits not saved, and its file changed on disk meanwhile.
    Changed,
    /// Its file is not there any more.
    Missing,
    /// Its file could not be read.
    Unreadable(FileError),
    /// Its recovery file could not be read; it is kept at this path.
    RecoveryUnreadable(FileError, PathBuf),
}

/// What filling the tabs found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filled {
    /// By the tab's position.
    pub problems: Vec<(usize, Problem)>,
    /// Recovery files no tab names (a crash between writing one and the
    /// state): their name and text, to reopen as drafts.
    pub orphans: Vec<(String, Result<String, FileError>)>,
}

/// Fills each tab of `state`: its recovery file's text when it has one
/// (unsaved; the file's text, when unchanged, is what it is compared with),
/// otherwise its file's text, which also takes any change made elsewhere.
/// A tab of an earlier version keeps the SQL the state held.
pub fn fill(state: &mut WorkspaceState, data_dir: &Path) -> Filled {
    let mut filled = Filled::default();
    for (index, tab) in state.tabs.iter_mut().enumerate() {
        let mut recovered = false;
        if let Some(name) = tab.recovery.clone() {
            match recovery::load(data_dir, &name) {
                Ok(text) => {
                    tab.sql = text;
                    recovered = true;
                }
                Err(error) => {
                    let at = recovery::location(data_dir, &name).unwrap_or_default();
                    filled
                        .problems
                        .push((index, Problem::RecoveryUnreadable(error, at)));
                    // The file stays where it is; the tab takes another.
                    tab.recovery = None;
                }
            }
        }
        let Some(file) = tab.file.as_mut() else {
            continue;
        };
        if recovered {
            // The text to compare with: the file's, when it is as the tab
            // left it.
            tab.saved_sql = None;
            match check(&file.path, &file.base) {
                Ok(OnDisk::Same) => {
                    if let Ok(loaded) = read(&file.path) {
                        tab.saved_sql = Some(loaded.text);
                    }
                }
                Ok(OnDisk::Changed) => filled.problems.push((index, Problem::Changed)),
                Ok(OnDisk::Missing) => filled.problems.push((index, Problem::Missing)),
                Err(error) => filled.problems.push((index, Problem::Unreadable(error))),
            }
            continue;
        }
        match read(&file.path) {
            Ok(loaded) => {
                tab.sql = loaded.text.clone();
                tab.saved_sql = Some(loaded.text);
                file.base = loaded.base;
                file.format = loaded.format;
            }
            Err(FileError::Missing) => {
                tab.saved_sql = None;
                filled.problems.push((index, Problem::Missing));
            }
            Err(error) => {
                tab.saved_sql = None;
                filled.problems.push((index, Problem::Unreadable(error)));
            }
        }
    }
    let named: Vec<&str> = state
        .tabs
        .iter()
        .filter_map(|tab| tab.recovery.as_deref())
        .collect();
    filled.orphans = recovery::names(data_dir)
        .into_iter()
        .filter(|name| !named.contains(&name.as_str()))
        .map(|name| {
            let text = recovery::load(data_dir, &name);
            (name, text)
        })
        .collect();
    filled
}

#[cfg(test)]
mod tests {
    use super::super::testing::Folder;
    use super::super::{TextFormat, write};
    use super::*;
    use crate::workspace_state::{TabFile, TabState};

    #[test]
    fn tabs_come_back_from_recovery_files_and_their_files() {
        let data = Folder::new();
        let files = Folder::new();
        let clean = files.path().join("clean.sql");
        let edited = files.path().join("edited.sql");
        let changed = files.path().join("changed.sql");
        let gone = files.path().join("gone.sql");
        let base = |path: &Path, text: &str| TabFile {
            path: path.to_path_buf(),
            base: write(path, text, TextFormat::default(), true).unwrap(),
            format: TextFormat::default(),
        };
        let clean_file = base(&clean, "SELECT 1;");
        let edited_file = base(&edited, "SELECT 2;");
        let changed_file = base(&changed, "SELECT 3;");
        let gone_file = base(&gone, "SELECT 4;");
        std::fs::remove_file(&gone).unwrap();
        // Changed elsewhere while the application was closed.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&changed, "SELECT 30;").unwrap();
        std::fs::write(&clean, "SELECT 10;").unwrap();
        let draft = recovery::new_name();
        let pending = recovery::new_name();
        let pending_changed = recovery::new_name();
        let orphan = recovery::new_name();
        recovery::store(data.path(), &draft, "SELECT 'draft';").unwrap();
        recovery::store(data.path(), &pending, "SELECT 2; -- edited").unwrap();
        recovery::store(data.path(), &pending_changed, "SELECT 3; -- mine").unwrap();
        recovery::store(data.path(), &orphan, "SELECT 'lost';").unwrap();
        let tab = |file: Option<TabFile>, recovery: Option<&String>| TabState {
            file,
            recovery: recovery.cloned(),
            ..TabState::default()
        };
        let mut state = WorkspaceState {
            tabs: vec![
                tab(Some(clean_file), None),
                tab(Some(edited_file), Some(&pending)),
                tab(Some(changed_file), Some(&pending_changed)),
                tab(Some(gone_file), None),
                tab(None, Some(&draft)),
            ],
            ..WorkspaceState::default()
        };
        let filled = fill(&mut state, data.path());
        // A clean file tab takes the file's text, changes included.
        assert_eq!(state.tabs[0].sql, "SELECT 10;");
        assert_eq!(state.tabs[0].saved_sql.as_deref(), Some("SELECT 10;"));
        // Pending edits come back unsaved, compared with the file's text.
        assert_eq!(state.tabs[1].sql, "SELECT 2; -- edited");
        assert_eq!(state.tabs[1].saved_sql.as_deref(), Some("SELECT 2;"));
        assert_eq!(state.tabs[2].sql, "SELECT 3; -- mine");
        assert_eq!(state.tabs[2].saved_sql, None);
        assert_eq!(state.tabs[4].sql, "SELECT 'draft';");
        assert_eq!(
            filled.problems,
            [(2, Problem::Changed), (3, Problem::Missing)]
        );
        assert_eq!(filled.orphans, [(orphan, Ok("SELECT 'lost';".to_owned()))]);
    }
}
