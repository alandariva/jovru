//! Editing a Databricks result (ADR-0039, section 9): the protocol reports
//! no column's origin, so the table is inferred from the SQL text, by the
//! maintainer's decision (E-3), and only for a single-table `SELECT` of
//! plain columns. The table is resolved against Unity Catalog, and edits
//! become statements with `:name` markers for the user to review and run
//! in a new query tab: nothing is written here.
use super::{Client, Constraints, bounded};
use crate::DatabricksDriver;
use jykre_core::write::{
    EditTarget, EditableColumn, EditableTable, KeyKind, ReadOnlyReason, ScriptParameter, WriteMode,
    WriteRequest, WriteScript, WriteValue, statement,
};
use jykre_core::*;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

/// How long reading the table from Unity Catalog may take.
const CATALOG: Duration = Duration::from_secs(60);

/// A token of the statement, as far as the inference needs one.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    /// An unquoted word, as written.
    Word(String),
    /// A name in backticks, without them.
    Quoted(String),
    Symbol(char),
    /// A string, a number or a parameter marker: never part of a name.
    Other,
}

impl Token {
    fn keyword(&self, word: &str) -> bool {
        matches!(self, Self::Word(text) if text.eq_ignore_ascii_case(word))
    }

    /// The name this token spells, when it is one.
    fn name(&self) -> Option<&str> {
        match self {
            Self::Word(text) | Self::Quoted(text) => Some(text),
            _ => None,
        }
    }
}

/// The statement's tokens, without whitespace and comments; `None` when a
/// string, a name or a comment does not end.
fn tokens(sql: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = sql.chars().collect();
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(&c) = chars.get(at) {
        let next = chars.get(at + 1).copied();
        if c.is_whitespace() {
            at += 1;
        } else if c == '-' && next == Some('-') {
            while chars.get(at).is_some_and(|c| *c != '\n') {
                at += 1;
            }
        } else if c == '/' && next == Some('*') {
            at += 2;
            loop {
                match (chars.get(at), chars.get(at + 1)) {
                    (Some('*'), Some('/')) => {
                        at += 2;
                        break;
                    }
                    (Some(_), _) => at += 1,
                    (None, _) => return None,
                }
            }
        } else if c == '\'' || c == '"' {
            at += 1;
            loop {
                match chars.get(at) {
                    Some('\\') => at += 2,
                    Some(quote) if *quote == c => {
                        at += 1;
                        break;
                    }
                    Some(_) => at += 1,
                    None => return None,
                }
            }
            found.push(Token::Other);
        } else if c == '`' {
            at += 1;
            let mut name = String::new();
            loop {
                match (chars.get(at), chars.get(at + 1)) {
                    (Some('`'), Some('`')) => {
                        name.push('`');
                        at += 2;
                    }
                    (Some('`'), _) => {
                        at += 1;
                        break;
                    }
                    (Some(c), _) => {
                        name.push(*c);
                        at += 1;
                    }
                    (None, _) => return None,
                }
            }
            found.push(Token::Quoted(name));
        } else if c.is_alphabetic() || c == '_' {
            let start = at;
            while chars
                .get(at)
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                at += 1;
            }
            found.push(Token::Word(chars[start..at].iter().collect()));
        } else if c.is_ascii_digit() || c == ':' || c == '?' {
            // A number or a parameter marker (`:name`, `?`).
            at += 1;
            while chars
                .get(at)
                .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
            {
                at += 1;
            }
            found.push(Token::Other);
        } else {
            found.push(Token::Symbol(c));
            at += 1;
        }
    }
    Some(found)
}

/// A result column the `SELECT` names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    /// `*` or `t.*`.
    Star { qualifier: Option<String> },
    /// `c`, `t.c`, `` `c` ``, optionally renamed.
    Column {
        qualifier: Option<String>,
        name: String,
        alias: Option<String>,
    },
}

/// A single-table `SELECT` of plain columns.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Select {
    /// One to three parts: `[catalog.][schema.]table`.
    table: Vec<String>,
    alias: Option<String>,
    items: Vec<Item>,
}

/// Words that end the table's name and alias, or that mean something other
/// than a single-table read.
const CLAUSES: [&str; 37] = [
    "WHERE",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "GROUP",
    "HAVING",
    "QUALIFY",
    "WINDOW",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "SEMI",
    "ANTI",
    "ON",
    "USING",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "MINUS",
    "LATERAL",
    "PIVOT",
    "UNPIVOT",
    "TABLESAMPLE",
    "VERSION",
    "TIMESTAMP",
    "FOR",
    "CLUSTER",
    "DISTRIBUTE",
    "SORT",
    "WITH",
    "AS",
    "FROM",
    "SELECT",
];

/// What may follow the table at the top level: a filter, an order and a
/// limit. Anything else (a join, a set operation, grouping) is refused.
const ALLOWED_AFTER: [&str; 5] = ["WHERE", "ORDER", "BY", "LIMIT", "OFFSET"];

/// Words refused anywhere at the top level after the table.
const REFUSED: [&str; 16] = [
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "MINUS",
    "GROUP",
    "HAVING",
    "QUALIFY",
    "WINDOW",
    "JOIN",
    "LATERAL",
    "PIVOT",
    "UNPIVOT",
    "CLUSTER",
    "DISTRIBUTE",
    "SORT",
    "TABLESAMPLE",
];

fn clause(token: &Token) -> bool {
    CLAUSES.iter().any(|word| token.keyword(word))
}

/// A name of one or more dotted parts starting at `at`; the parts and where
/// it ends.
fn dotted(tokens: &[Token], mut at: usize, most: usize) -> Option<(Vec<String>, usize)> {
    let mut parts = Vec::new();
    loop {
        let token = tokens.get(at)?;
        if matches!(token, Token::Word(_)) && clause(token) {
            return None;
        }
        parts.push(token.name()?.to_owned());
        at += 1;
        if parts.len() < most && tokens.get(at) == Some(&Token::Symbol('.')) {
            at += 1;
            continue;
        }
        return Some((parts, at));
    }
}

/// One select item's tokens.
fn item(tokens: &[Token]) -> Option<Item> {
    match tokens {
        [Token::Symbol('*')] => return Some(Item::Star { qualifier: None }),
        [qualifier, Token::Symbol('.'), Token::Symbol('*')] => {
            return Some(Item::Star {
                qualifier: Some(qualifier.name()?.to_owned()),
            });
        }
        _ => {}
    }
    let (parts, at) = dotted(tokens, 0, 2)?;
    let alias = match tokens.get(at..)? {
        [] => None,
        [as_, alias] if as_.keyword("AS") => Some(alias.name()?.to_owned()),
        [alias] if !clause(alias) => Some(alias.name()?.to_owned()),
        _ => return None,
    };
    let mut parts = parts.into_iter();
    let (qualifier, name) = match (parts.next(), parts.next()) {
        (Some(name), None) => (None, name),
        (Some(qualifier), Some(name)) => (Some(qualifier), name),
        _ => return None,
    };
    Some(Item::Column {
        qualifier,
        name,
        alias,
    })
}

/// The single-table `SELECT` the statement is, or `None` for anything else
/// ([ADR-0039, section 9](../../../../docs/adr/0039-editing-result-data-through-the-driver.md)).
fn infer(sql: &str) -> Option<Select> {
    let mut tokens = tokens(sql)?;
    while tokens.last() == Some(&Token::Symbol(';')) {
        tokens.pop();
    }
    if tokens.contains(&Token::Symbol(';')) || !tokens.first()?.keyword("SELECT") {
        return None;
    }
    let mut at = 1;
    if tokens.get(at)?.keyword("DISTINCT") {
        return None;
    }
    if tokens.get(at)?.keyword("ALL") {
        at += 1;
    }
    // The select list, to the top-level FROM.
    let from = tokens
        .iter()
        .enumerate()
        .skip(at)
        .find(|(_, token)| token.keyword("FROM"))
        .map(|(index, _)| index)?;
    let list = tokens.get(at..from)?;
    if list
        .iter()
        .any(|token| matches!(token, Token::Symbol('(' | ')')))
    {
        // A function, an aggregate or a subquery.
        return None;
    }
    let items = list
        .split(|token| *token == Token::Symbol(','))
        .map(item)
        .collect::<Option<Vec<Item>>>()?;
    if items.is_empty() {
        return None;
    }
    let (table, mut at) = dotted(&tokens, from + 1, 3)?;
    let alias = match tokens.get(at) {
        Some(token) if token.keyword("AS") => {
            let alias = tokens.get(at + 1)?.name()?.to_owned();
            at += 2;
            Some(alias)
        }
        Some(token @ (Token::Word(_) | Token::Quoted(_))) if !clause(token) => {
            at += 1;
            Some(token.name()?.to_owned())
        }
        _ => None,
    };
    // Right after the table: the end, or a filter, an order or a limit.
    if let Some(next) = tokens.get(at)
        && !ALLOWED_AFTER.iter().any(|word| next.keyword(word))
    {
        return None;
    }
    let mut depth = 0usize;
    for token in &tokens[at..] {
        match token {
            Token::Symbol('(') => depth += 1,
            Token::Symbol(')') => depth = depth.checked_sub(1)?,
            _ if depth == 0 && REFUSED.iter().any(|word| token.keyword(word)) => return None,
            _ => {}
        }
    }
    let name = alias.clone().or_else(|| table.last().cloned())?;
    let qualified = |qualifier: &Option<String>| {
        qualifier
            .as_ref()
            .is_none_or(|qualifier| qualifier.eq_ignore_ascii_case(&name))
    };
    if !items.iter().all(|item| match item {
        Item::Star { qualifier } | Item::Column { qualifier, .. } => qualified(qualifier),
    }) {
        return None;
    }
    Some(Select {
        table,
        alias,
        items,
    })
}

/// The kind of `:name` parameter a column of this type binds as, and
/// whether its text identifies one value; `None` for a type no parameter
/// kind holds (binary, interval, complex types, variant).
fn parameter_kind(type_text: &str) -> Option<(ParameterKind, bool)> {
    let lowered = type_text.trim().to_lowercase();
    let base = lowered.split(['(', '<', ' ']).next().unwrap_or_default();
    Some(match base {
        "tinyint" | "byte" | "smallint" | "short" | "int" | "integer" | "bigint" | "long"
        | "decimal" | "dec" | "numeric" => (ParameterKind::Number, true),
        // Approximate numbers do not identify a value by their text.
        "float" | "real" | "double" => (ParameterKind::Number, false),
        "string" | "varchar" | "char" => (ParameterKind::Text, true),
        "boolean" => (ParameterKind::Boolean, true),
        "date" => (ParameterKind::Date, true),
        // TIMESTAMP is shown in the session's time zone, where a repeated
        // hour reads the same for two instants.
        "timestamp" => (ParameterKind::Timestamp, false),
        "timestamp_ntz" => (ParameterKind::Timestamp, true),
        _ => return None,
    })
}

/// A table column, as Unity Catalog describes it.
struct TableColumn {
    name: String,
    column: Option<EditableColumn>,
}

/// The target of a result read with `statement`, resolved against Unity
/// Catalog from the session's `catalog` and `schema`.
pub(super) async fn edit_target(
    client: &Client,
    catalog: Option<&str>,
    schema: Option<&str>,
    statement: &str,
    columns: &[ColumnInfo],
    cancel: &CancellationToken,
) -> DriverResult<EditTarget> {
    let Some(select) = infer(statement) else {
        return Ok(EditTarget::ReadOnly(ReadOnlyReason::NotASingleTableSelect));
    };
    let written = select.table.join(".");
    // Unity Catalog keeps every name in lower case.
    let mut parts: Vec<String> = select
        .table
        .iter()
        .map(|part| part.to_lowercase())
        .collect();
    let full = match parts.len() {
        3 => Some(parts.clone()),
        2 => catalog.map(|catalog| {
            let mut full = vec![catalog.to_lowercase()];
            full.append(&mut parts);
            full
        }),
        _ => catalog.zip(schema).map(|(catalog, schema)| {
            let mut full = vec![catalog.to_lowercase(), schema.to_lowercase()];
            full.append(&mut parts);
            full
        }),
    };
    let Some([catalog, schema, table]) = full.as_deref() else {
        return Ok(EditTarget::ReadOnly(ReadOnlyReason::TableNotFound {
            name: written,
        }));
    };
    let read = bounded(cancel, CATALOG, client.table(catalog, schema, table)).await;
    let described = match read {
        Ok(described) => described,
        Err(error) if error.code == ErrorCode::NotFound => {
            return Ok(EditTarget::ReadOnly(ReadOnlyReason::TableNotFound {
                name: written,
            }));
        }
        Err(error) => return Err(error),
    };
    Ok(target(catalog, schema, table, &select, &described, columns))
}

/// The target `select` reads from the table Unity Catalog `described`, for
/// a result with `columns`.
fn target(
    catalog: &str,
    schema: &str,
    table: &str,
    select: &Select,
    described: &Value,
    columns: &[ColumnInfo],
) -> EditTarget {
    let kind = described["table_type"].as_str().unwrap_or_default();
    if !matches!(kind, "MANAGED" | "EXTERNAL") {
        let kind = match kind {
            "VIEW" => "view".to_owned(),
            "MATERIALIZED_VIEW" => "materialized view".to_owned(),
            "STREAMING_TABLE" => "streaming table".to_owned(),
            other => other.to_lowercase().replace('_', " "),
        };
        return EditTarget::ReadOnly(ReadOnlyReason::NotATable { kind });
    }
    let mut listed: Vec<(u64, TableColumn)> = described["columns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|column| {
            let name = column["name"].as_str()?.to_owned();
            let type_text = column["type_text"].as_str().unwrap_or_default();
            let nullable = column["nullable"].as_bool().unwrap_or(true);
            let editable = parameter_kind(type_text).map(|(_, can_key)| EditableColumn {
                name: name.clone(),
                declared_type: type_text.to_owned(),
                nullable,
                writable: true,
                has_default: nullable || super::column_default(column).is_some(),
                binary: false,
                can_key,
            });
            Some((
                column["position"].as_u64().unwrap_or(u64::MAX),
                TableColumn {
                    name,
                    column: editable,
                },
            ))
        })
        .collect();
    listed.sort_by_key(|(position, _)| *position);
    let table_columns: Vec<TableColumn> = listed.into_iter().map(|(_, column)| column).collect();
    // The result's columns, in order, as the SQL names them.
    let mut expected: Vec<(String, Option<EditableColumn>)> = Vec::new();
    for item in &select.items {
        match item {
            Item::Star { .. } => expected.extend(
                table_columns
                    .iter()
                    .map(|column| (column.name.clone(), column.column.clone())),
            ),
            Item::Column { name, alias, .. } => {
                let Some(found) = table_columns
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(name))
                else {
                    return EditTarget::ReadOnly(ReadOnlyReason::ColumnsDoNotMatch);
                };
                let shown = alias.clone().unwrap_or_else(|| found.name.clone());
                expected.push((shown, found.column.clone()));
            }
        }
    }
    if expected.len() != columns.len()
        || expected
            .iter()
            .zip(columns)
            .any(|((name, _), column)| !name.eq_ignore_ascii_case(&column.name))
    {
        return EditTarget::ReadOnly(ReadOnlyReason::ColumnsDoNotMatch);
    }
    let editable: Vec<Option<EditableColumn>> =
        expected.into_iter().map(|(_, column)| column).collect();
    let declared = Constraints::read(&described["table_constraints"]).primary_key;
    let position = |name: &String| {
        editable.iter().position(|column| {
            column
                .as_ref()
                .is_some_and(|column| column.name.eq_ignore_ascii_case(name))
        })
    };
    let key: Option<Vec<usize>> = (!declared.is_empty())
        .then(|| {
            declared
                .iter()
                .map(position)
                .collect::<Option<Vec<usize>>>()
        })
        .flatten();
    let (key, key_kind) = match key {
        Some(key) => (key, KeyKind::Informational),
        // The user names the key columns before anything is generated.
        None => (Vec::new(), KeyKind::Chosen),
    };
    if let Some(column) = key
        .iter()
        .filter_map(|position| editable.get(*position).and_then(Option::as_ref))
        .find(|column| !column.can_key)
    {
        return EditTarget::ReadOnly(ReadOnlyReason::KeyType {
            column: column.name.clone(),
        });
    }
    EditTarget::Editable(EditableTable {
        table: ObjectPath::default()
            .child(ObjectKind::Catalog, catalog)
            .child(ObjectKind::Schema, schema)
            .child(ObjectKind::Table, table),
        writes: WriteMode::Script,
        columns: editable,
        key,
        key_kind,
        side_effects: Vec::new(),
    })
}

/// A marker name for a value of `column`: its name in lower case with
/// anything but letters, digits and `_` replaced, then the change's number,
/// unique within the script.
fn marker(column: &str, change: usize, used: &mut HashSet<String>) -> String {
    let mut base: String = column
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    if !base
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
    {
        base = format!("c_{base}");
    }
    let mut name = format!("{base}_{change}");
    let mut extra = 2;
    while used.contains(&name) {
        name = format!("{base}_{change}_{extra}");
        extra += 1;
    }
    used.insert(name.clone());
    name
}

/// The statements that write `request`: one per change, each value a
/// `:name` marker whose value goes to the new tab's parameter fields.
pub(super) fn write_script(request: &WriteRequest) -> DriverResult<WriteScript> {
    request.validate()?;
    let target = &request.target;
    let table = DatabricksDriver.relation_name(&target.table);
    let quote = |name: &str| DatabricksDriver.quote_identifier(name);
    let mut used = HashSet::new();
    let mut statements = Vec::with_capacity(request.changes.len());
    let mut parameters = Vec::new();
    for (index, change) in request.changes.iter().enumerate() {
        let mut names = Vec::new();
        let built = statement(&table, target, change, &quote, &mut |_, column| {
            let name = marker(&column.name, index + 1, &mut used);
            names.push(name.clone());
            format!(":{name}")
        })?;
        for (name, (position, value)) in names.into_iter().zip(built.values) {
            let column = target.column(position).ok_or_else(|| {
                DriverError::validation("A change writes a column that cannot be written")
            })?;
            let Some((kind, _)) = parameter_kind(&column.declared_type) else {
                return Err(DriverError::validation(format!(
                    "The column {} cannot be written with a parameter",
                    column.name
                )));
            };
            let value = match value {
                WriteValue::Null => None,
                // A boolean's text, read as the parameter's kind like any.
                WriteValue::Boolean(flag) if is_parameter_value(kind, &flag.to_string()) => {
                    Some(flag.to_string())
                }
                WriteValue::Text(text) if is_parameter_value(kind, &text) => Some(text),
                WriteValue::Text(_) | WriteValue::Boolean(_) => {
                    return Err(DriverError::validation(format!(
                        "The value of {} in change {} does not fit a {} parameter",
                        column.name,
                        index + 1,
                        match kind {
                            ParameterKind::Text => "text",
                            ParameterKind::Number => "number",
                            ParameterKind::Boolean => "boolean",
                            ParameterKind::Date => "date",
                            ParameterKind::Timestamp => "timestamp",
                        }
                    )));
                }
                WriteValue::Bytes(_) => {
                    return Err(DriverError::validation(format!(
                        "The column {} cannot be written with a parameter",
                        column.name
                    )));
                }
            };
            parameters.push(ScriptParameter { name, kind, value });
        }
        statements.push(format!("{};", built.sql));
    }
    Ok(WriteScript {
        statements,
        parameters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn select(table: &[&str], alias: Option<&str>, items: Vec<Item>) -> Option<Select> {
        Some(Select {
            table: table.iter().map(|part| (*part).to_owned()).collect(),
            alias: alias.map(str::to_owned),
            items,
        })
    }

    fn column(name: &str) -> Item {
        Item::Column {
            qualifier: None,
            name: name.into(),
            alias: None,
        }
    }

    #[test]
    fn a_single_table_select_of_plain_columns_is_inferred() {
        assert_eq!(
            infer("SELECT * FROM main.sales.orders"),
            select(
                &["main", "sales", "orders"],
                None,
                vec![Item::Star { qualifier: None }]
            )
        );
        assert_eq!(
            infer(
                "-- the open orders\nselect o.id, `total price` AS total, o.*\nFROM `orders` o\nWHERE o.status = 'open' AND o.id IN (SELECT id FROM x) ORDER BY id LIMIT 10;"
            ),
            select(
                &["orders"],
                Some("o"),
                vec![
                    Item::Column {
                        qualifier: Some("o".into()),
                        name: "id".into(),
                        alias: None
                    },
                    Item::Column {
                        qualifier: None,
                        name: "total price".into(),
                        alias: Some("total".into())
                    },
                    Item::Star {
                        qualifier: Some("o".into())
                    }
                ]
            )
        );
        assert_eq!(
            infer("SELECT id, name FROM s.t WHERE created > :since"),
            select(&["s", "t"], None, vec![column("id"), column("name")])
        );
    }

    #[test]
    fn anything_else_stays_read_only() {
        for sql in [
            "SELECT a.id FROM a JOIN b ON a.id = b.id",
            "SELECT id FROM a, b",
            "SELECT id FROM (SELECT id FROM a)",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "SELECT id FROM a UNION SELECT id FROM b",
            "SELECT id FROM a WHERE x = 1 UNION ALL SELECT id FROM b",
            "SELECT DISTINCT id FROM a",
            "SELECT count(*) FROM a",
            "SELECT id FROM a GROUP BY id",
            "SELECT id + 1 FROM a",
            "SELECT id FROM a VERSION AS OF 3",
            "SELECT id FROM range(10)",
            "SELECT b.id FROM a",
            "SELECT id FROM a LATERAL VIEW explode(x) t AS y",
            "UPDATE a SET id = 1",
            "SELECT id FROM a; SELECT id FROM b",
            "SELECT 'unterminated FROM a",
            "SELECT id FROM a.b.c.d",
        ] {
            assert_eq!(infer(sql), None, "{sql}");
        }
    }

    fn described() -> Value {
        json!({
            "table_type": "MANAGED",
            "columns": [
                {"name": "id", "type_text": "bigint", "nullable": false, "position": 0},
                {"name": "price", "type_text": "decimal(10,2)", "nullable": true, "position": 1},
                {"name": "photo", "type_text": "binary", "nullable": true, "position": 2},
                {"name": "score", "type_text": "double", "nullable": true, "position": 3}
            ],
            "table_constraints": [
                {"primary_key_constraint": {"name": "pk", "child_columns": ["id"]}}
            ]
        })
    }

    fn result(names: &[&str]) -> Vec<ColumnInfo> {
        names
            .iter()
            .map(|name| ColumnInfo::new(*name, "string"))
            .collect()
    }

    #[test]
    fn the_catalog_resolves_the_table_its_columns_and_its_key() {
        let all = infer("SELECT * FROM t").unwrap();
        let editable = target(
            "c",
            "s",
            "t",
            &all,
            &described(),
            &result(&["id", "price", "photo", "score"]),
        )
        .editable()
        .unwrap();
        assert_eq!(editable.key, [0]);
        assert_eq!(editable.key_kind, KeyKind::Informational);
        assert_eq!(editable.writes, WriteMode::Script);
        assert!(
            editable.columns[2].is_none(),
            "binary has no parameter kind"
        );
        assert!(!editable.columns[3].as_ref().unwrap().can_key);

        // Without its key column the user names the key.
        let some = infer("SELECT price AS p FROM t").unwrap();
        let chosen = target("c", "s", "t", &some, &described(), &result(&["p"]))
            .editable()
            .unwrap();
        assert!(chosen.key.is_empty());
        assert_eq!(chosen.key_kind, KeyKind::Chosen);

        assert_eq!(
            target("c", "s", "t", &some, &described(), &result(&["price"])),
            EditTarget::ReadOnly(ReadOnlyReason::ColumnsDoNotMatch)
        );
        let missing = infer("SELECT nothing FROM t").unwrap();
        assert_eq!(
            target("c", "s", "t", &missing, &described(), &result(&["nothing"])),
            EditTarget::ReadOnly(ReadOnlyReason::ColumnsDoNotMatch)
        );
        let mut view = described();
        view["table_type"] = json!("VIEW");
        assert_eq!(
            target("c", "s", "t", &all, &view, &result(&["id"])),
            EditTarget::ReadOnly(ReadOnlyReason::NotATable {
                kind: "view".into()
            })
        );
    }

    /// The name is completed from the session's context and read from Unity
    /// Catalog's tables endpoint; a missing table says its name.
    #[tokio::test]
    async fn the_table_is_read_from_unity_catalog_in_the_sessions_context() {
        let (client, task) = super::super::tests::fixture(vec![(200, described())]).await;
        let found = edit_target(
            &client,
            Some("Main"),
            Some("sales"),
            "SELECT id, price FROM orders",
            &result(&["id", "price"]),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let requests = task.await.unwrap();
        assert!(requests[0].contains("main.sales.orders"), "{}", requests[0]);
        let table = found.editable().unwrap();
        assert_eq!(
            table.table,
            ObjectPath::default()
                .child(ObjectKind::Catalog, "main")
                .child(ObjectKind::Schema, "sales")
                .child(ObjectKind::Table, "orders")
        );
        let (client, task) = super::super::tests::fixture(vec![(
            404,
            json!({"error_code": "TABLE_DOES_NOT_EXIST", "message": "not found"}),
        )])
        .await;
        let missing = edit_target(
            &client,
            Some("main"),
            Some("sales"),
            "SELECT * FROM gone",
            &result(&["id"]),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert_eq!(
            missing,
            EditTarget::ReadOnly(ReadOnlyReason::TableNotFound {
                name: "gone".into()
            })
        );
        // Without a schema to complete the name, nothing is asked.
        let (client, _task) = super::super::tests::fixture(vec![]).await;
        assert_eq!(
            edit_target(
                &client,
                None,
                None,
                "SELECT * FROM gone",
                &result(&["id"]),
                &CancellationToken::new()
            )
            .await
            .unwrap(),
            EditTarget::ReadOnly(ReadOnlyReason::TableNotFound {
                name: "gone".into()
            })
        );
    }

    #[test]
    fn generated_statements_carry_markers_and_never_a_value() {
        let all = infer("SELECT * FROM t").unwrap();
        let editable = target(
            "main",
            "sales",
            "t",
            &all,
            &described(),
            &result(&["id", "price", "photo", "score"]),
        )
        .editable()
        .unwrap();
        let request = WriteRequest {
            target: editable,
            changes: vec![
                write::RowChange::Update {
                    key: vec![WriteValue::Text("7".into())],
                    set: vec![
                        (1, WriteValue::Text("19.90".into())),
                        (0, WriteValue::Text("8".into())),
                    ],
                },
                write::RowChange::Delete {
                    key: vec![WriteValue::Text("9".into())],
                },
                write::RowChange::Insert {
                    values: vec![(0, WriteValue::Text("10".into())), (1, WriteValue::Null)],
                },
            ],
            timeout: None,
            scope: write::WriteScope::Atomic,
        };
        let script = write_script(&request).unwrap();
        assert_eq!(
            script.statements,
            [
                "UPDATE `main`.`sales`.`t` SET `price` = :price_1, `id` = :id_1 WHERE `id` = :id_1_2;",
                "DELETE FROM `main`.`sales`.`t` WHERE `id` = :id_2;",
                "INSERT INTO `main`.`sales`.`t` (`id`, `price`) VALUES (:id_3, :price_3);",
            ]
        );
        assert!(!script.statements.concat().contains("19.90"));
        assert_eq!(
            script.parameters[0],
            ScriptParameter {
                name: "price_1".into(),
                kind: ParameterKind::Number,
                value: Some("19.90".into())
            }
        );
        assert_eq!(
            script.parameters.last().map(|p| p.value.clone()),
            Some(None)
        );
        // A value that does not fit its kind is refused, never rewritten.
        let mut wrong = request.clone();
        wrong.changes = vec![write::RowChange::Update {
            key: vec![WriteValue::Text("7".into())],
            set: vec![(1, WriteValue::Text("abc".into()))],
        }];
        assert!(write_script(&wrong).is_err());
    }
}
