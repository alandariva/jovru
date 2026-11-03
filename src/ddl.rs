//! Statements that change what the explorer shows (`CREATE`, `ALTER`,
//! `DROP`, `RENAME`, `COMMENT`) and the namespaces whose cached metadata
//! they leave stale. Read from the tokenizer; no window and no server.
//!
//! Only the target is identified, never the statement's meaning: a missed
//! statement leaves the cache as it was, which a refresh fixes, so anything
//! unclear is ignored rather than guessed.
use crate::drivers::{ObjectKind, ObjectPath, SessionContext, SqlDialect};
use crate::sql::{TokenKind, tokens};

/// What a statement changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// An object inside a namespace (table, view, routine, sequence, type, or
    /// the table of an index or trigger), named as written, qualified or not.
    Object(Vec<Name>),
    /// A namespace itself (`CREATE SCHEMA`, `DROP DATABASE`), with the level
    /// it names.
    Namespace(NamespaceWord, Vec<Name>),
}

/// The word a namespace statement used; dialects disagree on what it means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamespaceWord {
    Schema,
    Database,
    Catalog,
}

/// One part of a qualified name, unquoted, and whether it was quoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Name {
    pub text: String,
    pub quoted: bool,
}

/// A token reduced to what the classification needs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Part {
    /// An unquoted word, upper-cased for comparison, with its text.
    Word(String, String),
    /// A quoted identifier, unquoted.
    Quoted(String),
    Symbol(String),
    Other,
}

pub(crate) fn parts(sql: &str) -> Vec<Part> {
    tokens(sql)
        .into_iter()
        .filter(|(_, kind)| *kind != TokenKind::Comment)
        .map(|(range, kind)| {
            let text = &sql[range];
            match kind {
                TokenKind::Word | TokenKind::Keyword | TokenKind::Function => {
                    Part::Word(text.to_ascii_uppercase(), text.to_owned())
                }
                TokenKind::String if text.starts_with(['"', '`']) => {
                    let quote = &text[..1];
                    let inner = text
                        .strip_prefix(quote)
                        .map(|rest| rest.strip_suffix(quote).unwrap_or(rest))
                        .unwrap_or(text);
                    Part::Quoted(inner.replace(&format!("{quote}{quote}"), quote))
                }
                TokenKind::Symbol => Part::Symbol(text.to_owned()),
                _ => Part::Other,
            }
        })
        .collect()
}

/// Words that may sit between `CREATE` and the kind of object.
const MODIFIERS: &[&str] = &[
    "OR",
    "REPLACE",
    "TEMP",
    "TEMPORARY",
    "GLOBAL",
    "LOCAL",
    "UNLOGGED",
    "EXTERNAL",
    "MATERIALIZED",
    "RECURSIVE",
    "UNIQUE",
    "SECURE",
    "STREAMING",
    "LIVE",
    "CONSTRAINT",
    "AGGREGATE",
];

/// Kinds of object inside a namespace whose change can show in the explorer.
const OBJECTS: &[&str] = &[
    "TABLE",
    "VIEW",
    "SEQUENCE",
    "FUNCTION",
    "PROCEDURE",
    "TYPE",
    "DOMAIN",
];

/// Reads the name starting at `at`: words or quoted identifiers joined by
/// dots. Returns it and the position after it.
pub(crate) fn name_at(parts: &[Part], mut at: usize) -> Option<(Vec<Name>, usize)> {
    let mut name = Vec::new();
    loop {
        match parts.get(at)? {
            Part::Word(_, text) => name.push(Name {
                text: text.clone(),
                quoted: false,
            }),
            Part::Quoted(text) => name.push(Name {
                text: text.clone(),
                quoted: true,
            }),
            _ => return None,
        }
        at += 1;
        if parts.get(at) == Some(&Part::Symbol(".".into())) {
            at += 1;
        } else {
            return Some((name, at));
        }
    }
}

fn word(parts: &[Part], at: usize) -> Option<&str> {
    match parts.get(at) {
        Some(Part::Word(upper, _)) => Some(upper),
        _ => None,
    }
}

/// Skips `IF [NOT] EXISTS`, `ONLY` and `CONCURRENTLY` from `at`.
fn skip_guards(parts: &[Part], mut at: usize) -> usize {
    loop {
        match word(parts, at) {
            Some("IF") => {
                at += 1;
                if word(parts, at) == Some("NOT") {
                    at += 1;
                }
                if word(parts, at) == Some("EXISTS") {
                    at += 1;
                }
            }
            Some("ONLY" | "CONCURRENTLY") => at += 1,
            _ => return at,
        }
    }
}

fn namespace_word(word: &str) -> Option<NamespaceWord> {
    match word {
        "SCHEMA" => Some(NamespaceWord::Schema),
        "DATABASE" => Some(NamespaceWord::Database),
        "CATALOG" => Some(NamespaceWord::Catalog),
        _ => None,
    }
}

/// What one statement changed; empty when it changes nothing the explorer
/// shows or cannot be read with confidence.
pub fn targets(sql: &str) -> Vec<Target> {
    let parts = parts(sql);
    let Some(verb) = word(&parts, 0) else {
        return Vec::new();
    };
    match verb {
        "CREATE" | "ALTER" | "DROP" => object_targets(&parts, verb == "DROP"),
        "COMMENT" => comment_targets(&parts),
        // MySQL: RENAME TABLE a TO b, c TO d
        "RENAME" if word(&parts, 1) == Some("TABLE") => {
            let mut found = Vec::new();
            let mut at = 2;
            while let Some((name, next)) = name_at(&parts, at) {
                found.push(Target::Object(name));
                at = next;
                match parts.get(at) {
                    Some(Part::Word(upper, _)) if upper == "TO" => at += 1,
                    Some(Part::Symbol(symbol)) if symbol == "," => at += 1,
                    _ => break,
                }
            }
            found
        }
        _ => Vec::new(),
    }
}

fn object_targets(parts: &[Part], drop: bool) -> Vec<Target> {
    // The kind of object comes within the first words (after modifiers such
    // as OR REPLACE, TEMPORARY or MATERIALIZED).
    let mut at = 1;
    while word(parts, at).is_some_and(|word| MODIFIERS.contains(&word)) {
        at += 1;
    }
    let Some(kind) = word(parts, at) else {
        return Vec::new();
    };
    at += 1;
    let mut found = Vec::new();
    if let Some(namespace) = namespace_word(kind) {
        at = skip_guards(parts, at);
        while let Some((name, next)) = name_at(parts, at) {
            found.push(Target::Namespace(namespace, name));
            at = next;
            if drop && parts.get(at) == Some(&Part::Symbol(",".into())) {
                at += 1;
                continue;
            }
            // ALTER SCHEMA a RENAME TO b also creates b.
            if word(parts, at) == Some("RENAME")
                && word(parts, at + 1) == Some("TO")
                && let Some((renamed, _)) = name_at(parts, at + 2)
            {
                found.push(Target::Namespace(namespace, renamed));
            }
            break;
        }
        return found;
    }
    if kind == "INDEX" || kind == "TRIGGER" {
        // The table the index or trigger belongs to is what changes; a
        // DROP INDEX names only the index, inside its table's namespace.
        if drop {
            at = skip_guards(parts, at);
            if let Some((name, _)) = name_at(parts, at) {
                found.push(Target::Object(name));
            }
            return found;
        }
        let Some(on) = (at..parts.len()).find(|index| word(parts, *index) == Some("ON")) else {
            return found;
        };
        if let Some((name, _)) = name_at(parts, skip_guards(parts, on + 1)) {
            found.push(Target::Object(name));
        }
        return found;
    }
    if !OBJECTS.contains(&kind) {
        return found;
    }
    at = skip_guards(parts, at);
    while let Some((name, next)) = name_at(parts, at) {
        found.push(Target::Object(name));
        at = next;
        if drop && parts.get(at) == Some(&Part::Symbol(",".into())) {
            at += 1;
            continue;
        }
        break;
    }
    // ALTER TABLE a SET SCHEMA b, ALTER TABLE a RENAME TO b.c
    for index in at..parts.len() {
        match (word(parts, index), word(parts, index + 1)) {
            (Some("SET"), Some("SCHEMA")) => {
                if let Some((schema, _)) = name_at(parts, index + 2) {
                    found.push(Target::Namespace(NamespaceWord::Schema, schema));
                }
            }
            (Some("RENAME"), Some("TO")) => {
                if let Some((renamed, _)) = name_at(parts, index + 2) {
                    found.push(Target::Object(renamed));
                }
            }
            _ => {}
        }
    }
    found
}

fn comment_targets(parts: &[Part]) -> Vec<Target> {
    if word(parts, 1) != Some("ON") {
        return Vec::new();
    }
    let mut at = 2;
    if word(parts, at) == Some("MATERIALIZED") {
        at += 1;
    }
    let Some(kind) = word(parts, at) else {
        return Vec::new();
    };
    let Some((mut name, _)) = name_at(parts, at + 1) else {
        return Vec::new();
    };
    match kind {
        "COLUMN" if name.len() >= 2 => {
            name.pop();
            vec![Target::Object(name)]
        }
        _ if OBJECTS.contains(&kind) => vec![Target::Object(name)],
        _ => match namespace_word(kind) {
            Some(namespace) => vec![Target::Namespace(namespace, name)],
            None => Vec::new(),
        },
    }
}

/// How the server stores a name: quoted as written; unquoted folded the way
/// the dialect folds it (PostgreSQL and Unity Catalog to lower case, MySQL as
/// written).
pub(crate) fn stored(name: &Name, dialect: SqlDialect) -> String {
    if name.quoted {
        return name.text.clone();
    }
    match dialect {
        SqlDialect::MySql => name.text.clone(),
        _ => name.text.to_lowercase(),
    }
}

/// Whether a namespace word names the last namespace level.
fn names_level(word: NamespaceWord, level: ObjectKind, dialect: SqlDialect) -> bool {
    match word {
        // A MySQL schema is a database; a Databricks database is a schema.
        NamespaceWord::Schema => matches!(level, ObjectKind::Schema | ObjectKind::Database),
        NamespaceWord::Database => {
            level == ObjectKind::Database
                || (level == ObjectKind::Schema && dialect == SqlDialect::DatabricksSql)
        }
        NamespaceWord::Catalog => level == ObjectKind::Catalog,
    }
}

/// The last-level namespaces the statements in `sql` made stale, for a
/// connection whose namespace levels are `levels`. Unqualified names resolve
/// in each of `contexts` (the tab's context before and after the run), so a
/// `USE` in the same script does not hide where an object went.
pub fn stale_namespaces(
    statements: &[String],
    levels: &[ObjectKind],
    dialect: SqlDialect,
    contexts: &[SessionContext],
) -> Vec<ObjectPath> {
    let Some(last) = levels.last().copied() else {
        return Vec::new();
    };
    let mut found: Vec<ObjectPath> = Vec::new();
    for sql in statements {
        for target in targets(sql) {
            // An object's namespaces are its qualifier; a namespace is its own.
            let names = match &target {
                Target::Object(name) => &name[..name.len().saturating_sub(1)],
                Target::Namespace(word, name) if names_level(*word, last, dialect) => &name[..],
                Target::Namespace(..) => continue,
            };
            let names: Vec<String> = names.iter().map(|name| stored(name, dialect)).collect();
            for context in contexts {
                let filled = [context.catalog.clone(), context.schema.clone()];
                // The context's levels, aligned with the connection's: one
                // level is the schema, two are the catalog and the schema.
                let known = &filled[filled.len().saturating_sub(levels.len())..];
                let written = names.len().min(levels.len());
                // Levels the statement did not write come from the context.
                let missing = levels.len() - written;
                let mut path_names: Vec<String> = Vec::with_capacity(levels.len());
                let mut complete = true;
                for value in &known[..missing] {
                    match value {
                        Some(value) => path_names.push(value.clone()),
                        None => complete = false,
                    }
                }
                path_names.extend(names[names.len() - written..].iter().cloned());
                if !complete || path_names.len() != levels.len() {
                    continue;
                }
                let path = levels
                    .iter()
                    .zip(path_names)
                    .fold(ObjectPath::default(), |path, (kind, name)| {
                        path.child(*kind, name)
                    });
                if !found.contains(&path) {
                    found.push(path);
                }
            }
        }
    }
    found
}

/// The session context a last-level namespace stands for: with one level
/// (a MySQL database) its name is the schema; with two (a PostgreSQL
/// database and schema, a Databricks catalog and schema) they are the
/// catalog and the schema.
pub fn namespace_context(path: &ObjectPath) -> SessionContext {
    let mut names = path.0.iter().map(|part| part.name.clone());
    match path.0.len() {
        1 => SessionContext {
            catalog: None,
            schema: names.next(),
        },
        _ => SessionContext {
            catalog: names.next(),
            schema: names.next(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(target: &Target) -> Vec<String> {
        let (Target::Object(name) | Target::Namespace(_, name)) = target;
        name.iter().map(|part| part.text.clone()).collect()
    }

    #[test]
    fn statements_name_their_targets_and_others_name_nothing() {
        let table = targets("CREATE TABLE IF NOT EXISTS sales.orders (id int)");
        assert_eq!(names(&table[0]), ["sales", "orders"]);
        let view = targets(
            "create or replace materialized view \"Sales\".\"Big \"\"one\"\"\" as select 1",
        );
        assert_eq!(
            view[0],
            Target::Object(vec![
                Name {
                    text: "Sales".into(),
                    quoted: true
                },
                Name {
                    text: "Big \"one\"".into(),
                    quoted: true
                },
            ])
        );
        let dropped = targets("DROP TABLE IF EXISTS a.x, b.y CASCADE");
        assert_eq!(dropped.len(), 2);
        assert_eq!(names(&dropped[1]), ["b", "y"]);
        let index = targets("CREATE UNIQUE INDEX CONCURRENTLY idx ON ONLY crm.customers (email)");
        assert_eq!(names(&index[0]), ["crm", "customers"]);
        let moved = targets("ALTER TABLE public.t SET SCHEMA archive");
        assert_eq!(moved.len(), 2);
        assert_eq!(
            moved[1],
            Target::Namespace(
                NamespaceWord::Schema,
                vec![Name {
                    text: "archive".into(),
                    quoted: false
                }]
            )
        );
        let renamed = targets("RENAME TABLE `a`.t TO b.t");
        assert_eq!(renamed.len(), 2);
        let comment = targets("COMMENT ON COLUMN sales.orders.total IS 'x'");
        assert_eq!(names(&comment[0]), ["sales", "orders"]);
        let schema = targets("-- a new one\nCREATE SCHEMA IF NOT EXISTS reporting");
        assert_eq!(
            schema[0],
            Target::Namespace(
                NamespaceWord::Schema,
                vec![Name {
                    text: "reporting".into(),
                    quoted: false
                }]
            )
        );
        for nothing in [
            "SELECT * FROM t",
            "INSERT INTO t VALUES (1)",
            "CREATE USER bob",
            "ALTER SYSTEM SET work_mem = '1MB'",
            "",
        ] {
            assert!(targets(nothing).is_empty(), "{nothing}");
        }
    }

    #[test]
    fn a_namespace_maps_to_the_context_that_selects_it() {
        let mysql = ObjectPath::default().child(ObjectKind::Database, "shop");
        assert_eq!(
            namespace_context(&mysql),
            SessionContext {
                catalog: None,
                schema: Some("shop".into())
            }
        );
        let postgres = ObjectPath::default()
            .child(ObjectKind::Database, "shop")
            .child(ObjectKind::Schema, "sales");
        let context = namespace_context(&postgres);
        assert_eq!(context.catalog.as_deref(), Some("shop"));
        assert_eq!(context.schema.as_deref(), Some("sales"));
        // The mapping is what stale_namespaces reads back.
        assert_eq!(
            stale_namespaces(
                &["CREATE TABLE t (id int)".into()],
                &[ObjectKind::Database, ObjectKind::Schema],
                SqlDialect::Postgres,
                &[context]
            ),
            [postgres]
        );
    }

    #[test]
    fn stale_namespaces_resolve_unqualified_names_in_each_context() {
        let postgres = [ObjectKind::Database, ObjectKind::Schema];
        let before = SessionContext {
            catalog: Some("shop".into()),
            schema: Some("public".into()),
        };
        let after = SessionContext {
            catalog: Some("shop".into()),
            schema: Some("sales".into()),
        };
        let schema = |name: &str| {
            ObjectPath::default()
                .child(ObjectKind::Database, "shop")
                .child(ObjectKind::Schema, name)
        };
        let found = stale_namespaces(
            &[
                "CREATE TABLE Orders (id int)".into(),
                "DROP VIEW Crm.v".into(),
            ],
            &postgres,
            SqlDialect::Postgres,
            &[before.clone(), after.clone()],
        );
        // Unqualified: both contexts; qualified: folded to lower case.
        assert_eq!(found, [schema("public"), schema("sales"), schema("crm")]);
        // A new schema sits in the context's database.
        assert_eq!(
            stale_namespaces(
                &["CREATE SCHEMA \"Big\"".into()],
                &postgres,
                SqlDialect::Postgres,
                std::slice::from_ref(&before)
            ),
            [schema("Big")]
        );
        // A PostgreSQL database is not the last level: nothing to read.
        assert!(
            stale_namespaces(
                &["CREATE DATABASE other".into()],
                &postgres,
                SqlDialect::Postgres,
                &[before]
            )
            .is_empty()
        );
        // MySQL: one level, the database, kept as written.
        let mysql = [ObjectKind::Database];
        let context = SessionContext {
            catalog: None,
            schema: Some("Shop".into()),
        };
        assert_eq!(
            stale_namespaces(
                &["create table T (id int)".into(), "CREATE SCHEMA Two".into()],
                &mysql,
                SqlDialect::MySql,
                &[context]
            ),
            [
                ObjectPath::default().child(ObjectKind::Database, "Shop"),
                ObjectPath::default().child(ObjectKind::Database, "Two")
            ]
        );
        // Databricks: a database is a schema; no context, no guess.
        let databricks = [ObjectKind::Catalog, ObjectKind::Schema];
        assert_eq!(
            stale_namespaces(
                &[
                    "CREATE DATABASE main.Raw".into(),
                    "CREATE TABLE t (id int)".into()
                ],
                &databricks,
                SqlDialect::DatabricksSql,
                &[SessionContext::default()]
            ),
            [ObjectPath::default()
                .child(ObjectKind::Catalog, "main")
                .child(ObjectKind::Schema, "raw")]
        );
    }
}
