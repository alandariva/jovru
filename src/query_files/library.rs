//! What is local to this machine about the saved-query files, under the
//! `query_library` setting: the library's directory, each file's stable
//! identity, its tags and its connection, and a migration in progress.
//! Never SQL or titles (the files hold them).
//!
//! It lives apart from `workspace.state`, whose writer keeps only the fields
//! it knows ([ADR-0024](../../../docs/adr/0024-reports.md)). Keys this
//! version does not know are kept, in the value and in each entry.
use super::names;
use crate::reports::ConnectionRef;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// The settings key.
pub const SETTINGS_KEY: &str = "query_library";

/// A file the library knows locally.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// Stable identity, used by tabs and the palette.
    pub id: u64,
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Labels without `#`, each once ignoring case.
    pub tags: Vec<String>,
    /// The connection its tab opens on.
    pub connection: Option<ConnectionRef>,
    extra: Map<String, Value>,
}

/// One query of a migration in progress: the identity it had in
/// `workspace.state` and the file planned for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub id: u64,
    pub path: PathBuf,
}

/// A stored value that cannot be read: it is reported and never written
/// over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unreadable;

/// The local state of the library.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryLibrary {
    /// The directory the user chose; `None` for the default.
    pub directory: Option<PathBuf>,
    pub next_id: u64,
    pub entries: Vec<Entry>,
    /// The plan of a migration that has not finished, written before any
    /// file is created ([`super::migration`]).
    pub migration: Vec<Planned>,
    extra: Map<String, Value>,
}

/// Keys of `object` other than `known`, as they were.
fn unknown(object: &Map<String, Value>, known: &[&str]) -> Map<String, Value> {
    object
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The labels of `raw`, cleaned as typed tags are.
fn tags(raw: &Value) -> Vec<String> {
    let text: Vec<&str> = raw
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    crate::workspace_state::parse_tags(&text.join(","))
}

impl Entry {
    /// An entry for the file at `path`, with no identity yet (one is given
    /// by [`QueryLibrary::add`]), tags or connection.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            ..Self::default()
        }
    }

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let id = value["id"].as_u64().filter(|id| *id > 0)?;
        let path = PathBuf::from(value["path"].as_str().filter(|path| !path.is_empty())?);
        Some(Self {
            id,
            path,
            tags: tags(&value["tags"]),
            connection: ConnectionRef::from_value(&value["connection"]),
            extra: unknown(object, &["id", "path", "tags", "connection"]),
        })
    }

    fn to_value(&self) -> Value {
        let mut object = self.extra.clone();
        object.insert("id".into(), json!(self.id));
        object.insert("path".into(), json!(self.path.to_string_lossy()));
        object.insert("tags".into(), json!(self.tags));
        object.insert(
            "connection".into(),
            self.connection
                .as_ref()
                .map_or(Value::Null, ConnectionRef::to_value),
        );
        Value::Object(object)
    }
}

impl QueryLibrary {
    /// Reads the stored value. A missing value is the default directory with
    /// nothing local; `Err` when the value cannot be read, which the caller
    /// reports and never writes over.
    pub fn from_value(value: Option<Value>) -> Result<Self, Unreadable> {
        let Some(value) = value else {
            return Ok(Self {
                next_id: 1,
                ..Self::default()
            });
        };
        let object = value.as_object().ok_or(Unreadable)?;
        let mut entries: Vec<Entry> = Vec::new();
        for entry in value["entries"].as_array().into_iter().flatten() {
            let entry = Entry::from_value(entry).ok_or(Unreadable)?;
            // One entry per file and per identity.
            if entries
                .iter()
                .any(|kept| kept.id == entry.id || names::same_path(&kept.path, &entry.path))
            {
                continue;
            }
            entries.push(entry);
        }
        let next_id = value["next_id"]
            .as_u64()
            .unwrap_or(1)
            .max(entries.iter().map(|entry| entry.id + 1).max().unwrap_or(1));
        let migration = value["migration"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|planned| {
                Some(Planned {
                    id: planned["id"].as_u64()?,
                    path: PathBuf::from(planned["path"].as_str()?),
                })
            })
            .collect();
        Ok(Self {
            directory: value["directory"]
                .as_str()
                .filter(|directory| !directory.is_empty())
                .map(PathBuf::from),
            next_id,
            entries,
            migration,
            extra: unknown(
                object,
                &["version", "directory", "next_id", "entries", "migration"],
            ),
        })
    }

    pub fn to_value(&self) -> Value {
        let mut object = self.extra.clone();
        object.insert("version".into(), json!(1));
        object.insert(
            "directory".into(),
            json!(self.directory.as_ref().map(|path| path.to_string_lossy())),
        );
        object.insert("next_id".into(), json!(self.next_id.max(1)));
        object.insert(
            "entries".into(),
            Value::Array(self.entries.iter().map(Entry::to_value).collect()),
        );
        object.insert(
            "migration".into(),
            if self.migration.is_empty() {
                Value::Null
            } else {
                Value::Array(
                    self.migration
                        .iter()
                        .map(|planned| {
                            json!({"id": planned.id, "path": planned.path.to_string_lossy()})
                        })
                        .collect(),
                )
            },
        );
        Value::Object(object)
    }

    /// The library's directory: the one chosen, or the default.
    pub fn directory(&self, data_dir: &Path) -> PathBuf {
        self.directory
            .clone()
            .unwrap_or_else(|| default_directory(data_dir))
    }

    pub fn entry(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub fn entry_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.id == id)
    }

    /// The entry of the file at `path`.
    pub fn find(&self, path: &Path) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|entry| names::same_path(&entry.path, path))
    }

    /// The identity of the file at `path`, given one when it has none yet.
    pub fn identify(&mut self, path: &Path) -> u64 {
        if let Some(entry) = self.find(path) {
            return entry.id;
        }
        self.add(Entry {
            path: path.to_path_buf(),
            ..Entry::default()
        })
    }

    /// Adds an entry with the next identity (an `id` of 0) or its own, and
    /// returns its identity.
    pub fn add(&mut self, mut entry: Entry) -> u64 {
        if entry.id == 0 || self.entry(entry.id).is_some() {
            entry.id = self.next_id.max(1);
        }
        self.next_id = self.next_id.max(entry.id + 1);
        let id = entry.id;
        self.entries.push(entry);
        id
    }

    /// A file renamed or saved elsewhere by the application: its entry, with
    /// its tags and connection, follows it.
    pub fn moved(&mut self, id: u64, path: &Path) {
        if let Some(entry) = self.entry_mut(id) {
            entry.path = path.to_path_buf();
        }
    }

    /// Forgets the entry `id` (the user removed a missing file from the
    /// library).
    pub fn forget(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        before != self.entries.len()
    }
}

/// The default directory of saved queries: `<Documents>/Jykre Studio/Queries`;
/// without a Documents folder, `<home>/Documents/…`; without a home, the data
/// folder's `Queries`. With `JYKRE_CONFIG_DIR` set, `<it>/Queries`, so tests
/// and manual checks never touch the user's documents.
pub fn default_directory(data_dir: &Path) -> PathBuf {
    let isolated = std::env::var_os(crate::connection_store::CONFIG_DIR_VARIABLE)
        .filter(|dir| !dir.is_empty());
    resolve_default(
        isolated.map(PathBuf::from),
        dirs::document_dir(),
        dirs::home_dir(),
        data_dir,
    )
}

fn resolve_default(
    isolated: Option<PathBuf>,
    documents: Option<PathBuf>,
    home: Option<PathBuf>,
    data_dir: &Path,
) -> PathBuf {
    if let Some(isolated) = isolated {
        return isolated.join("Queries");
    }
    documents
        .or_else(|| home.map(|home| home.join("Documents")))
        .map(|documents| documents.join("Jykre Studio").join("Queries"))
        .unwrap_or_else(|| data_dir.join("Queries"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::ConnectionId;

    #[test]
    fn local_state_round_trips_and_keeps_unknown_keys() {
        let id = ConnectionId::new();
        let stored = json!({
            "version": 1,
            "future": [1],
            "directory": null,
            "next_id": 3,
            "entries": [
                {"id": 1, "path": "C:/q/a.sql", "tags": ["#Finance", "finance", " x "],
                 "connection": {"id": id.to_string(), "name": "Local"}, "pinned": true},
                {"id": 2, "path": "C:/q/b.sql", "tags": [], "connection": null},
                {"id": 2, "path": "C:/q/c.sql"},
            ],
            "migration": [{"id": 9, "path": "C:/q/old.sql"}],
        });
        let library = QueryLibrary::from_value(Some(stored)).unwrap();
        assert_eq!(library.entries.len(), 2);
        assert_eq!(library.entries[0].tags, ["Finance", "x"]);
        assert_eq!(
            library.entries[0].connection.as_ref().and_then(|c| c.id),
            Some(id)
        );
        assert_eq!(library.migration[0].id, 9);
        let written = library.to_value();
        assert_eq!(written["future"], json!([1]));
        assert_eq!(written["entries"][0]["pinned"], json!(true));
        assert_eq!(QueryLibrary::from_value(Some(written)).unwrap(), library);
        assert!(QueryLibrary::from_value(Some(json!("broken"))).is_err());
        assert!(QueryLibrary::from_value(Some(json!({"entries": [{"path": "x"}]}))).is_err());
        let empty = QueryLibrary::from_value(None).unwrap();
        assert!(empty.entries.is_empty() && empty.next_id == 1);
    }

    #[test]
    fn identities_are_stable_and_follow_a_moved_file() {
        let mut library = QueryLibrary::from_value(None).unwrap();
        let a = library.identify(Path::new("/q/a.sql"));
        assert_eq!(library.identify(Path::new("/q/a.sql")), a);
        let b = library.identify(Path::new("/q/b.sql"));
        assert_ne!(a, b);
        library.entry_mut(a).unwrap().tags = vec!["sales".into()];
        library.moved(a, Path::new("/q/renamed.sql"));
        assert_eq!(
            library.find(Path::new("/q/renamed.sql")).unwrap().tags,
            ["sales"]
        );
        assert!(library.find(Path::new("/q/a.sql")).is_none());
        assert!(library.forget(b) && !library.forget(b));
        // An identity already used gets the next one.
        let c = library.add(Entry {
            id: a,
            path: "/q/c.sql".into(),
            ..Entry::default()
        });
        assert!(c > b);
    }

    #[test]
    fn the_default_directory_stays_isolated_in_tests() {
        let data = Path::new("/data");
        assert_eq!(
            resolve_default(Some("/iso".into()), Some("/docs".into()), None, data),
            PathBuf::from("/iso/Queries")
        );
        assert_eq!(
            resolve_default(None, Some("/docs".into()), None, data),
            PathBuf::from("/docs/Jykre Studio/Queries")
        );
        assert_eq!(
            resolve_default(None, None, Some("/home/u".into()), data),
            PathBuf::from("/home/u/Documents/Jykre Studio/Queries")
        );
        assert_eq!(
            resolve_default(None, None, None, data),
            PathBuf::from("/data/Queries")
        );
    }
}
