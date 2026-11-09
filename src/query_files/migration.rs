//! Moving the saved queries kept in `config.db` (`workspace.state`) to
//! `.sql` files ([ADR-0031](../../../docs/adr/0031-saved-queries-as-sql-files.md)).
//!
//! The application plans the files, writes the plan to `query_library`
//! before creating any ([`super::library::QueryLibrary::migration`]), writes
//! and verifies them here, keeps a backup of the original queries, and only
//! then records the files and drops the queries from `workspace.state`. A
//! crash at any step neither duplicates nor loses a query: a planned file
//! that exists and holds the query's SQL is adopted, one that holds
//! something else is kept and the query takes the next free name. Nothing
//! here runs SQL.
use super::library::{Entry, Planned, QueryLibrary};
use super::{FileError, TextFormat, names, read, write};
use crate::drivers::ConnectionId;
use crate::reports::ConnectionRef;
use crate::workspace_state::QueryEntry;
use std::fs;
use std::path::{Path, PathBuf};

/// A query that is now a verified file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migrated {
    pub id: u64,
    pub path: PathBuf,
    /// Its title had to change to be a file name (made safe, or numbered).
    pub renamed: bool,
}

/// A query that could not be moved; it stays in `workspace.state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub id: u64,
    pub title: String,
    pub error: FileError,
}

/// The editor's text, as the file holds it.
fn normalized(sql: &str) -> String {
    sql.replace("\r\n", "\n")
}

/// Whether `path` is taken: a file there, or another planned one.
fn taken(path: &Path, planned: &[Planned]) -> bool {
    fs::symlink_metadata(path).is_ok()
        || planned
            .iter()
            .any(|other| names::same_path(&other.path, path))
}

/// A file in `directory` for each saved query, named by its title; a name
/// taken by an existing file or by another planned file (ignoring case
/// where the system does) is numbered.
pub fn plan(saved: &[QueryEntry], directory: &Path) -> Vec<Planned> {
    let mut planned: Vec<Planned> = Vec::with_capacity(saved.len());
    for query in saved {
        let path = names::free(directory, &query.title, &|path| taken(path, &planned));
        planned.push(Planned { id: query.id, path });
    }
    planned
}

/// Whether the file at `path` holds exactly `sql`.
fn holds(path: &Path, sql: &str) -> bool {
    read(path).is_ok_and(|loaded| loaded.text == normalized(sql))
}

/// Writes and verifies the planned files (on the blocking pool). A query
/// whose `library` entry already points at a file holding its SQL is
/// adopted as it is (a migration that crashed after its files were
/// recorded). Returns what moved and what did not.
pub fn write_files(
    saved: &[QueryEntry],
    plan: &[Planned],
    library: &QueryLibrary,
    directory: &Path,
) -> (Vec<Migrated>, Vec<Failure>) {
    let mut migrated = Vec::new();
    let mut failed = Vec::new();
    if let Err(error) = fs::create_dir_all(directory) {
        let error = FileError::io(&error);
        for query in saved {
            failed.push(Failure {
                id: query.id,
                title: query.title.clone(),
                error: error.clone(),
            });
        }
        return (migrated, failed);
    }
    for query in saved {
        if let Some(entry) = library.entry(query.id)
            && holds(&entry.path, &query.sql)
        {
            migrated.push(Migrated {
                id: query.id,
                renamed: names::title_of(&entry.path) != query.title,
                path: entry.path.clone(),
            });
            continue;
        }
        let first = plan
            .iter()
            .find(|planned| planned.id == query.id)
            .map(|planned| planned.path.clone())
            .unwrap_or_else(|| directory.join(names::file_name(&query.title)));
        match write_one(query, first, plan) {
            Ok(path) => migrated.push(Migrated {
                id: query.id,
                renamed: names::title_of(&path) != query.title,
                path,
            }),
            Err(error) => failed.push(Failure {
                id: query.id,
                title: query.title.clone(),
                error,
            }),
        }
    }
    (migrated, failed)
}

/// Writes one query at `first`, or adopts it there; another file in the
/// way keeps its content and the query takes the next free name.
fn write_one(query: &QueryEntry, first: PathBuf, plan: &[Planned]) -> Result<PathBuf, FileError> {
    let sql = normalized(&query.sql);
    let folder = first.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut path = first;
    for _ in 0..1000 {
        if fs::symlink_metadata(&path).is_ok() {
            if holds(&path, &sql) {
                return Ok(path);
            }
            let others: Vec<Planned> = plan
                .iter()
                .filter(|planned| planned.id != query.id)
                .cloned()
                .collect();
            path = names::free(&folder, &query.title, &|candidate| {
                taken(candidate, &others) || names::same_path(candidate, &path)
            });
            continue;
        }
        match write(&path, &sql, TextFormat::default(), false) {
            Ok(_) => {}
            // Someone created it between the check and the write: try again.
            Err(FileError::Exists) => continue,
            Err(error) => return Err(error),
        }
        // Read back: only a file that holds the query counts.
        return if holds(&path, &sql) {
            Ok(path)
        } else {
            Err(FileError::Io(
                "the file written does not hold the query's SQL".into(),
            ))
        };
    }
    Err(FileError::Exists)
}

/// Keeps the original saved queries (as `workspace.state` held them) in
/// `<data folder>/backups/saved-queries-<millis>.json`, never replacing a
/// file, and returns where.
pub fn backup(data_dir: &Path, saved: &serde_json::Value) -> Result<PathBuf, FileError> {
    let folder = data_dir.join("backups");
    fs::create_dir_all(&folder).map_err(|error| FileError::io(&error))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let text =
        serde_json::to_string_pretty(saved).map_err(|error| FileError::Io(error.to_string()))?;
    for attempt in 0..100 {
        let name = if attempt == 0 {
            format!("saved-queries-{stamp}.json")
        } else {
            format!("saved-queries-{stamp}-{attempt}.json")
        };
        let path = folder.join(name);
        match write(&path, &text, TextFormat::default(), false) {
            Ok(_) => return Ok(path),
            Err(FileError::Exists) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(FileError::Exists)
}

/// Records the verified files in `library`, with each query's identity,
/// tags and connection (its name, plus the identity of the connection of
/// that name when `connection_id` knows one), and ends the migration's
/// journal.
pub fn commit(
    library: &mut QueryLibrary,
    saved: &[QueryEntry],
    migrated: &[Migrated],
    connection_id: &dyn Fn(&str) -> Option<ConnectionId>,
) {
    for moved in migrated {
        let Some(query) = saved.iter().find(|query| query.id == moved.id) else {
            continue;
        };
        let connection = (!query.connection.is_empty()).then(|| ConnectionRef {
            id: connection_id(&query.connection),
            name: query.connection.clone(),
        });
        if let Some(entry) = library.entry_mut(moved.id) {
            entry.path = moved.path.clone();
            entry.tags = query.tags.clone();
            entry.connection = connection;
        } else if library.find(&moved.path).is_some() {
            // The file was discovered meanwhile under another identity:
            // that identity keeps it, and gets the query's local data.
            let id = library.identify(&moved.path);
            if let Some(entry) = library.entry_mut(id) {
                entry.tags = query.tags.clone();
                entry.connection = connection;
            }
        } else {
            let mut entry = Entry::new(moved.path.clone());
            entry.id = moved.id;
            entry.tags = query.tags.clone();
            entry.connection = connection;
            library.add(entry);
        }
    }
    library.migration.clear();
}

#[cfg(test)]
mod tests {
    use super::super::testing::Folder;
    use super::*;

    fn query(id: u64, title: &str, sql: &str, tags: &[&str]) -> QueryEntry {
        QueryEntry {
            id,
            title: title.into(),
            connection: "Local".into(),
            sql: sql.into(),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            ..QueryEntry::default()
        }
    }

    #[test]
    fn saved_queries_become_verified_files_without_overwriting_any() {
        let folder = Folder::new();
        let dir = folder.path().join("Queries");
        fs::create_dir_all(&dir).unwrap();
        // A file of the user's has the first query's name.
        fs::write(dir.join("orders.sql"), "-- mine").unwrap();
        let saved = vec![
            query(4, "orders", "SELECT 1;\r\nSELECT 2;", &["sales"]),
            query(7, "orders", "SELECT 3;", &[]),
            query(9, "Q1/Q2: totals", "SELECT 4;", &["finance", "q"]),
        ];
        let plan = plan(&saved, &dir);
        let names: Vec<String> = plan.iter().map(|p| names::title_of(&p.path)).collect();
        assert_eq!(names, ["orders (2)", "orders (3)", "Q1_Q2_ totals"]);
        let mut library = QueryLibrary::from_value(None).unwrap();
        library.migration = plan.clone();
        let (migrated, failed) = write_files(&saved, &plan, &library, &dir);
        assert!(failed.is_empty());
        assert_eq!(
            fs::read_to_string(dir.join("orders.sql")).unwrap(),
            "-- mine"
        );
        assert_eq!(
            read(&migrated[0].path).unwrap().text,
            "SELECT 1;\nSELECT 2;"
        );
        assert!(migrated.iter().all(|moved| moved.renamed));
        let id = ConnectionId::new();
        commit(&mut library, &saved, &migrated, &|name| {
            (name == "Local").then_some(id)
        });
        assert!(library.migration.is_empty());
        let entry = library.entry(9).unwrap();
        assert_eq!(entry.tags, ["finance", "q"]);
        assert_eq!(entry.connection.as_ref().and_then(|c| c.id), Some(id));
        assert_eq!(library.entry(4).unwrap().path, migrated[0].path);
        // Running it again adopts every file and writes nothing new.
        let again = super::plan(&saved, &dir);
        let (migrated_again, failed) = write_files(&saved, &again, &library, &dir);
        assert!(failed.is_empty());
        assert_eq!(migrated_again, migrated);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 4);
    }

    #[test]
    fn a_crash_after_the_journal_neither_duplicates_nor_loses_a_query() {
        let folder = Folder::new();
        let dir = folder.path().to_path_buf();
        let saved = vec![
            query(1, "a", "SELECT 1;", &[]),
            query(2, "b", "SELECT 2;", &[]),
        ];
        let plan = plan(&saved, &dir);
        // The first file was written before the crash; the second's name
        // was taken meanwhile by another file.
        fs::write(&plan[0].path, "SELECT 1;").unwrap();
        fs::write(&plan[1].path, "SELECT 99;").unwrap();
        let library = QueryLibrary::from_value(None).unwrap();
        let (migrated, failed) = write_files(&saved, &plan, &library, &dir);
        assert!(failed.is_empty());
        assert_eq!(migrated[0].path, plan[0].path);
        assert!(!migrated[0].renamed);
        assert_eq!(names::title_of(&migrated[1].path), "b (2)");
        assert_eq!(fs::read_to_string(&plan[1].path).unwrap(), "SELECT 99;");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3);
    }

    #[test]
    fn a_folder_that_cannot_be_written_fails_every_query_and_keeps_them() {
        let folder = Folder::new();
        // A file where the directory should be.
        let dir = folder.path().join("Queries");
        fs::write(&dir, "not a folder").unwrap();
        let saved = vec![query(1, "a", "SELECT 1;", &[])];
        let plan = plan(&saved, &dir);
        let library = QueryLibrary::from_value(None).unwrap();
        let (migrated, failed) = write_files(&saved, &plan, &library, &dir);
        assert!(migrated.is_empty());
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id, 1);
    }

    #[test]
    fn the_backup_keeps_the_original_queries_and_never_replaces_one() {
        let folder = Folder::new();
        let value = serde_json::json!([{"id": 1, "title": "a", "sql": "SELECT 1"}]);
        let first = backup(folder.path(), &value).unwrap();
        let second = backup(folder.path(), &value).unwrap();
        assert_ne!(first, second);
        let read: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&first).unwrap()).unwrap();
        assert_eq!(read, value);
    }
}
