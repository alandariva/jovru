//! Writing a result's rows back to their table (ADR-0039): which table a
//! result's columns come from, read from the schema and the table pragmas
//! in the session, and the statements that write each change with bound
//! values.
//!
//! The catalog queries are listed in
//! [Drivers](../../../docs/reference/drivers.md#editing-results).
use crate::runtime::map_error;
use crate::{MAIN_SCHEMA, SqliteDriver};
use jykre_core::write::{
    CatalogTable, EditTarget, EditableColumn, ReadOnlyReason, ReferenceAction, SideEffect,
    WriteMode, WriteRequest, WriteValue, changed, insert_target, resolve, single_relation,
    statement, still_matches,
};
use jykre_core::*;
use sqlx::sqlite::SqliteConnection;

/// The savepoint a write uses inside the session's open transaction.
pub(crate) const SAVEPOINT: &str = "jykre_edit";

/// What SQLite says about a result column's source, with no I/O: the
/// `db.table` text (`table` for `main`) and the column's own name.
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

/// A name quoted for SQL the driver writes itself.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The target of a result with these columns, read from the schema.
pub(crate) async fn edit_target(
    connection: &mut SqliteConnection,
    columns: &[ColumnInfo],
) -> DriverResult<EditTarget> {
    let relation = match single_relation(columns) {
        Ok(relation) => relation.to_owned(),
        Err(reason) => return Ok(EditTarget::ReadOnly(reason)),
    };
    let Some((schema, table)) = split(connection, &relation).await? else {
        return Ok(EditTarget::ReadOnly(ReadOnlyReason::TableNotFound {
            name: relation,
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

/// The database and the table SQLite named (decision E-6): `table` is a
/// table of `main`; `db.table` is split at a dot. A name with dots is
/// checked against `pragma_database_list` and each database's schema, and
/// used only when exactly one reading names a table that exists.
async fn split(
    connection: &mut SqliteConnection,
    relation: &str,
) -> DriverResult<Option<(String, String)>> {
    let mut readings = vec![(MAIN_SCHEMA.to_owned(), relation.to_owned())];
    readings.extend(
        relation
            .match_indices('.')
            .map(|(at, _)| (relation[..at].to_owned(), relation[at + 1..].to_owned())),
    );
    if readings.len() == 1 {
        return Ok(readings.pop());
    }
    let databases: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_database_list")
        .fetch_all(&mut *connection)
        .await
        .map_err(map_error)?;
    let mut found = Vec::new();
    for (schema, table) in readings {
        if !databases.contains(&schema) {
            continue;
        }
        let sql = format!(
            "SELECT count(*) FROM {}.sqlite_schema WHERE name = ?1",
            quoted(&schema)
        );
        let exists: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
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

/// SQLite's affinity of a declared type, by its rules: `INT`, then `CHAR`,
/// `CLOB` or `TEXT`, then `BLOB` or nothing, then `REAL`, `FLOA` or `DOUB`,
/// else `NUMERIC`.
fn affinity(declared: &str) -> &'static str {
    let upper = declared.to_uppercase();
    if upper.contains("INT") {
        "INTEGER"
    } else if ["CHAR", "CLOB", "TEXT"]
        .iter()
        .any(|word| upper.contains(word))
    {
        "TEXT"
    } else if upper.contains("BLOB") || upper.trim().is_empty() {
        "BLOB"
    } else if ["REAL", "FLOA", "DOUB"]
        .iter()
        .any(|word| upper.contains(word))
    {
        "REAL"
    } else {
        "NUMERIC"
    }
}

/// The table at `path` (its database, `main` or attached, and name) as an
/// import's target, read from the schema.
pub(crate) async fn table_target(
    connection: &mut SqliteConnection,
    path: &ObjectPath,
) -> DriverResult<EditTarget> {
    let Some(table) = path.name_of(ObjectKind::Table) else {
        return Err(DriverError::validation("Choose a table to import into"));
    };
    let schema = path.name_of(ObjectKind::Schema).unwrap_or(MAIN_SCHEMA);
    Ok(match catalog(connection, schema, table).await? {
        Some(Ok(catalog)) => insert_target(catalog),
        Some(Err(reason)) => EditTarget::ReadOnly(reason),
        None => EditTarget::ReadOnly(ReadOnlyReason::TableNotFound { name: table.into() }),
    })
}

/// The table as a write target, or why it cannot be one; `None` when it is
/// gone. Column identifiers are the lowercase column names (SQLite compares
/// them without case).
async fn catalog(
    connection: &mut SqliteConnection,
    schema: &str,
    table: &str,
) -> DriverResult<Option<Result<CatalogTable, ReadOnlyReason>>> {
    let sql = format!(
        "SELECT type, coalesce(sql, '') FROM {}.sqlite_schema WHERE name = ?1 AND type IN ('table', 'view')",
        quoted(schema)
    );
    let found: Option<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(table)
        .fetch_optional(&mut *connection)
        .await
        .map_err(map_error)?;
    let Some((kind, definition)) = found else {
        return Ok(None);
    };
    if kind == "view" {
        return Ok(Some(Err(ReadOnlyReason::NotATable {
            kind: "view".into(),
        })));
    }
    if definition
        .trim_start()
        .to_uppercase()
        .starts_with("CREATE VIRTUAL")
    {
        return Ok(Some(Err(ReadOnlyReason::NotATable {
            kind: "virtual table".into(),
        })));
    }
    let rows: Vec<(String, String, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT name, type, \"notnull\", dflt_value IS NOT NULL, pk, hidden \
         FROM pragma_table_xinfo(?1, ?2) ORDER BY cid",
    )
    .bind(table)
    .bind(schema)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let key_columns = rows.iter().filter(|row| row.4 > 0).count();
    let mut primary: Vec<(i64, String)> = Vec::new();
    let mut not_null = Vec::new();
    let mut columns = Vec::with_capacity(rows.len());
    for (name, declared, notnull, default, pk, hidden) in rows {
        // Hidden columns of virtual tables are not the table's.
        if hidden == 1 {
            continue;
        }
        if pk > 0 {
            primary.push((pk, name.clone()));
        }
        if notnull != 0 {
            not_null.push(name.to_lowercase());
        }
        let affinity = affinity(&declared);
        // `INTEGER PRIMARY KEY` is the rowid: an insert may leave it out.
        let rowid = pk == 1 && key_columns == 1 && declared.eq_ignore_ascii_case("INTEGER");
        columns.push((
            name.to_lowercase(),
            EditableColumn {
                name,
                declared_type: declared.clone(),
                nullable: notnull == 0,
                // 2 and 3: generated columns.
                writable: hidden == 0,
                has_default: default != 0 || notnull == 0 || rowid,
                binary: declared.to_uppercase().contains("BLOB"),
                can_key: affinity != "REAL",
            },
        ));
    }
    primary.sort();
    let primary_key: Vec<String> = primary.into_iter().map(|(_, name)| name).collect();
    let indexes: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT name, \"unique\", partial FROM pragma_index_list(?1, ?2) ORDER BY seq DESC",
    )
    .bind(table)
    .bind(schema)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let mut unique_keys = Vec::new();
    for (index, unique, partial) in indexes {
        if unique == 0 || partial != 0 {
            continue;
        }
        let parts: Vec<(i64, Option<String>)> =
            sqlx::query_as("SELECT cid, name FROM pragma_index_info(?1, ?2) ORDER BY seqno")
                .bind(&index)
                .bind(schema)
                .fetch_all(&mut *connection)
                .await
                .map_err(map_error)?;
        // An expression's column number is -2.
        let names: Option<Vec<String>> = parts
            .into_iter()
            .map(|(cid, name)| name.filter(|_| cid >= 0))
            .collect();
        if let Some(names) = names
            && !names.is_empty()
            && names
                .iter()
                .all(|name| not_null.contains(&name.to_lowercase()))
        {
            unique_keys.push(names);
        }
    }
    let sql = format!(
        "SELECT name FROM {}.sqlite_schema WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name",
        quoted(schema)
    );
    let triggers: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(table)
        .fetch_all(&mut *connection)
        .await
        .map_err(map_error)?;
    let sql = format!(
        "SELECT DISTINCT m.name, f.id, f.on_delete, f.on_update \
         FROM {}.sqlite_schema m, pragma_foreign_key_list(m.name, ?2) f \
         WHERE m.type = 'table' AND f.\"table\" = ?1 COLLATE NOCASE ORDER BY m.name, f.id",
        quoted(schema)
    );
    let references: Vec<(String, i64, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(table)
        .bind(schema)
        .fetch_all(&mut *connection)
        .await
        .map_err(map_error)?;
    let action = |rule: &str| match rule.to_uppercase().as_str() {
        "CASCADE" => Some(ReferenceAction::Cascade),
        "SET NULL" => Some(ReferenceAction::SetNull),
        "SET DEFAULT" => Some(ReferenceAction::SetDefault),
        _ => None,
    };
    let mut side_effects: Vec<SideEffect> = triggers
        .into_iter()
        .map(|name| SideEffect::Trigger { name })
        .collect();
    for (referencing, id, on_delete, on_update) in references {
        let (on_delete, on_update) = (action(&on_delete), action(&on_update));
        if on_delete.is_some() || on_update.is_some() {
            side_effects.push(SideEffect::Reference {
                name: format!("{referencing} #{id}"),
                table: referencing,
                on_delete,
                on_update,
            });
        }
    }
    Ok(Some(Ok(CatalogTable {
        table: ObjectPath::default()
            .child(ObjectKind::Schema, schema)
            .child(ObjectKind::Table, table),
        columns,
        primary_key,
        unique_keys,
        side_effects,
    })))
}

/// How the changes of a write went, before it is committed or rolled back.
pub(crate) enum Applied {
    /// Every change affected exactly one row.
    Written(u64),
    /// Change `index` affected `matched` rows.
    Mismatch { index: usize, matched: u64 },
}

/// Checks the target again and runs one statement per change, its text
/// bound as text (the column's affinity converts it) and its bytes as
/// bytes. Runs inside the transaction or savepoint the caller opened;
/// commits nothing.
pub(crate) async fn apply(
    connection: &mut SqliteConnection,
    request: &WriteRequest,
) -> DriverResult<Applied> {
    let target = &request.target;
    let (Some(schema), Some(table)) = (
        target.table.name_of(ObjectKind::Schema),
        target.table.name_of(ObjectKind::Table),
    ) else {
        return Err(changed());
    };
    match catalog(connection, schema, table).await? {
        Some(Ok(fresh)) if still_matches(target, &fresh) => {}
        _ => return Err(changed()),
    }
    let name = SqliteDriver.relation_name(&target.table);
    let quote = |name: &str| SqliteDriver.quote_identifier(name);
    let mut rows = 0;
    for (index, change) in request.changes.iter().enumerate() {
        let built = statement(&name, target, change, &quote, &mut |number, _| {
            format!("?{number}")
        })?;
        let mut query = sqlx::query(sqlx::AssertSqlSafe(built.sql)).persistent(false);
        for (_, value) in &built.values {
            query = match value {
                WriteValue::Null => query.bind(None::<String>),
                WriteValue::Text(text) => query.bind(text.clone()),
                WriteValue::Bytes(bytes) => query.bind(bytes.clone()),
                // SQLite has no boolean: 1 or 0 (ADR-0040).
                WriteValue::Boolean(flag) => query.bind(i64::from(*flag)),
            };
        }
        let matched = query
            .execute(&mut *connection)
            .await
            .map_err(|error| map_error(error).at_change(index))?
            .rows_affected();
        if matched != 1 {
            return Ok(Applied::Mismatch { index, matched });
        }
        rows += 1;
    }
    Ok(Applied::Written(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_follows_sqlites_rules() {
        assert_eq!(affinity("INTEGER"), "INTEGER");
        assert_eq!(affinity("varchar(80)"), "TEXT");
        assert_eq!(affinity(""), "BLOB");
        assert_eq!(affinity("double precision"), "REAL");
        assert_eq!(affinity("DECIMAL(10,2)"), "NUMERIC");
        // `INT` wins: "POINT" holds it.
        assert_eq!(affinity("FLOATING POINT"), "INTEGER");
    }
}
