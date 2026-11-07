//! The tools the model may call (ADR-0028). They read this computer's
//! metadata cache and nothing else: no session, no server, no result. Every
//! answer is plain text with a bounded size, and a failure is an answer
//! too, so the model can adjust instead of the turn failing.
use super::facts::SchemaScope;
use crate::backend::{self, PatternSearch};
use jykre_assistant::{ToolCall, ToolSpec};
use jykre_core::{MetadataObject, ObjectKind, ObjectPath};
use jykre_storage::SearchCursor;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

/// The longest a tool may take ([reliability](../../docs/reliability.md)).
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(10);
const SEARCH_PAGE: u32 = 100;
const MAX_COLUMNS: usize = 300;
const MAX_DEFAULT: usize = 100;
const MAX_COMMENT: usize = 200;
const MAX_DESCRIPTION: usize = 16 * 1024;

pub const SEARCH_OBJECTS: &str = "search_objects";
pub const DESCRIBE_RELATION: &str = "describe_relation";
/// Answered by the user, not the cache ([`super::released`]).
pub const RUN_QUERY: &str = "run_query";

/// The tools of one conversation, over one connection's cache.
pub struct CacheTools {
    scope: SchemaScope,
    /// Search pages already handed out; the model continues one by number.
    cursors: Mutex<Vec<SearchCursor>>,
}

impl CacheTools {
    pub fn new(scope: SchemaScope) -> Self {
        Self {
            scope,
            cursors: Mutex::new(Vec::new()),
        }
    }

    pub fn scope(&self) -> &SchemaScope {
        &self.scope
    }

    /// What the model is told about each tool; `queries` offers
    /// `run_query` too.
    pub fn specs(queries: bool) -> Vec<ToolSpec> {
        let mut specs = Self::cache_specs();
        if queries {
            specs.push(ToolSpec {
                name: RUN_QUERY.into(),
                description: "Asks the user to run ONE SQL statement on the database and \
                    share its result. The user approves or declines the statement first, then \
                    sends or withholds the result and may hide the values of any column. At \
                    most 50 rows come back, cells cut at 200 characters, 16 KiB in all. \
                    `:name` parameters are not supported. Prefer aggregates, COUNT and LIMIT; \
                    ask only when the structure is not enough."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "sql": {"type": "string", "description": "One statement, in the database's dialect."},
                        "purpose": {"type": "string", "description": "Why you need it, in one short sentence for the user."}
                    },
                    "required": ["sql", "purpose"]
                }),
            });
        }
        specs
    }

    fn cache_specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: SEARCH_OBJECTS.into(),
                description: "Finds tables, views or columns whose NAME matches a pattern, \
                    case-insensitively. `*` matches any run of characters and `?` exactly one: \
                    `*cliente*` contains 'cliente', `cliente*` starts with it, `*_id` ends with \
                    it. A pattern without wildcards means 'contains'. To list a schema, use \
                    pattern `*` with kinds [\"table\", \"view\"] and that `namespace`. Returns \
                    up to 100 dotted paths per page, with column types; pass `after` for the \
                    next page."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "kinds": {"type": "array", "items": {"type": "string", "enum": ["table", "view", "column"]}},
                        "namespace": {"type": "string", "description": "Only inside this schema or catalog.schema. Optional."},
                        "after": {"type": "string", "description": "The `next` value of the previous page. Optional."}
                    },
                    "required": ["pattern"]
                }),
            },
            ToolSpec {
                name: DESCRIBE_RELATION.into(),
                description: "Describes one table or view: its columns in order with type, \
                    nullability, primary key, default and comment, and its foreign keys. \
                    `path` is the dotted name; a partial name (just the table) works when it \
                    is unique."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            },
        ]
    }

    /// Answers one call; never fails.
    pub async fn call(&self, call: &ToolCall) -> String {
        let work = async {
            match call.name.as_str() {
                SEARCH_OBJECTS => self.search_objects(&call.arguments).await,
                DESCRIBE_RELATION => self.describe_relation(&call.arguments).await,
                other => format!("Error: there is no tool named {other}."),
            }
        };
        tokio::time::timeout(TOOL_TIMEOUT, work)
            .await
            .unwrap_or_else(|_| "Error: reading the metadata cache took too long.".into())
    }

    async fn relations(&self) -> Result<Vec<(ObjectPath, ObjectKind)>, String> {
        let scope = &self.scope;
        backend::cached_relations(&scope.dir, scope.connection, &scope.config)
            .await
            .map(|(_, relations)| relations)
            .map_err(|_| "Error: the metadata cache of this connection could not be read.".into())
    }

    async fn search_objects(&self, arguments: &Value) -> String {
        let Some(pattern) = text(arguments, "pattern").filter(|p| !p.trim().is_empty()) else {
            return "Error: `pattern` is required.".into();
        };
        let kinds: Option<Vec<ObjectKind>> =
            arguments
                .get("kinds")
                .and_then(Value::as_array)
                .map(|kinds| {
                    kinds
                        .iter()
                        .filter_map(Value::as_str)
                        .filter_map(|kind| match kind {
                            "table" => Some(vec![ObjectKind::Table]),
                            "view" => Some(vec![ObjectKind::View, ObjectKind::MaterializedView]),
                            "column" => Some(vec![ObjectKind::Column]),
                            _ => None,
                        })
                        .flatten()
                        .collect()
                });
        let namespace = match text(arguments, "namespace") {
            Some(namespace) => {
                let relations = match self.relations().await {
                    Ok(relations) => relations,
                    Err(error) => return error,
                };
                match resolve_namespace(&relations, &parse_path(namespace)) {
                    Ok(namespace) => Some(namespace),
                    Err(error) => return error,
                }
            }
            None => None,
        };
        let after = match text(arguments, "after") {
            Some(token) => match self.cursor(token) {
                Some(cursor) => Some(cursor),
                None => return "Error: `after` is not a value this search returned.".into(),
            },
            None => None,
        };
        let scope = &self.scope;
        let page = backend::search_metadata_pattern(
            &scope.dir,
            scope.connection,
            &scope.config,
            PatternSearch {
                pattern,
                kinds: kinds.as_deref().filter(|kinds| !kinds.is_empty()),
                namespace: namespace.as_ref(),
                after: after.as_ref(),
                limit: SEARCH_PAGE,
            },
        )
        .await;
        let page = match page {
            Ok(page) => page,
            Err(_) => {
                return "Error: the metadata cache of this connection could not be searched."
                    .into();
            }
        };
        let mut answer = format!("Matches for {pattern}:\n");
        if page.objects.is_empty() {
            answer.push_str("(none)\n");
        }
        for object in &page.objects {
            let _ = write!(
                answer,
                "{} ({}",
                path_text(&object.path),
                kind_name(object.kind)
            );
            if let Some(data_type) = &object.data_type {
                let _ = write!(answer, ", {data_type}");
            }
            answer.push_str(")\n");
        }
        if let Some(next) = page.next {
            let token = self.keep_cursor(next);
            let _ = writeln!(answer, "next: {token}");
        }
        answer
    }

    async fn describe_relation(&self, arguments: &Value) -> String {
        let Some(path) = text(arguments, "path").filter(|p| !p.trim().is_empty()) else {
            return "Error: `path` is required.".into();
        };
        let relations = match self.relations().await {
            Ok(relations) => relations,
            Err(error) => return error,
        };
        let context = self.context_namespace(&relations);
        let (relation, kind) =
            match resolve_relation(&relations, &parse_path(path), context.as_ref()) {
                Ok(found) => found,
                Err(error) => return error,
            };
        let scope = &self.scope;
        let children = match backend::relation_columns(
            &scope.dir,
            scope.connection,
            &scope.config,
            relation.clone(),
        )
        .await
        {
            Ok(children) => children,
            Err(_) => {
                return "Error: the metadata cache of this connection could not be read.".into();
            }
        };
        describe(&relation, kind, &children)
    }

    /// The namespace the tab's context names, when it is one the cache has.
    fn context_namespace(&self, relations: &[(ObjectPath, ObjectKind)]) -> Option<ObjectPath> {
        let context = &self.scope.context;
        let names: Vec<String> = [context.catalog.clone(), context.schema.clone()]
            .into_iter()
            .flatten()
            .collect();
        if names.is_empty() {
            return None;
        }
        resolve_namespace(relations, &names).ok()
    }

    fn keep_cursor(&self, cursor: SearchCursor) -> String {
        let mut cursors = self
            .cursors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        cursors.push(cursor);
        format!("page-{}", cursors.len())
    }

    fn cursor(&self, token: &str) -> Option<SearchCursor> {
        let index: usize = token.strip_prefix("page-")?.parse().ok()?;
        let cursors = self
            .cursors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        cursors.get(index.checked_sub(1)?).cloned()
    }
}

fn text<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Database => "database",
        ObjectKind::Catalog => "catalog",
        ObjectKind::Schema => "schema",
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        ObjectKind::Column => "column",
        ObjectKind::ForeignKey => "foreign key",
        ObjectKind::Partition => "partition",
        _ => "object",
    }
}

/// A path as the model reads and writes it: names joined by dots, a name
/// holding a dot or a double quote written in double quotes.
pub fn path_text(path: &ObjectPath) -> String {
    path.0
        .iter()
        .map(|segment| quoted_name(&segment.name))
        .collect::<Vec<_>>()
        .join(".")
}

fn quoted_name(name: &str) -> String {
    if name.contains(['.', '"']) {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name.to_owned()
    }
}

/// The names of a dotted path, reading [`path_text`]'s quoting back.
pub fn parse_path(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut name = String::new();
    let mut quoted = false;
    let mut chars = text.trim().chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                name.push('"');
            }
            '"' => quoted = !quoted,
            '.' if !quoted => names.push(std::mem::take(&mut name)),
            other => name.push(other),
        }
    }
    names.push(name);
    names
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

fn names_of(path: &ObjectPath) -> Vec<&str> {
    path.0.iter().map(|segment| segment.name.as_str()).collect()
}

/// Paths whose last names are `names`: exactly, or else ignoring case.
fn matching<'a, T: Clone>(
    candidates: impl Iterator<Item = (&'a ObjectPath, T)> + Clone,
    names: &[String],
) -> Vec<(ObjectPath, T)> {
    let tail = |path: &ObjectPath, fold: bool| {
        let path = names_of(path);
        path.len() >= names.len()
            && path[path.len() - names.len()..]
                .iter()
                .zip(names)
                .all(|(have, want)| {
                    if fold {
                        have.to_lowercase() == want.to_lowercase()
                    } else {
                        have == want
                    }
                })
    };
    let exact: Vec<_> = candidates
        .clone()
        .filter(|(path, _)| tail(path, false))
        .map(|(path, value)| (path.clone(), value))
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    candidates
        .filter(|(path, _)| tail(path, true))
        .map(|(path, value)| (path.clone(), value))
        .collect()
}

fn resolve_namespace(
    relations: &[(ObjectPath, ObjectKind)],
    names: &[String],
) -> Result<ObjectPath, String> {
    let mut parents: Vec<ObjectPath> = relations
        .iter()
        .filter_map(|(path, _)| path.parent())
        .collect();
    parents.sort_by_key(path_text);
    parents.dedup();
    let found = matching(parents.iter().map(|path| (path, ())), names);
    match found.as_slice() {
        [(path, ())] => Ok(path.clone()),
        [] => Err(format!(
            "Error: no schema named {} is in this computer's metadata cache; the user may need to refresh the explorer.",
            names.join(".")
        )),
        many => Err(ambiguous(names, many.iter().map(|(path, _)| path))),
    }
}

fn resolve_relation(
    relations: &[(ObjectPath, ObjectKind)],
    names: &[String],
    context: Option<&ObjectPath>,
) -> Result<(ObjectPath, ObjectKind), String> {
    let found = matching(relations.iter().map(|(path, kind)| (path, *kind)), names);
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!(
            "Error: {} is not in this computer's metadata cache; the user may need to refresh the explorer.",
            names.join(".")
        )),
        many => {
            // The tab's schema settles it, as an unqualified name would.
            let in_context: Vec<_> = many
                .iter()
                .filter(|(path, _)| {
                    context.is_some_and(|context| path.parent().as_ref() == Some(context))
                })
                .collect();
            match in_context.as_slice() {
                [one] => Ok((*one).clone()),
                _ => Err(ambiguous(names, many.iter().map(|(path, _)| path))),
            }
        }
    }
}

fn ambiguous<'a>(names: &[String], paths: impl Iterator<Item = &'a ObjectPath>) -> String {
    let mut answer = format!("Error: {} is ambiguous; use one of:\n", names.join("."));
    for path in paths.take(20) {
        let _ = writeln!(answer, "{}", path_text(path));
    }
    answer
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.replace(['\r', '\n'], " ");
    match flat.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &flat[..at]),
        None => flat,
    }
}

/// A relation as compact text: columns in order, keys, defaults, comments.
pub fn describe(relation: &ObjectPath, kind: ObjectKind, children: &[MetadataObject]) -> String {
    let mut answer = format!("{} ({})\n", path_text(relation), kind_name(kind));
    let columns: Vec<&MetadataObject> = children
        .iter()
        .filter(|child| child.kind == ObjectKind::Column)
        .collect();
    if columns.is_empty() {
        answer.push_str("(no columns in the cache; the user may need to refresh this table)\n");
    }
    answer.push_str("columns:\n");
    let mut primary: Vec<(u32, &str)> = Vec::new();
    for column in columns.iter().take(MAX_COLUMNS) {
        let _ = write!(answer, "  {}", quoted_name(&column.name));
        if let Some(data_type) = &column.data_type {
            let _ = write!(answer, " {data_type}");
        }
        match column.nullable {
            Some(false) => answer.push_str(" NOT NULL"),
            Some(true) => answer.push_str(" NULL"),
            None => {}
        }
        if let Some(position) = column.primary_key {
            primary.push((position, column.name.as_str()));
        }
        if let Some(default) = &column.default_value {
            let _ = write!(answer, " DEFAULT {}", clip(default, MAX_DEFAULT));
        }
        if let Some(comment) = &column.comment {
            let _ = write!(answer, " -- {}", clip(comment, MAX_COMMENT));
        }
        answer.push('\n');
        if answer.len() > MAX_DESCRIPTION {
            break;
        }
    }
    if columns.len() > MAX_COLUMNS {
        let _ = writeln!(
            answer,
            "  … {} more columns not shown",
            columns.len() - MAX_COLUMNS
        );
    }
    if !primary.is_empty() {
        primary.sort();
        let names: Vec<String> = primary.iter().map(|(_, name)| quoted_name(name)).collect();
        let _ = writeln!(answer, "primary key: ({})", names.join(", "));
    }
    let foreign: Vec<&MetadataObject> = children
        .iter()
        .filter(|child| child.kind == ObjectKind::ForeignKey)
        .collect();
    if !foreign.is_empty() {
        answer.push_str("foreign keys:\n");
        for key in foreign {
            if let Some(target) = &key.references {
                let _ = writeln!(
                    answer,
                    "  ({}) -> {} ({})",
                    target.columns.join(", "),
                    path_text(&target.relation),
                    target.referenced.join(", ")
                );
            }
        }
    }
    if answer.len() > MAX_DESCRIPTION {
        let mut end = MAX_DESCRIPTION;
        while !answer.is_char_boundary(end) {
            end -= 1;
        }
        answer.truncate(end);
        answer.push_str("\n… (description cut at 16 KiB)\n");
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use jykre_core::PathSegment;

    fn path(parts: &[(ObjectKind, &str)]) -> ObjectPath {
        ObjectPath(
            parts
                .iter()
                .map(|(kind, name)| PathSegment {
                    kind: *kind,
                    name: (*name).into(),
                })
                .collect(),
        )
    }

    #[test]
    fn dotted_paths_round_trip_with_quoted_names() {
        let tricky = path(&[
            (ObjectKind::Schema, "sales.eu"),
            (ObjectKind::Table, "say \"hi\""),
        ]);
        assert_eq!(path_text(&tricky), "\"sales.eu\".\"say \"\"hi\"\"\"");
        assert_eq!(parse_path(&path_text(&tricky)), ["sales.eu", "say \"hi\""]);
        assert_eq!(parse_path(" sales . orders "), ["sales", "orders"]);
    }

    #[test]
    fn relations_resolve_by_tail_then_case_then_context() {
        let relations = vec![
            (
                path(&[(ObjectKind::Schema, "sales"), (ObjectKind::Table, "orders")]),
                ObjectKind::Table,
            ),
            (
                path(&[
                    (ObjectKind::Schema, "archive"),
                    (ObjectKind::Table, "orders"),
                ]),
                ObjectKind::Table,
            ),
            (
                path(&[
                    (ObjectKind::Schema, "sales"),
                    (ObjectKind::Table, "Clientes"),
                ]),
                ObjectKind::Table,
            ),
        ];
        let names = |text: &str| parse_path(text);
        assert_eq!(
            path_text(
                &resolve_relation(&relations, &names("archive.orders"), None)
                    .unwrap()
                    .0
            ),
            "archive.orders"
        );
        assert_eq!(
            path_text(
                &resolve_relation(&relations, &names("clientes"), None)
                    .unwrap()
                    .0
            ),
            "sales.Clientes"
        );
        let error = resolve_relation(&relations, &names("orders"), None).unwrap_err();
        assert!(error.contains("ambiguous") && error.contains("archive.orders"));
        let sales = path(&[(ObjectKind::Schema, "sales")]);
        assert_eq!(
            path_text(
                &resolve_relation(&relations, &names("orders"), Some(&sales))
                    .unwrap()
                    .0
            ),
            "sales.orders"
        );
        assert!(
            resolve_relation(&relations, &names("missing"), None)
                .unwrap_err()
                .contains("refresh")
        );
        assert_eq!(
            path_text(&resolve_namespace(&relations, &names("SALES")).unwrap()),
            "sales"
        );
    }

    #[test]
    fn a_description_lists_columns_keys_defaults_and_comments_within_bounds() {
        let relation = path(&[(ObjectKind::Schema, "sales"), (ObjectKind::Table, "orders")]);
        let column = |name: &str, ordinal: u32| MetadataObject {
            kind: ObjectKind::Column,
            path: relation.child(ObjectKind::Column, name),
            name: name.into(),
            ordinal: Some(ordinal),
            data_type: Some("bigint".into()),
            nullable: Some(ordinal != 1),
            comment: None,
            default_value: None,
            primary_key: (ordinal == 1).then_some(1),
            references: None,
            stored_at: None,
        };
        let mut status = column("status", 2);
        status.data_type = Some("text".into());
        status.default_value = Some("'pending'::text".into());
        status.comment = Some("synthetic\ncomment".into());
        let children = vec![column("id", 1), status];
        let text = describe(&relation, ObjectKind::Table, &children);
        assert!(text.starts_with("sales.orders (table)\n"));
        assert!(text.contains("  id bigint NOT NULL\n"));
        assert!(text.contains("  status text NULL DEFAULT 'pending'::text -- synthetic comment\n"));
        assert!(text.contains("primary key: (id)"));

        let many: Vec<_> = (1..=400)
            .map(|i| column(&format!("c{i:03}_{}", "x".repeat(60)), i))
            .collect();
        let text = describe(&relation, ObjectKind::Table, &many);
        assert!(text.len() <= MAX_DESCRIPTION + 64, "{}", text.len());
    }
}
