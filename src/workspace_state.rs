//! What the workspace keeps between sessions: the open query tabs, the saved
//! queries and the history.
//!
//! The state is stored as one JSON value in `config.db` settings
//! ([Persistence](../docs/persistence.md)). Reading tolerates missing or
//! unknown fields, so a file written by another version still opens.
use crate::drivers::ObjectPath;
use crate::parameters::{KindChoice, TabParameter};
use jykre_core::is_parameter_name;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};

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
    /// The identity the next saved query gets.
    pub next_id: u64,
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
    /// The saved query the tab edits, by its identity.
    pub saved_id: Option<u64>,
    /// Statement timeout in seconds; `0` waits as long as it takes.
    pub timeout_seconds: Option<u64>,
    /// The SQL the tab opened with (a template or a table preview), which
    /// closing the tab does not lose. `None` for tabs from older versions.
    pub opened_sql: Option<String>,
    /// Where unqualified names resolved in the tab (its catalog and schema),
    /// applied again to the session the next run opens.
    pub catalog: Option<String>,
    pub schema: Option<String>,
    /// The table or view the tab was opened for, so opening it again shows
    /// this tab instead of a copy.
    pub object: Option<ObjectPath>,
    /// What the tab's `:name` fields hold, kept with the tab (ADR-0026).
    pub parameters: Vec<TabParameter>,
    /// The statement the tab reads its table's rows with, when it was opened
    /// as the table's Data view; its result tab is called Data. `None` for
    /// other tabs and tabs from older versions.
    pub data_sql: Option<String>,
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

    /// Closing the tab would lose SQL the user wrote: it is not blank, not
    /// the SQL the tab opened with and not the version saved in the library.
    pub fn has_unsaved_sql(&self) -> bool {
        has_unsaved_sql(
            &self.sql,
            self.saved_sql.as_deref(),
            self.opened_sql.as_deref(),
        )
    }
}

/// See [`TabState::has_unsaved_sql`].
pub fn has_unsaved_sql(sql: &str, saved: Option<&str>, opened: Option<&str>) -> bool {
    let same = |other: Option<&str>| other.is_some_and(|other| other.trim() == sql.trim());
    !sql.trim().is_empty() && !same(saved) && !same(opened)
}

/// How a statement in the history ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

/// A saved query or a history entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryEntry {
    /// The identity of a saved query: its title is only a label, so two
    /// saved queries never replace each other. `0` in the history.
    pub id: u64,
    pub title: String,
    pub connection: String,
    pub sql: String,
    /// History: when the statement ended, in seconds since the Unix epoch.
    pub at: Option<u64>,
    /// History: how it ended.
    pub status: Option<RunStatus>,
    /// History: the outcome in words (rows, rows affected, the error).
    pub detail: Option<String>,
    /// History: how long it ran.
    pub elapsed_ms: Option<u64>,
    /// Saved query: the labels the user gave it, as [`parse_tags`] keeps
    /// them. Empty in the history.
    pub tags: Vec<String>,
}

/// The tags typed in a field, separated by commas: trimmed, without a
/// leading `#`, without blanks and without repeats (ignoring case), in the
/// order typed.
pub fn parse_tags(text: &str) -> Vec<String> {
    clean_tags(text.split(','))
}

/// See [`parse_tags`]; also cleans tags read from a stored state.
fn clean_tags<'a>(raw: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in raw {
        let tag = tag.trim().trim_start_matches('#').trim();
        if !tag.is_empty() && !tags.iter().any(|kept| kept.eq_ignore_ascii_case(tag)) {
            tags.push(tag.to_owned());
        }
    }
    tags
}

/// Every tag of the saved queries, once each (ignoring case), in
/// alphabetical order.
pub fn all_tags(saved: &[QueryEntry]) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in saved.iter().flat_map(|entry| &entry.tags) {
        if !tags.iter().any(|kept| kept.eq_ignore_ascii_case(tag)) {
            tags.push(tag.clone());
        }
    }
    tags.sort_by_key(|tag| tag.to_lowercase());
    tags
}

/// The words of a search that are not `#tag` filters, for searches that
/// know nothing of tags (the explorer's metadata).
pub fn without_tag_filters(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|word| !word.starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ")
}

impl QueryEntry {
    /// A history entry for a statement that ended now.
    pub fn ran(
        title: &str,
        connection: &str,
        sql: &str,
        status: RunStatus,
        detail: Option<String>,
        elapsed_ms: Option<u64>,
    ) -> Self {
        Self {
            id: 0,
            title: title.to_owned(),
            connection: connection.to_owned(),
            sql: sql.to_owned(),
            at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .map(|elapsed| elapsed.as_secs()),
            status: Some(status),
            detail,
            elapsed_ms,
            tags: Vec::new(),
        }
    }

    /// When the entry was recorded, if it says.
    pub fn time(&self) -> Option<SystemTime> {
        self.at
            .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
    }

    /// The start of the SQL on one line, for a list: its first line that is
    /// not blank, cut at `limit` characters, with `…` when there is more.
    pub fn preview(&self, limit: usize) -> String {
        let mut lines = self
            .sql
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty());
        let line = lines.next().unwrap_or_default();
        let mut preview: String = line.chars().take(limit).collect();
        if line.chars().nth(limit).is_some() || lines.next().is_some() {
            preview.push('…');
        }
        preview
    }

    /// Whether the entry matches a search, ignoring case (`query` is already
    /// lower case). Each `#word` asks for a tag that starts with `word`; the
    /// rest of the search must be in its SQL, title, connection or tags. An
    /// empty search matches everything.
    pub fn matches(&self, query: &str) -> bool {
        let tags: Vec<String> = self.tags.iter().map(|tag| tag.to_lowercase()).collect();
        let tagged = query
            .split_whitespace()
            .filter_map(|word| word.strip_prefix('#'))
            // A `#` alone is a tag still being typed.
            .filter(|wanted| !wanted.is_empty())
            .all(|wanted| tags.iter().any(|tag| tag.starts_with(wanted)));
        let text = without_tag_filters(query);
        tagged
            && (text.is_empty()
                || [&self.sql, &self.title, &self.connection]
                    .into_iter()
                    .chain(&self.tags)
                    .any(|field| field.to_lowercase().contains(&text)))
    }

    fn from_value(value: &Value) -> Self {
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
        Self {
            id: value["id"].as_u64().unwrap_or(0),
            title: text("title"),
            connection: text("connection"),
            sql: text("sql"),
            at: value["at"].as_u64(),
            status: value["status"].as_str().and_then(RunStatus::parse),
            detail: value["detail"].as_str().map(str::to_owned),
            elapsed_ms: value["elapsed_ms"].as_u64(),
            tags: clean_tags(
                value["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            ),
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "connection": self.connection,
            "sql": self.sql,
            "at": self.at,
            "status": self.status.map(RunStatus::as_str),
            "detail": self.detail,
            "elapsed_ms": self.elapsed_ms,
            "tags": self.tags,
        })
    }
}

/// A tab's parameter fields; a field without a valid name is dropped, and an
/// unknown kind reads as automatic.
fn read_parameters(value: &Value) -> Vec<TabParameter> {
    let mut parameters: Vec<TabParameter> = Vec::new();
    for item in value.as_array().into_iter().flatten() {
        let Some(name) = item["name"].as_str().filter(|name| is_parameter_name(name)) else {
            continue;
        };
        if parameters.iter().any(|known| known.name == name) {
            continue;
        }
        parameters.push(TabParameter {
            name: name.to_owned(),
            kind: KindChoice::parse(item["kind"].as_str().unwrap_or_default()),
            text: item["text"].as_str().unwrap_or_default().to_owned(),
            null: item["null"].as_bool().unwrap_or(false),
        });
    }
    parameters
}

impl WorkspaceState {
    /// Reads a stored value; anything unreadable starts from an empty state
    /// rather than failing the application, and missing fields take their
    /// defaults. Saved queries written before they had an identity get one.
    pub fn from_value(value: Option<Value>) -> Self {
        let value = value.unwrap_or(Value::Null);
        let text = |value: &Value, key: &str| value[key].as_str().unwrap_or_default().to_owned();
        let entries = |key: &str| -> Vec<QueryEntry> {
            value[key]
                .as_array()
                .into_iter()
                .flatten()
                .map(QueryEntry::from_value)
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
                saved_id: tab["saved_id"].as_u64(),
                timeout_seconds: tab["timeout_seconds"].as_u64(),
                opened_sql: tab["opened_sql"].as_str().map(str::to_owned),
                catalog: tab["catalog"].as_str().map(str::to_owned),
                schema: tab["schema"].as_str().map(str::to_owned),
                object: serde_json::from_value(tab["object"].clone()).ok(),
                parameters: read_parameters(&tab["parameters"]),
                data_sql: tab["data_sql"].as_str().map(str::to_owned),
            })
            .collect();
        let mut history = entries("history");
        history.truncate(HISTORY_LIMIT);
        let mut saved_queries = entries("saved_queries");
        let mut next_id = value["next_id"].as_u64().unwrap_or(1).max(1).max(
            saved_queries
                .iter()
                .map(|entry| entry.id.saturating_add(1))
                .max()
                .unwrap_or(1),
        );
        // A missing or repeated identity is replaced, so each saved query
        // has its own.
        let mut seen = std::collections::HashSet::new();
        for entry in &mut saved_queries {
            if entry.id == 0 || !seen.insert(entry.id) {
                entry.id = next_id;
                seen.insert(next_id);
                next_id += 1;
            }
        }
        let mut state = Self {
            active: value["active"]
                .as_u64()
                .map(|active| active as usize)
                .filter(|active| *active < tabs.len()),
            tabs,
            saved_queries,
            history,
            next_query: value["next_query"]
                .as_u64()
                .map_or(1, |n| n as usize)
                .max(1),
            next_id,
        };
        // A tab of an older version is linked to the saved query with its
        // title, connection and SQL, as it was found before identities.
        for tab in &mut state.tabs {
            if tab.saved_id.is_none()
                && let Some(saved) = &tab.saved_sql
            {
                tab.saved_id = state
                    .saved_queries
                    .iter()
                    .find(|entry| {
                        entry.title == tab.title
                            && entry.connection == tab.connection
                            && entry.sql == *saved
                    })
                    .map(|entry| entry.id);
            }
            if tab
                .saved_id
                .is_none_or(|id| !state.saved_queries.iter().any(|entry| entry.id == id))
            {
                // Its saved query is gone: the tab is a draft again.
                tab.saved_id = None;
                tab.saved_sql = None;
            }
        }
        state
    }

    pub fn to_value(&self) -> Value {
        let entries = |entries: &[QueryEntry]| -> Vec<Value> {
            entries.iter().map(QueryEntry::to_value).collect()
        };
        json!({
            "tabs": self.tabs.iter().map(|tab| json!({
                "title": tab.title,
                "connection": tab.connection,
                "sql": tab.sql,
                "saved_sql": tab.saved_sql,
                "saved_id": tab.saved_id,
                "timeout_seconds": tab.timeout_seconds,
                "opened_sql": tab.opened_sql,
                "catalog": tab.catalog,
                "schema": tab.schema,
                "object": tab.object,
                "parameters": tab.parameters.iter().map(|parameter| json!({
                    "name": parameter.name,
                    "kind": parameter.kind.as_str(),
                    "text": parameter.text,
                    "null": parameter.null,
                })).collect::<Vec<_>>(),
                "data_sql": tab.data_sql,
            })).collect::<Vec<_>>(),
            "active": self.active,
            "saved_queries": entries(&self.saved_queries),
            "history": entries(&self.history),
            "next_query": self.next_query,
            "next_id": self.next_id,
        })
    }

    /// Records a run at the top of the history, keeping its size bounded.
    pub fn record(history: &mut Vec<QueryEntry>, entry: QueryEntry) {
        history.insert(0, entry);
        history.truncate(HISTORY_LIMIT);
    }
}

/// Saves a query in the library: the one with identity `id` is updated in
/// place (its title and tags stay), otherwise a new one is added with
/// `title` and `tags`. Saving never removes anything, so saving twice keeps
/// the query. Returns the identity.
pub fn save_query(
    saved: &mut Vec<QueryEntry>,
    next_id: &mut u64,
    id: Option<u64>,
    title: &str,
    tags: &[String],
    connection: &str,
    sql: &str,
) -> u64 {
    if let Some(entry) = id.and_then(|id| saved.iter_mut().find(|entry| entry.id == id)) {
        entry.connection = connection.to_owned();
        entry.sql = sql.to_owned();
        return entry.id;
    }
    let id = (*next_id).max(1);
    *next_id = id + 1;
    saved.push(QueryEntry {
        id,
        title: title.to_owned(),
        connection: connection.to_owned(),
        sql: sql.to_owned(),
        tags: tags.to_vec(),
        ..QueryEntry::default()
    });
    id
}

/// Renames the saved query `id` and replaces its tags. `false` when it is
/// no longer in the library.
pub fn edit_saved(saved: &mut [QueryEntry], id: u64, title: &str, tags: &[String]) -> bool {
    let Some(entry) = saved.iter_mut().find(|entry| entry.id == id) else {
        return false;
    };
    entry.title = title.to_owned();
    entry.tags = tags.to_vec();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::{ObjectKind, PathSegment};

    #[test]
    fn the_state_round_trips_and_tolerates_old_or_broken_values() {
        let state = WorkspaceState {
            tabs: vec![TabState {
                title: "Query 1".into(),
                connection: "PostgreSQL · Local".into(),
                sql: "SELECT 'é';".into(),
                saved_sql: Some("SELECT 'é';".into()),
                saved_id: Some(4),
                timeout_seconds: TabState::store_timeout(None),
                opened_sql: Some("SELECT 1;".into()),
                catalog: Some("shop".into()),
                schema: Some("sales".into()),
                object: Some(ObjectPath(vec![
                    PathSegment {
                        kind: ObjectKind::Schema,
                        name: "sales".into(),
                    },
                    PathSegment {
                        kind: ObjectKind::Table,
                        name: "orders".into(),
                    },
                ])),
                parameters: vec![
                    TabParameter {
                        name: "id".into(),
                        kind: KindChoice::Auto,
                        text: "42".into(),
                        null: false,
                    },
                    TabParameter {
                        name: "day".into(),
                        kind: KindChoice::Fixed(jykre_core::ParameterKind::Date),
                        text: "kept while NULL".into(),
                        null: true,
                    },
                ],
                data_sql: Some("SELECT * FROM \"sales\".\"orders\";".into()),
            }],
            active: Some(0),
            saved_queries: vec![QueryEntry {
                id: 4,
                title: "Saved".into(),
                connection: "MySQL".into(),
                sql: "SELECT 2".into(),
                tags: vec!["finance".into(), "monthly".into()],
                ..QueryEntry::default()
            }],
            history: vec![QueryEntry::ran(
                "Query 1",
                "MySQL",
                "SELECT 3",
                RunStatus::Failed,
                Some("Syntax error".into()),
                Some(12),
            )],
            next_query: 2,
            next_id: 5,
        };
        assert_eq!(WorkspaceState::from_value(Some(state.to_value())), state);
        assert_eq!(state.tabs[0].timeout(Some(Duration::from_secs(300))), None);
        let old = serde_json::json!({"tabs": [{"title": "Old", "sql": "SELECT 1"}], "active": 7, "extra": true});
        let read = WorkspaceState::from_value(Some(old));
        assert_eq!(read.tabs[0].title, "Old");
        assert_eq!(read.tabs[0].schema, None);
        assert_eq!(read.tabs[0].object, None);
        assert_eq!(read.tabs[0].data_sql, None);
        assert_eq!(
            read.tabs[0].timeout(Some(Duration::from_secs(300))),
            Some(Duration::from_secs(300))
        );
        assert_eq!(read.active, None);
        assert_eq!(read.next_query, 1);
        assert!(read.tabs[0].parameters.is_empty());
        let fields = serde_json::json!({"tabs": [{"parameters": [
            {"name": "a", "kind": "money", "text": "1"},
            {"name": "not valid", "text": "x"},
            {"name": "a", "kind": "date"},
            "broken",
        ]}]});
        let read = WorkspaceState::from_value(Some(fields));
        assert_eq!(
            read.tabs[0].parameters,
            vec![TabParameter {
                name: "a".into(),
                kind: KindChoice::Auto,
                text: "1".into(),
                null: false,
            }]
        );
        assert_eq!(
            WorkspaceState::from_value(Some(serde_json::json!("broken"))),
            WorkspaceState {
                next_query: 1,
                next_id: 1,
                ..WorkspaceState::default()
            }
        );
        assert_eq!(WorkspaceState::from_value(None).tabs.len(), 0);
    }

    #[test]
    fn saved_queries_from_older_versions_get_their_own_identity() {
        // Two saved queries with the same title, written before identities,
        // and a tab that was saved as the second one.
        let old = serde_json::json!({
            "tabs": [{"title": "orders", "connection": "Local", "sql": "SELECT 2", "saved_sql": "SELECT 2"}],
            "saved_queries": [
                {"title": "orders", "connection": "Local", "sql": "SELECT 1"},
                {"title": "orders", "connection": "Local", "sql": "SELECT 2"},
            ],
            "history": [{"title": "Query 1", "connection": "Local", "sql": "SELECT 9"}],
        });
        let read = WorkspaceState::from_value(Some(old));
        let ids: Vec<u64> = read.saved_queries.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, [1, 2]);
        assert_eq!(read.next_id, 3);
        assert_eq!(read.tabs[0].saved_id, Some(2));
        assert_eq!(read.tabs[0].saved_sql.as_deref(), Some("SELECT 2"));
        // An old history entry reads without a time or status.
        assert_eq!(read.history[0].status, None);
        assert_eq!(read.history[0].time(), None);
        // A tab linked to a removed saved query becomes a draft.
        let orphan = serde_json::json!({
            "tabs": [{"title": "x", "sql": "SELECT 1", "saved_sql": "SELECT 1", "saved_id": 9}],
            "saved_queries": [{"id": 3, "title": "y", "sql": "SELECT 2"}, {"id": 3, "title": "z", "sql": "SELECT 3"}],
            "next_id": 2,
        });
        let read = WorkspaceState::from_value(Some(orphan));
        assert_eq!(read.tabs[0].saved_id, None);
        assert_eq!(read.tabs[0].saved_sql, None);
        // A repeated identity is replaced, and the next one is past both.
        assert_eq!(
            read.saved_queries
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [3, 4]
        );
        assert_eq!(read.next_id, 5);
    }

    #[test]
    fn saving_is_idempotent_and_never_replaces_a_query_with_the_same_title() {
        let mut saved = Vec::new();
        let mut next = 1;
        let first = save_query(
            &mut saved,
            &mut next,
            None,
            "orders",
            &[],
            "Local",
            "SELECT 1",
        );
        // Another tab with the same title is another saved query.
        let second = save_query(
            &mut saved,
            &mut next,
            None,
            "orders",
            &[],
            "Local",
            "SELECT 2",
        );
        assert_ne!(first, second);
        assert_eq!(saved.len(), 2);
        // Saving the same query again updates it, never removes it.
        assert_eq!(
            save_query(
                &mut saved,
                &mut next,
                Some(first),
                "renamed",
                &[],
                "Local",
                "SELECT 1"
            ),
            first
        );
        assert_eq!(
            save_query(
                &mut saved,
                &mut next,
                Some(first),
                "renamed",
                &[],
                "Local",
                "SELECT 10"
            ),
            first
        );
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].sql, "SELECT 10");
        assert_eq!(saved[0].title, "orders");
        assert_eq!(saved[1].sql, "SELECT 2");
        // A query removed meanwhile is saved again as a new one.
        saved.remove(0);
        let third = save_query(
            &mut saved,
            &mut next,
            Some(first),
            "orders",
            &[],
            "Local",
            "SELECT 1",
        );
        assert!(third > second);
    }

    #[test]
    fn tags_are_cleaned_listed_and_read_from_older_states() {
        assert_eq!(
            parse_tags(" #Finance, monthly ,, finance,#,  report "),
            ["Finance", "monthly", "report"]
        );
        assert!(parse_tags("  , ").is_empty());
        let tagged = |tags: &[&str]| QueryEntry {
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            ..QueryEntry::default()
        };
        assert_eq!(
            all_tags(&[tagged(&["sales", "Daily"]), tagged(&["daily", "audit"])]),
            ["audit", "Daily", "sales"]
        );
        // A saved query written before tags reads without them, and stored
        // tags are cleaned like typed ones.
        let read = WorkspaceState::from_value(Some(serde_json::json!({
            "saved_queries": [
                {"id": 1, "title": "old", "sql": "SELECT 1"},
                {"id": 2, "title": "new", "sql": "SELECT 2", "tags": ["a", " A ", "", 3, "#b"]},
            ],
        })));
        assert!(read.saved_queries[0].tags.is_empty());
        assert_eq!(read.saved_queries[1].tags, ["a", "b"]);
    }

    #[test]
    fn saved_queries_are_renamed_and_retagged_by_identity() {
        let mut saved = Vec::new();
        let mut next = 1;
        let tags = parse_tags("sales");
        let id = save_query(
            &mut saved, &mut next, None, "Query 1", &tags, "Local", "SELECT 1",
        );
        assert_eq!(saved[0].tags, ["sales"]);
        // Saving the SQL again keeps the name and the tags.
        save_query(
            &mut saved,
            &mut next,
            Some(id),
            "Query 9",
            &[],
            "Local",
            "SELECT 2",
        );
        assert_eq!(
            (saved[0].title.as_str(), &saved[0].tags[..]),
            ("Query 1", &tags[..])
        );
        assert!(edit_saved(
            &mut saved,
            id,
            "Orders by day",
            &parse_tags("daily, sales")
        ));
        assert_eq!(saved[0].title, "Orders by day");
        assert_eq!(saved[0].tags, ["daily", "sales"]);
        assert_eq!(saved[0].sql, "SELECT 2");
        assert!(!edit_saved(&mut saved, id + 1, "x", &[]));
    }

    #[test]
    fn searches_filter_by_tag_with_a_hash() {
        let entry = QueryEntry {
            title: "Revenue".into(),
            connection: "Local".into(),
            sql: "SELECT SUM(total) FROM orders".into(),
            tags: vec!["Finance".into(), "monthly".into()],
            ..QueryEntry::default()
        };
        // Tags are searched like the other fields.
        assert!(entry.matches("month"));
        // `#word` needs a tag that starts with it, every one of them.
        assert!(entry.matches("#fin"));
        assert!(entry.matches("#fin #month"));
        assert!(!entry.matches("#fin #weekly"));
        assert!(!entry.matches("#revenue"));
        // The rest of the search applies as well.
        assert!(entry.matches("#fin orders"));
        assert!(!entry.matches("#fin customers"));
        assert!(entry.matches("#"));
        assert_eq!(without_tag_filters("#fin  orders #x by"), "orders by");
    }

    #[test]
    fn history_entries_preview_their_sql_and_match_searches() {
        let entry = QueryEntry::ran(
            "Query 2",
            "PostgreSQL · Local",
            "\n  SELECT id, name\n  FROM customers;",
            RunStatus::Succeeded,
            None,
            Some(5),
        );
        assert_eq!(entry.preview(80), "SELECT id, name…");
        assert_eq!(entry.preview(6), "SELECT…");
        assert!(entry.time().is_some());
        assert!(entry.matches("customers"));
        assert!(entry.matches("postgresql"));
        assert!(entry.matches(""));
        assert!(!entry.matches("orders"));
        let short = QueryEntry {
            sql: "SELECT 1;".into(),
            ..QueryEntry::default()
        };
        assert_eq!(short.preview(80), "SELECT 1;");
    }

    #[test]
    fn only_sql_the_user_wrote_and_did_not_save_counts_as_unsaved() {
        let tab = |sql: &str, saved: Option<&str>, opened: Option<&str>| TabState {
            sql: sql.into(),
            saved_sql: saved.map(Into::into),
            opened_sql: opened.map(Into::into),
            ..TabState::default()
        };
        // The template a new tab opens with, or a blank editor, loses nothing.
        assert!(!tab("SELECT 1;", None, Some("SELECT 1;")).has_unsaved_sql());
        assert!(!tab("  \n", None, Some("SELECT 1;")).has_unsaved_sql());
        assert!(!tab("SELECT 1;\n", None, Some("SELECT 1;")).has_unsaved_sql());
        // Edited SQL is unsaved until the library has this version.
        assert!(tab("SELECT 2;", None, Some("SELECT 1;")).has_unsaved_sql());
        assert!(!tab("SELECT 2;", Some("SELECT 2;"), Some("SELECT 1;")).has_unsaved_sql());
        assert!(tab("SELECT 3;", Some("SELECT 2;"), None).has_unsaved_sql());
        // A tab from an older version has no template: its SQL counts.
        assert!(tab("SELECT 1;", None, None).has_unsaved_sql());
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
