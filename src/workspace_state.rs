//! What the workspace keeps between sessions: the open query tabs, the saved
//! queries and the history.
//!
//! The state is stored as one JSON value in `config.db` settings
//! ([Persistence](../docs/persistence.md)). Reading tolerates missing or
//! unknown fields, so a file written by another version still opens.
use serde_json::{Value, json};
use std::time::Duration;

/// The settings key the state is stored under.
pub const SETTINGS_KEY: &str = "workspace.state";
/// History entries kept, newest first.
pub const HISTORY_LIMIT: usize = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceState {
    pub tabs: Vec<TabState>,
    /// Index into `tabs` of the tab that was on screen.
    pub active: Option<usize>,
    pub saved_queries: Vec<QueryEntry>,
    /// Newest first, at most [`HISTORY_LIMIT`].
    pub history: Vec<QueryEntry>,
    /// The number the next "Query N" tab gets.
    pub next_query: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabState {
    pub title: String,
    /// The connection's name; a tab whose connection no longer exists opens
    /// without one.
    pub connection: String,
    pub sql: String,
    /// The SQL as last saved to the library, when the tab is a saved query.
    pub saved_sql: Option<String>,
    /// Statement timeout in seconds; `0` waits as long as it takes.
    pub timeout_seconds: Option<u64>,
}

impl TabState {
    /// The tab's statement timeout, or `fallback` when none was stored.
    pub fn timeout(&self, fallback: Option<Duration>) -> Option<Duration> {
        match self.timeout_seconds {
            None => fallback,
            Some(0) => None,
            Some(seconds) => Some(Duration::from_secs(seconds)),
        }
    }

    pub fn store_timeout(timeout: Option<Duration>) -> Option<u64> {
        Some(timeout.map_or(0, |timeout| timeout.as_secs().max(1)))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryEntry {
    pub title: String,
    pub connection: String,
    pub sql: String,
}

impl WorkspaceState {
    /// Reads a stored value; anything unreadable starts from an empty state
    /// rather than failing the application, and missing fields take their
    /// defaults.
    pub fn from_value(value: Option<Value>) -> Self {
        let value = value.unwrap_or(Value::Null);
        let text = |value: &Value, key: &str| value[key].as_str().unwrap_or_default().to_owned();
        let entries = |key: &str| -> Vec<QueryEntry> {
            value[key]
                .as_array()
                .into_iter()
                .flatten()
                .map(|entry| QueryEntry {
                    title: text(entry, "title"),
                    connection: text(entry, "connection"),
                    sql: text(entry, "sql"),
                })
                .collect()
        };
        let tabs: Vec<TabState> = value["tabs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tab| TabState {
                title: text(tab, "title"),
                connection: text(tab, "connection"),
                sql: text(tab, "sql"),
                saved_sql: tab["saved_sql"].as_str().map(str::to_owned),
                timeout_seconds: tab["timeout_seconds"].as_u64(),
            })
            .collect();
        let mut history = entries("history");
        history.truncate(HISTORY_LIMIT);
        Self {
            active: value["active"]
                .as_u64()
                .map(|active| active as usize)
                .filter(|active| *active < tabs.len()),
            tabs,
            saved_queries: entries("saved_queries"),
            history,
            next_query: value["next_query"]
                .as_u64()
                .map_or(1, |n| n as usize)
                .max(1),
        }
    }

    pub fn to_value(&self) -> Value {
        let entries = |entries: &[QueryEntry]| -> Vec<Value> {
            entries
                .iter()
                .map(|entry| json!({"title": entry.title, "connection": entry.connection, "sql": entry.sql}))
                .collect()
        };
        json!({
            "tabs": self.tabs.iter().map(|tab| json!({
                "title": tab.title,
                "connection": tab.connection,
                "sql": tab.sql,
                "saved_sql": tab.saved_sql,
                "timeout_seconds": tab.timeout_seconds,
            })).collect::<Vec<_>>(),
            "active": self.active,
            "saved_queries": entries(&self.saved_queries),
            "history": entries(&self.history),
            "next_query": self.next_query,
        })
    }

    /// Records a run at the top of the history, keeping its size bounded.
    pub fn record(history: &mut Vec<QueryEntry>, entry: QueryEntry) {
        history.insert(0, entry);
        history.truncate(HISTORY_LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_round_trips_and_tolerates_old_or_broken_values() {
        let state = WorkspaceState {
            tabs: vec![TabState {
                title: "Query 1".into(),
                connection: "PostgreSQL · Local".into(),
                sql: "SELECT 'é';".into(),
                saved_sql: Some("SELECT 'é';".into()),
                timeout_seconds: TabState::store_timeout(None),
            }],
            active: Some(0),
            saved_queries: vec![QueryEntry {
                title: "Saved".into(),
                connection: "MySQL".into(),
                sql: "SELECT 2".into(),
            }],
            history: Vec::new(),
            next_query: 2,
        };
        assert_eq!(WorkspaceState::from_value(Some(state.to_value())), state);
        assert_eq!(state.tabs[0].timeout(Some(Duration::from_secs(300))), None);
        let old = serde_json::json!({"tabs": [{"title": "Old", "sql": "SELECT 1"}], "active": 7, "extra": true});
        let read = WorkspaceState::from_value(Some(old));
        assert_eq!(read.tabs[0].title, "Old");
        assert_eq!(
            read.tabs[0].timeout(Some(Duration::from_secs(300))),
            Some(Duration::from_secs(300))
        );
        assert_eq!(read.active, None);
        assert_eq!(read.next_query, 1);
        assert_eq!(
            WorkspaceState::from_value(Some(serde_json::json!("broken"))),
            WorkspaceState {
                next_query: 1,
                ..WorkspaceState::default()
            }
        );
        assert_eq!(WorkspaceState::from_value(None).tabs.len(), 0);
    }

    #[test]
    fn the_history_keeps_the_newest_entries() {
        let mut history = Vec::new();
        for n in 0..HISTORY_LIMIT + 5 {
            WorkspaceState::record(
                &mut history,
                QueryEntry {
                    sql: n.to_string(),
                    ..QueryEntry::default()
                },
            );
        }
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history[0].sql, (HISTORY_LIMIT + 4).to_string());
    }
}
