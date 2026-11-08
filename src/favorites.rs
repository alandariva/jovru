//! Explorer favorites: connections, tables and views the user keeps at hand
//! (0.2.0 M9).
//!
//! A favorite names its connection by identity, so it survives a rename,
//! and its object by structural path ([`ObjectPath`]), never by a label. It
//! is stored as one JSON value in `config.db` settings
//! ([Persistence](../docs/reference/persistence.md)); keys this version does
//! not know are kept, and a value that cannot be read is reported and never
//! written over. Listing favorites reads nothing from a server: whether an
//! object still exists is answered by the metadata cache.
use crate::drivers::{ConnectionId, ObjectKind, ObjectPath};
use serde_json::{Map, Value, json};

/// The settings key favorites are stored under.
pub const SETTINGS_KEY: &str = "explorer.favorites";

/// One favorite: a connection (`path` is `None`) or a table or view of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Favorite {
    pub connection: ConnectionId,
    /// The object's structural path in the connection's metadata, or `None`
    /// for the connection itself.
    pub path: Option<ObjectPath>,
    /// Where the object's rows are read from when it differs from `path`
    /// (the explorer's `stored_at`).
    pub stored_at: Option<ObjectPath>,
    /// Keys this version does not know, kept as they were.
    extra: Map<String, Value>,
}

impl Favorite {
    pub fn connection(connection: ConnectionId) -> Self {
        Self {
            connection,
            path: None,
            stored_at: None,
            extra: Map::new(),
        }
    }

    pub fn object(
        connection: ConnectionId,
        path: ObjectPath,
        stored_at: Option<ObjectPath>,
    ) -> Self {
        Self {
            connection,
            stored_at: stored_at.filter(|stored| *stored != path),
            path: Some(path),
            extra: Map::new(),
        }
    }

    /// The kind of its object, `None` for a connection.
    pub fn kind(&self) -> Option<ObjectKind> {
        self.path.as_ref()?.0.last().map(|segment| segment.kind)
    }

    /// Whether it names the same connection and object as `other`.
    pub fn same(&self, connection: ConnectionId, path: Option<&ObjectPath>) -> bool {
        self.connection == connection && self.path.as_ref() == path
    }

    fn from_value(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let connection = serde_json::from_value(value["connection"].clone()).ok()?;
        let path: Option<ObjectPath> = match &value["path"] {
            Value::Null => None,
            path => Some(serde_json::from_value(path.clone()).ok()?),
        };
        // Only connections, tables and views are favorites; a path to
        // anything else is not one this version can open.
        if path.as_ref().is_some_and(|path| {
            !path
                .0
                .last()
                .is_some_and(|segment| segment.kind.is_relation())
        }) {
            return None;
        }
        let stored_at = serde_json::from_value(value["stored_at"].clone()).ok();
        let extra = object
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "connection" | "path" | "stored_at"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Some(Self {
            connection,
            path,
            stored_at,
            extra,
        })
    }

    fn to_value(&self) -> Value {
        let mut object = self.extra.clone();
        object.insert("connection".into(), json!(self.connection));
        object.insert("path".into(), json!(self.path));
        object.insert("stored_at".into(), json!(self.stored_at));
        Value::Object(object)
    }
}

/// A stored value that cannot be read: it is reported and never written
/// over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unreadable;

/// The favorites, in the order the user added them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Favorites {
    pub items: Vec<Favorite>,
    /// Entries this version cannot read, written back after the others.
    unread: Vec<Value>,
    /// Top-level keys this version does not know.
    extra: Map<String, Value>,
}

impl Favorites {
    /// Reads a stored value. A missing value is no favorites; an entry that
    /// cannot be read is left out of the list but kept for writing, so an
    /// older or newer version's entries are not lost. `Err` when the value
    /// itself cannot be read: the caller reports it and never writes over
    /// it.
    pub fn from_value(value: Option<Value>) -> Result<Self, Unreadable> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let object = value.as_object().ok_or(Unreadable)?;
        let list = match &value["favorites"] {
            Value::Null => &Vec::new(),
            Value::Array(list) => list,
            _ => return Err(Unreadable),
        };
        let mut items = Vec::new();
        let mut unread = Vec::new();
        for entry in list {
            match Favorite::from_value(entry) {
                Some(favorite)
                    if !items.iter().any(|kept: &Favorite| {
                        kept.same(favorite.connection, favorite.path.as_ref())
                    }) =>
                {
                    items.push(favorite)
                }
                Some(_) => {}
                None => unread.push(entry.clone()),
            }
        }
        let extra = object
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "version" | "favorites"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Ok(Self {
            items,
            unread,
            extra,
        })
    }

    pub fn to_value(&self) -> Value {
        let mut object = self.extra.clone();
        object.insert("version".into(), json!(1));
        object.insert(
            "favorites".into(),
            Value::Array(
                self.items
                    .iter()
                    .map(Favorite::to_value)
                    .chain(self.unread.iter().cloned())
                    .collect(),
            ),
        );
        Value::Object(object)
    }

    pub fn contains(&self, connection: ConnectionId, path: Option<&ObjectPath>) -> bool {
        self.items.iter().any(|item| item.same(connection, path))
    }

    /// Adds a favorite at the end; one already there is not added twice.
    /// Returns whether the list changed.
    pub fn add(&mut self, favorite: Favorite) -> bool {
        if self.contains(favorite.connection, favorite.path.as_ref()) {
            return false;
        }
        self.items.push(favorite);
        true
    }

    /// Removes the favorite of `connection` and `path`. Returns whether it
    /// was there.
    pub fn remove(&mut self, connection: ConnectionId, path: Option<&ObjectPath>) -> bool {
        let before = self.items.len();
        self.items.retain(|item| !item.same(connection, path));
        self.items.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::PathSegment;

    fn table(schema: &str, name: &str) -> ObjectPath {
        ObjectPath(vec![
            PathSegment {
                kind: ObjectKind::Schema,
                name: schema.into(),
            },
            PathSegment {
                kind: ObjectKind::Table,
                name: name.into(),
            },
        ])
    }

    #[test]
    fn favorites_round_trip_by_identity_and_structural_path() {
        let id = ConnectionId::new();
        let mut favorites = Favorites::default();
        assert!(favorites.add(Favorite::connection(id)));
        // A name with dots and slashes stays one segment.
        let odd = table("sales.eu", "orders/2026");
        assert!(favorites.add(Favorite::object(id, odd.clone(), Some(odd.clone()))));
        assert!(!favorites.add(Favorite::object(id, odd.clone(), None)));
        assert!(favorites.contains(id, Some(&odd)));
        assert!(favorites.contains(id, None));
        let read = Favorites::from_value(Some(favorites.to_value())).unwrap();
        assert_eq!(read, favorites);
        assert_eq!(read.items[1].kind(), Some(ObjectKind::Table));
        // `stored_at` equal to the path is not stored twice.
        assert_eq!(read.items[1].stored_at, None);
        assert!(favorites.remove(id, Some(&odd)));
        assert!(!favorites.remove(id, Some(&odd)));
        assert_eq!(favorites.items.len(), 1);
    }

    #[test]
    fn unknown_or_unreadable_entries_are_kept_and_broken_values_refused() {
        let id = ConnectionId::new();
        let stored = json!({
            "version": 1,
            "future": true,
            "favorites": [
                {"connection": id, "path": null, "pinned": 3},
                {"connection": "not an id", "path": null},
                {"connection": id, "path": [{"kind": "column", "name": "x"}]},
                {"connection": id, "path": null},
            ],
        });
        let read = Favorites::from_value(Some(stored)).unwrap();
        // The repeated connection is listed once; the two unreadable
        // entries are not listed but survive a write.
        assert_eq!(read.items.len(), 1);
        let written = read.to_value();
        assert_eq!(written["future"], json!(true));
        assert_eq!(written["favorites"][0]["pinned"], json!(3));
        assert_eq!(written["favorites"].as_array().map(Vec::len), Some(3));
        assert_eq!(written["favorites"][1]["connection"], json!("not an id"));
        // Read again, they are still kept and still not listed.
        let again = Favorites::from_value(Some(written.clone())).unwrap();
        assert_eq!(again.items.len(), 1);
        assert_eq!(again.to_value(), written);
        assert_eq!(Favorites::from_value(None).unwrap(), Favorites::default());
        assert!(Favorites::from_value(Some(json!("broken"))).is_err());
        assert!(Favorites::from_value(Some(json!({"favorites": 3}))).is_err());
    }
}
