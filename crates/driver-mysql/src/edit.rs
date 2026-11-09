//! Writing a result's rows back to their table (ADR-0039): which table a
//! result's columns come from, read from `information_schema` in the
//! session, and the statements that write each change with bound values.
//!
//! The catalog queries are listed in
//! [Drivers](../../../docs/reference/drivers.md#editing-results).
use crate::MySqlDriver;
use crate::runtime::map_error;
use jykre_core::write::{
    CatalogTable, EditTarget, EditableColumn, ReadOnlyReason, ReferenceAction, SideEffect,
    WriteMode, WriteRequest, WriteValue, changed, resolve, single_relation, statement,
    still_matches,
};
use jykre_core::*;
use sqlx::Row as _;
use sqlx::mysql::MySqlConnection;

/// The savepoint a write uses inside the session's open transaction.
pub(crate) const SAVEPOINT: &str = "jykre_edit";

/// What the protocol says about a result column's source, with no I/O: the
/// `schema.table` text and the physical column name.
pub(crate) fn origin(origin: sqlx::ColumnOrigin) -> ColumnOrigin {
    match origin {
        sqlx::ColumnOrigin::Table(table) => ColumnOrigin::Stored(SourceColumn {
            relation: table.table.to_string(),
            column: table.name.to_string(),
        }),
        sqlx::ColumnOrigin::Expression => ColumnOrigin::Expression,
        _ => ColumnOrigin::Unknown,
    }
}

/// The target of a result with these columns, read from the catalog.
pub(crate) async fn edit_target(
    connection: &mut MySqlConnection,
    columns: &[ColumnInfo],
) -> DriverResult<EditTarget> {
    let relation = match single_relation(columns) {
        Ok(relation) => relation.to_owned(),
        Err(reason) => return Ok(EditTarget::ReadOnly(reason)),
    };
    let Some((schema, table)) = split(connection, &relation).await? else {
        return Ok(EditTarget::ReadOnly(ReadOnlyReason::NotATable {
            kind: "derived table".into(),
        }));
    };
    let catalog = match catalog(connection, &schema, &table).await? {
        Some(Ok(catalog)) => catalog,
        Some(Err(reason)) => return Ok(EditTarget::ReadOnly(reason)),
        None => return Err(changed()),
    };
    let sources: Vec<Option<String>> = columns
        .iter()
        .map(|column| match &column.origin {
            ColumnOrigin::Stored(source) if source.relation == relation => {
                Some(source.column.to_lowercase())
            }
            _ => None,
        })
        .collect();
    Ok(resolve(catalog, &sources, WriteMode::Direct))
}

/// The schema and the table SQLx joined with a dot (decision E-6): split at
/// the only dot, or, with several, at the one split that names a table the
/// catalog has. `None` when no or several splits do.
async fn split(
    connection: &mut MySqlConnection,
    relation: &str,
) -> DriverResult<Option<(String, String)>> {
    let splits: Vec<(String, String)> = relation
        .match_indices('.')
        .map(|(at, _)| (relation[..at].to_owned(), relation[at + 1..].to_owned()))
        .collect();
    if splits.len() <= 1 {
        return Ok(splits.into_iter().next());
    }
    let mut found = Vec::new();
    for (schema, table) in splits {
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
        )
        .bind(&schema)
        .bind(&table)
        .fetch_one(&mut *connection)
        .await
        .map_err(map_error)?;
        if exists > 0 {
            found.push((schema, table));
        }
    }
    Ok(if found.len() == 1 { found.pop() } else { None })
}

/// The table as a write target, or why it cannot be one; `None` when it is
/// gone. Column identifiers are the lowercase column names (MySQL compares
/// them without case).
async fn catalog(
    connection: &mut MySqlConnection,
    schema: &str,
    table: &str,
) -> DriverResult<Option<Result<CatalogTable, ReadOnlyReason>>> {
    let found: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT t.TABLE_TYPE, t.ENGINE, e.TRANSACTIONS FROM information_schema.TABLES t \
         LEFT JOIN information_schema.ENGINES e ON e.ENGINE = t.ENGINE \
         WHERE t.TABLE_SCHEMA = ? AND t.TABLE_NAME = ?",
    )
    .bind(schema)
    .bind(table)
    .fetch_optional(&mut *connection)
    .await
    .map_err(map_error)?;
    let Some((kind, _, transactions)) = found else {
        return Ok(None);
    };
    if kind != "BASE TABLE" {
        let kind = if kind == "VIEW" {
            "view".to_owned()
        } else {
            kind.to_lowercase()
        };
        return Ok(Some(Err(ReadOnlyReason::NotATable { kind })));
    }
    if transactions.as_deref() != Some("YES") {
        return Ok(Some(Err(ReadOnlyReason::NotTransactional)));
    }
    let rows = sqlx::query(
        "SELECT COLUMN_NAME AS name, COLUMN_TYPE AS ctype, DATA_TYPE AS dtype, IS_NULLABLE AS nullable, \
         CAST(COLUMN_DEFAULT IS NOT NULL AS SIGNED) AS dflt, EXTRA AS extra \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
         ORDER BY ORDINAL_POSITION",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let mut columns = Vec::with_capacity(rows.len());
    let mut not_null = Vec::new();
    for row in rows {
        let name: String = row.try_get("name").map_err(map_error)?;
        let declared_type: String = row.try_get("ctype").map_err(map_error)?;
        let data_type = row
            .try_get::<String, _>("dtype")
            .map_err(map_error)?
            .to_lowercase();
        let nullable = row.try_get::<String, _>("nullable").map_err(map_error)? == "YES";
        let default = row.try_get::<i64, _>("dflt").map_err(map_error)? != 0;
        let extra = row
            .try_get::<Option<String>, _>("extra")
            .map_err(map_error)?
            .unwrap_or_default()
            .to_lowercase();
        if !nullable {
            not_null.push(name.to_lowercase());
        }
        // Spatial values arrive as bytes the grid cannot show as text.
        if matches!(
            data_type.as_str(),
            "geometry"
                | "point"
                | "linestring"
                | "polygon"
                | "multipoint"
                | "multilinestring"
                | "multipolygon"
                | "geometrycollection"
                | "geomcollection"
        ) {
            continue;
        }
        let generated = extra.contains("virtual generated") || extra.contains("stored generated");
        columns.push((
            name.to_lowercase(),
            EditableColumn {
                name,
                declared_type,
                nullable,
                writable: !generated,
                has_default: default
                    || nullable
                    || extra.contains("auto_increment")
                    || extra.contains("default_generated"),
                binary: matches!(
                    data_type.as_str(),
                    "binary"
                        | "varbinary"
                        | "tinyblob"
                        | "blob"
                        | "mediumblob"
                        | "longblob"
                        | "bit"
                ),
                // Approximate numbers, and TIMESTAMP, which the session's
                // time zone converts: a value in a repeated hour reads the
                // same for two instants.
                can_key: !matches!(
                    data_type.as_str(),
                    "float" | "double" | "real" | "timestamp"
                ),
            },
        ));
    }
    // One row per column of each unique index, the primary key first.
    // Functional parts have no column name.
    let parts: Vec<(String, i64, Option<String>)> = sqlx::query_as(
        "SELECT INDEX_NAME, CAST(NON_UNIQUE AS SIGNED), COLUMN_NAME FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
         ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let mut indexes: Vec<(String, Option<Vec<String>>)> = Vec::new();
    for (index, non_unique, column) in parts {
        if non_unique != 0 {
            continue;
        }
        if indexes.last().is_none_or(|(name, _)| *name != index) {
            indexes.push((index.clone(), Some(Vec::new())));
        }
        if let Some((_, columns)) = indexes.last_mut() {
            match (columns.as_mut(), column) {
                (Some(columns), Some(column)) => columns.push(column),
                _ => *columns = None,
            }
        }
    }
    let mut primary_key = Vec::new();
    let mut unique_keys = Vec::new();
    for (index, names) in indexes {
        let Some(names) = names else { continue };
        let names: Vec<String> = names
            .into_iter()
            .map(|name| {
                // The catalog's spelling of the column.
                columns
                    .iter()
                    .find(|(id, _)| *id == name.to_lowercase())
                    .map_or(name, |(_, column)| column.name.clone())
            })
            .collect();
        if index == "PRIMARY" {
            primary_key = names;
        } else if names
            .iter()
            .all(|name| not_null.contains(&name.to_lowercase()))
        {
            unique_keys.push(names);
        }
    }
    let triggers: Vec<String> = sqlx::query_scalar(
        "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS \
         WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY TRIGGER_NAME",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let references: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT CONSTRAINT_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, DELETE_RULE, UPDATE_RULE \
         FROM information_schema.REFERENTIAL_CONSTRAINTS \
         WHERE UNIQUE_CONSTRAINT_SCHEMA = ? AND REFERENCED_TABLE_NAME = ? ORDER BY CONSTRAINT_NAME",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let action = |rule: &str| match rule {
        "CASCADE" => Some(ReferenceAction::Cascade),
        "SET NULL" => Some(ReferenceAction::SetNull),
        "SET DEFAULT" => Some(ReferenceAction::SetDefault),
        _ => None,
    };
    let mut side_effects: Vec<SideEffect> = triggers
        .into_iter()
        .map(|name| SideEffect::Trigger { name })
        .collect();
    for (owner, referencing, name, on_delete, on_update) in references {
        let (on_delete, on_update) = (action(&on_delete), action(&on_update));
        if on_delete.is_some() || on_update.is_some() {
            side_effects.push(SideEffect::Reference {
                table: format!("{owner}.{referencing}"),
                name,
                on_delete,
                on_update,
            });
        }
    }
    Ok(Some(Ok(CatalogTable {
        table: ObjectPath::default()
            .child(ObjectKind::Database, schema)
            .child(ObjectKind::Table, table),
        columns,
        primary_key,
        unique_keys,
        side_effects,
    })))
}

/// The placeholder of a value of `column`: the cast that makes MySQL read
/// the text as the column's type, so a number is never compared to a
/// string as floating point. Text columns take the text as it is, so their
/// collation applies; binary columns take bytes.
pub(crate) fn placeholder(column: &EditableColumn) -> String {
    let declared = column.declared_type.to_lowercase();
    let base = declared
        .split(['(', ' '])
        .next()
        .unwrap_or_default()
        .to_owned();
    let arguments = declared
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(inside, _)| inside.to_owned());
    let fsp = arguments
        .as_deref()
        .and_then(|digits| digits.trim().parse::<u8>().ok())
        .map_or(String::new(), |fsp| format!("({fsp})"));
    match base.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" => {
            if declared.contains("unsigned") {
                "CAST(? AS UNSIGNED)".to_owned()
            } else {
                "CAST(? AS SIGNED)".to_owned()
            }
        }
        "decimal" | "numeric" | "dec" | "fixed" => {
            let (precision, scale) = arguments
                .as_deref()
                .map(|inside| {
                    let mut parts = inside.split(',').map(|part| part.trim().parse::<u8>().ok());
                    (
                        parts.next().flatten().unwrap_or(10),
                        parts.next().flatten().unwrap_or(0),
                    )
                })
                .unwrap_or((10, 0));
            format!("CAST(? AS DECIMAL({precision},{scale}))")
        }
        "date" => "CAST(? AS DATE)".to_owned(),
        "datetime" | "timestamp" => format!("CAST(? AS DATETIME{fsp})"),
        "time" => format!("CAST(? AS TIME{fsp})"),
        _ => "?".to_owned(),
    }
}

/// How the changes of a write went, before it is committed or rolled back.
pub(crate) enum Applied {
    /// Every change affected exactly one row.
    Written(u64),
    /// Change `index` affected `matched` rows.
    Mismatch { index: usize, matched: u64 },
    /// MySQL adjusted a value of change `index` (a warning), which it says.
    Adjusted { index: usize, warning: String },
}

/// Checks the target again and runs one statement per change, with its
/// values bound, reading `@@warning_count` after each. Runs inside the
/// transaction or savepoint the caller opened; commits nothing.
pub(crate) async fn apply(
    connection: &mut MySqlConnection,
    request: &WriteRequest,
) -> DriverResult<Applied> {
    let target = &request.target;
    let (Some(schema), Some(table)) = (
        target.table.name_of(ObjectKind::Database),
        target.table.name_of(ObjectKind::Table),
    ) else {
        return Err(changed());
    };
    match catalog(connection, schema, table).await? {
        Some(Ok(fresh)) if still_matches(target, &fresh) => {}
        _ => return Err(changed()),
    }
    let name = MySqlDriver.relation_name(&target.table);
    let quote = |name: &str| MySqlDriver.quote_identifier(name);
    let mut rows = 0;
    for (index, change) in request.changes.iter().enumerate() {
        let built = statement(&name, target, change, &quote, &mut |_, column| {
            placeholder(column)
        })?;
        let mut query = sqlx::query(sqlx::AssertSqlSafe(built.sql)).persistent(false);
        for (position, value) in &built.values {
            let binary = target.column(*position).is_some_and(|column| column.binary);
            query = match (value, binary) {
                (WriteValue::Null, _) => query.bind(None::<String>),
                (WriteValue::Text(text), false) => query.bind(text.clone()),
                (WriteValue::Bytes(bytes), true) => query.bind(bytes.clone()),
                _ => {
                    return Err(DriverError::validation(
                        "A binary column takes bytes and a text column takes text",
                    ));
                }
            };
        }
        let matched = query
            .execute(&mut *connection)
            .await
            .map_err(map_error)?
            .rows_affected();
        // The text protocol, which leaves the diagnostics as they are.
        let warnings = sqlx::raw_sql("SELECT @@warning_count")
            .fetch_one(&mut *connection)
            .await
            .map_err(map_error)?;
        if text_cell(&warnings, 0)?.trim() != "0" {
            let shown = sqlx::raw_sql("SHOW WARNINGS")
                .fetch_optional(&mut *connection)
                .await
                .map_err(map_error)?;
            let warning = match shown {
                Some(row) => text_cell(&row, 2)?,
                None => String::new(),
            };
            return Ok(Applied::Adjusted { index, warning });
        }
        if matched != 1 {
            return Ok(Applied::Mismatch { index, matched });
        }
        rows += 1;
    }
    Ok(Applied::Written(rows))
}

/// A cell of a text-protocol row as text.
fn text_cell(row: &sqlx::mysql::MySqlRow, index: usize) -> DriverResult<String> {
    let value = row.try_get_raw(index).map_err(map_error)?;
    if sqlx::ValueRef::is_null(&value) {
        return Ok(String::new());
    }
    let bytes = <Vec<u8> as sqlx::Decode<sqlx::MySql>>::decode(value)
        .map_err(|_| DriverError::new(ErrorCode::Protocol, "Invalid database value"))?;
    String::from_utf8(bytes)
        .map_err(|_| DriverError::new(ErrorCode::Protocol, "Invalid database text encoding"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(declared: &str) -> EditableColumn {
        EditableColumn {
            name: "c".into(),
            declared_type: declared.into(),
            nullable: true,
            writable: true,
            has_default: true,
            binary: false,
            can_key: true,
        }
    }

    #[test]
    fn values_are_cast_by_the_declared_type_family() {
        let cases = [
            ("int", "CAST(? AS SIGNED)"),
            ("bigint unsigned", "CAST(? AS UNSIGNED)"),
            ("tinyint(1)", "CAST(? AS SIGNED)"),
            ("decimal(65,30)", "CAST(? AS DECIMAL(65,30))"),
            ("decimal(12,2) unsigned", "CAST(? AS DECIMAL(12,2))"),
            ("decimal", "CAST(? AS DECIMAL(10,0))"),
            ("date", "CAST(? AS DATE)"),
            ("datetime(6)", "CAST(? AS DATETIME(6))"),
            ("datetime", "CAST(? AS DATETIME)"),
            ("timestamp(3)", "CAST(? AS DATETIME(3))"),
            ("time(2)", "CAST(? AS TIME(2))"),
            ("varchar(80)", "?"),
            ("enum('a','b')", "?"),
            ("varbinary(16)", "?"),
            ("double", "?"),
        ];
        for (declared, expected) in cases {
            assert_eq!(placeholder(&column(declared)), expected, "{declared}");
        }
    }

    #[test]
    fn origins_keep_the_joined_table_and_the_physical_column() {
        assert_eq!(
            origin(sqlx::ColumnOrigin::Expression),
            ColumnOrigin::Expression
        );
        assert_eq!(origin(sqlx::ColumnOrigin::Unknown), ColumnOrigin::Unknown);
    }
}
