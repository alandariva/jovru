#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! Which relations completion offers columns of at the cursor, over the cases
//! of `fixtures/scopes.txt` in the three dialects.
use jykre_core::{ObjectKind, ObjectPath, SessionContext, SqlDialect};
use jykre_sql_language::completion::{NameIndex, Relation, Wanted, request};
use std::collections::HashSet;

const CURSOR: char = '‸';

/// Every relation the cases read, so that one read in the wrong query
/// resolves and shows.
const RELATIONS: &[&str] = &[
    "orders",
    "customers",
    "order_lines",
    "events",
    "categories",
    "refunds",
    "audit",
    "order_totals",
    "archive_orders",
    "updates",
    "sales.orders",
    "sales.order_lines",
    "sales.daily",
    "sales.keep",
    "sales.returns",
    "crm.customers",
    "crm.blocked",
    "archive.orders",
    "staging.orders",
    "shop.orders",
    "shop.customers",
    "hr.employees",
    "\"Sales\".\"Orders\"",
];

struct Case {
    line: usize,
    dialect: String,
    /// `None` when the cursor asks for a relation name.
    relations: Option<Vec<String>>,
    sql: String,
    cursor: usize,
}

fn cases(text: &str) -> Vec<Case> {
    let mut cases: Vec<Case> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix("== ") {
            let (dialect, relations) = header.split_once('|').expect("dialect | relations");
            let relations = relations.trim();
            cases.push(Case {
                line: number + 1,
                dialect: dialect.trim().to_owned(),
                relations: (relations != "-").then(|| {
                    relations
                        .split(',')
                        .map(|name| name.trim().to_owned())
                        .collect()
                }),
                sql: String::new(),
                cursor: 0,
            });
        } else {
            let case = cases.last_mut().expect("SQL after a header");
            case.sql.push_str(line);
            case.sql.push('\n');
        }
    }
    for case in &mut cases {
        let sql = case.sql.trim_end().to_owned();
        case.cursor = sql.find(CURSOR).expect("a cursor in every case");
        case.sql = sql.replacen(CURSOR, "", 1);
    }
    cases
}

/// The dialect, the levels of its namespaces and a tab context whose
/// default schema is `def`.
fn setup(dialect: &str) -> (SqlDialect, Vec<ObjectKind>, SessionContext) {
    let context = |catalog: Option<&str>| SessionContext {
        catalog: catalog.map(str::to_owned),
        schema: Some("def".into()),
    };
    match dialect {
        "postgres" => (
            SqlDialect::Postgres,
            vec![ObjectKind::Database, ObjectKind::Schema],
            context(Some("jykre")),
        ),
        "mysql" => (SqlDialect::MySql, vec![ObjectKind::Database], context(None)),
        "databricks" => (
            SqlDialect::DatabricksSql,
            vec![ObjectKind::Catalog, ObjectKind::Schema],
            context(Some("main")),
        ),
        other => unreachable!("unknown dialect {other}"),
    }
}

/// The parts of a written name, unquoted; unquoted parts in lower case.
fn split(written: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut quoted = false;
    for ch in written.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '`' => {
                quote = Some(ch);
                quoted = true;
            }
            None if ch == '.' => {
                let part = std::mem::take(&mut current);
                parts.push(if quoted { part } else { part.to_lowercase() });
                quoted = false;
            }
            None => current.push(ch),
        }
    }
    parts.push(if quoted {
        current
    } else {
        current.to_lowercase()
    });
    parts
}

/// The path of a relation written `[[catalog.]schema.]table`, the levels it
/// leaves out taken from `context`.
fn path(written: &str, levels: &[ObjectKind], context: &SessionContext) -> ObjectPath {
    let parts = split(written);
    let (table, namespaces) = parts.split_last().expect("a name");
    let defaults = [context.catalog.clone(), context.schema.clone()];
    let defaults = &defaults[defaults.len() - levels.len()..];
    let written = namespaces.len().min(levels.len());
    let names = defaults[..levels.len() - written]
        .iter()
        .map(|name| name.clone().expect("a default namespace"))
        .chain(namespaces[namespaces.len() - written..].iter().cloned());
    levels
        .iter()
        .zip(names)
        .fold(ObjectPath::default(), |path, (kind, name)| {
            path.child(*kind, name)
        })
        .child(ObjectKind::Table, table.clone())
}

#[test]
fn completion_reads_the_relations_of_the_query_around_the_cursor() {
    let cases = cases(include_str!("fixtures/scopes.txt"));
    assert_eq!(cases.len(), 121);
    let mut failures = Vec::new();
    for case in &cases {
        let (dialect, levels, context) = setup(&case.dialect);
        let relations = RELATIONS
            .iter()
            .map(|written| Relation {
                path: path(written, &levels, &context),
                kind: ObjectKind::Table,
            })
            .collect();
        let index = NameIndex::new(1, levels.clone(), dialect, relations);
        let wanted = request(&case.sql, case.cursor, &index, &context).map(|asked| asked.wanted);
        let found: Option<HashSet<ObjectPath>> = match wanted {
            Some(Wanted::Columns { relations, .. }) => Some(relations.into_iter().collect()),
            Some(Wanted::Relations { .. }) => None,
            other => {
                failures.push(format!("line {}: asked for {other:?}", case.line));
                continue;
            }
        };
        let expected: Option<HashSet<ObjectPath>> = case.relations.as_ref().map(|names| {
            names
                .iter()
                .map(|name| path(name, &levels, &context))
                .collect()
        });
        if found != expected {
            failures.push(format!(
                "line {}: {}\n  found {found:?}\n  expected {expected:?}",
                case.line,
                case.sql.replace('\n', " ")
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
