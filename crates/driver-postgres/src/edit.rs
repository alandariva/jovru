//! Writing a result's rows back to their table (ADR-0039): which table a
//! result's columns come from, read from the catalog in the session, and
//! the statements that write each change with bound values.
//!
//! The catalog queries are listed in
//! [Drivers](../../../docs/reference/drivers.md#editing-results).
use crate::PostgresDriver;
use crate::runtime::map_error;
use jykre_core::write::{
    CatalogTable, EditTarget, EditableColumn, ReadOnlyReason, ReferenceAction, SideEffect,
    WriteMode, WriteRequest, WriteValue, changed, insert_target, resolve, single_relation,
    statement, still_matches,
};
use jykre_core::*;
use sqlx::postgres::PgConnection;
use sqlx::postgres::types::Oid;

/// The savepoint a write (or a catalog read) uses inside the session's open
/// transaction, so a failure there leaves that transaction as it was.
pub(crate) const SAVEPOINT: &str = "jykre_edit";

/// What the protocol says about a result column's source, with no I/O: the
/// relation's OID and the attribute number, as decimal text.
pub(crate) fn origin(relation: Option<Oid>, attribute: Option<i16>) -> ColumnOrigin {
    match (relation, attribute) {
        (Some(relation), Some(attribute)) if relation.0 != 0 && attribute > 0 => {
            ColumnOrigin::Stored(SourceColumn {
                relation: relation.0.to_string(),
                column: attribute.to_string(),
            })
        }
        _ => ColumnOrigin::Expression,
    }
}

/// The target of a result with these columns, read from the catalog.
pub(crate) async fn edit_target(
    connection: &mut PgConnection,
    database: &str,
    columns: &[ColumnInfo],
) -> DriverResult<EditTarget> {
    let relation = match single_relation(columns) {
        Ok(relation) => relation.to_owned(),
        Err(reason) => return Ok(EditTarget::ReadOnly(reason)),
    };
    let oid: u32 = relation
        .parse()
        .map_err(|_| DriverError::new(ErrorCode::Protocol, "Invalid column origin"))?;
    let catalog = match catalog(connection, database, oid).await? {
        Ok(catalog) => catalog,
        Err(reason) => return Ok(EditTarget::ReadOnly(reason)),
    };
    let sources: Vec<Option<String>> = columns
        .iter()
        .map(|column| match &column.origin {
            ColumnOrigin::Stored(source) if source.relation == relation => {
                Some(source.column.clone())
            }
            _ => None,
        })
        .collect();
    Ok(resolve(catalog, &sources, WriteMode::Direct))
}

/// The table at `path` (its schema and name; the session is in its
/// database) as an import's target, read from the catalog.
pub(crate) async fn table_target(
    connection: &mut PgConnection,
    database: &str,
    path: &ObjectPath,
) -> DriverResult<EditTarget> {
    let (Some(schema), Some(table)) = (
        path.name_of(ObjectKind::Schema),
        path.name_of(ObjectKind::Table),
    ) else {
        return Err(DriverError::validation("Choose a table to import into"));
    };
    let oid: Option<Oid> = sqlx::query_scalar(
        "SELECT c.oid FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2",
    )
    .bind(schema)
    .bind(table)
    .fetch_optional(&mut *connection)
    .await
    .map_err(map_error)?;
    let Some(oid) = oid else {
        return Ok(EditTarget::ReadOnly(ReadOnlyReason::TableNotFound {
            name: format!("{schema}.{table}"),
        }));
    };
    Ok(match catalog(connection, database, oid.0).await? {
        Ok(catalog) => insert_target(catalog),
        Err(reason) => EditTarget::ReadOnly(reason),
    })
}

/// A row of `pg_attribute`: number, name, type without modifier, NOT NULL,
/// default, generated, identity and the type's name.
type AttributeRow = (i16, String, String, bool, bool, String, String, String);

/// The relation with this OID as a write target, or why it cannot be one.
/// Column identifiers are attribute numbers, as the result's origins give.
async fn catalog(
    connection: &mut PgConnection,
    database: &str,
    oid: u32,
) -> DriverResult<Result<CatalogTable, ReadOnlyReason>> {
    let oid = Oid(oid);
    let relation: Option<(String, String, String)> = sqlx::query_as(
        "SELECT n.nspname::text, c.relname::text, c.relkind::text \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.oid = $1",
    )
    .bind(oid)
    .fetch_optional(&mut *connection)
    .await
    .map_err(map_error)?;
    let Some((schema, name, kind)) = relation else {
        return Err(changed());
    };
    if kind != "r" && kind != "p" {
        let kind = match kind.as_str() {
            "v" => "view",
            "m" => "materialized view",
            "f" => "foreign table",
            "c" => "composite type",
            _ => "relation",
        };
        return Ok(Err(ReadOnlyReason::NotATable { kind: kind.into() }));
    }
    // The type without its modifier: an explicit cast to `varchar(3)`
    // would cut a longer text silently, while assigning it to the column
    // fails, as it must.
    let attributes: Vec<AttributeRow> = sqlx::query_as(
        "SELECT a.attnum, a.attname::text, pg_catalog.format_type(a.atttypid, NULL), \
             a.attnotnull, a.atthasdef, a.attgenerated::text, a.attidentity::text, t.typname::text \
             FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
    )
    .bind(oid)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let name_of = |number: i16| {
        attributes
            .iter()
            .find(|attribute| attribute.0 == number)
            .map(|attribute| (attribute.1.clone(), attribute.3))
    };
    let indexes: Vec<(bool, Vec<i16>, i16, bool)> = sqlx::query_as(
        "SELECT i.indisprimary, i.indkey::int2[], i.indnkeyatts, \
         (i.indexprs IS NULL AND i.indpred IS NULL) \
         FROM pg_catalog.pg_index i \
         WHERE i.indrelid = $1 AND i.indisunique AND i.indisvalid AND i.indimmediate \
         ORDER BY i.indisprimary DESC, i.indexrelid",
    )
    .bind(oid)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let mut primary_key = Vec::new();
    let mut unique_keys = Vec::new();
    for (primary, numbers, key_columns, plain) in indexes {
        let numbers = numbers.iter().take(key_columns.max(0) as usize);
        // An expression's attribute number is 0.
        let columns: Option<Vec<(String, bool)>> = numbers.map(|number| name_of(*number)).collect();
        let Some(columns) = columns.filter(|_| plain) else {
            continue;
        };
        if primary {
            primary_key = columns.into_iter().map(|(name, _)| name).collect();
        } else if columns.iter().all(|(_, not_null)| *not_null) {
            unique_keys.push(columns.into_iter().map(|(name, _)| name).collect());
        }
    }
    let triggers: Vec<String> = sqlx::query_scalar(
        "SELECT tgname::text FROM pg_catalog.pg_trigger \
         WHERE tgrelid = $1 AND NOT tgisinternal ORDER BY 1",
    )
    .bind(oid)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let references: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT conrelid::regclass::text, conname::text, confdeltype::text, confupdtype::text \
         FROM pg_catalog.pg_constraint WHERE confrelid = $1 AND contype = 'f' ORDER BY 2",
    )
    .bind(oid)
    .fetch_all(&mut *connection)
    .await
    .map_err(map_error)?;
    let action = |code: &str| match code {
        "c" => Some(ReferenceAction::Cascade),
        "n" => Some(ReferenceAction::SetNull),
        "d" => Some(ReferenceAction::SetDefault),
        _ => None,
    };
    let mut side_effects: Vec<SideEffect> = triggers
        .into_iter()
        .map(|name| SideEffect::Trigger { name })
        .collect();
    for (table, name, on_delete, on_update) in references {
        let (on_delete, on_update) = (action(&on_delete), action(&on_update));
        if on_delete.is_some() || on_update.is_some() {
            side_effects.push(SideEffect::Reference {
                table,
                name,
                on_delete,
                on_update,
            });
        }
    }
    let columns = attributes
        .into_iter()
        .map(
            |(number, name, declared_type, not_null, has_default, generated, identity, kind)| {
                (
                    number.to_string(),
                    EditableColumn {
                        name,
                        declared_type,
                        nullable: !not_null,
                        writable: generated.is_empty() && identity != "a",
                        has_default: has_default || !not_null || !identity.is_empty(),
                        binary: false,
                        can_key: !matches!(kind.as_str(), "float4" | "float8"),
                    },
                )
            },
        )
        .collect();
    Ok(Ok(CatalogTable {
        table: ObjectPath::default()
            .child(ObjectKind::Database, database)
            .child(ObjectKind::Schema, schema)
            .child(ObjectKind::Table, name),
        columns,
        primary_key,
        unique_keys,
        side_effects,
    }))
}

/// How the changes of a write went, before it is committed or rolled back.
pub(crate) enum Applied {
    /// Every change affected exactly one row.
    Written(u64),
    /// Change `index` affected `matched` rows.
    Mismatch { index: usize, matched: u64 },
}

/// Checks the target again and runs one statement per change, each with its
/// values bound as text and read by the server as the column's type. Runs
/// inside the transaction or savepoint the caller opened; commits nothing.
pub(crate) async fn apply(
    connection: &mut PgConnection,
    database: &str,
    request: &WriteRequest,
) -> DriverResult<Applied> {
    let target = &request.target;
    let (Some(schema), Some(table)) = (
        target.table.name_of(ObjectKind::Schema),
        target.table.name_of(ObjectKind::Table),
    ) else {
        return Err(changed());
    };
    let oid: Option<Oid> = sqlx::query_scalar(
        "SELECT c.oid FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2",
    )
    .bind(schema)
    .bind(table)
    .fetch_optional(&mut *connection)
    .await
    .map_err(map_error)?;
    let fresh = match oid {
        Some(oid) => catalog(connection, database, oid.0).await?,
        None => return Err(changed()),
    };
    if !fresh.is_ok_and(|fresh| still_matches(target, &fresh)) {
        return Err(changed());
    }
    let name = PostgresDriver.relation_name(&target.table);
    let quote = |name: &str| PostgresDriver.quote_identifier(name);
    let mut rows = 0;
    for (index, change) in request.changes.iter().enumerate() {
        let built = statement(&name, target, change, &quote, &mut |number, column| {
            format!("CAST(${number} AS {})", column.declared_type)
        })?;
        let mut query = sqlx::query(sqlx::AssertSqlSafe(built.sql)).persistent(false);
        for (_, value) in &built.values {
            query = match value {
                WriteValue::Null => query.bind(None::<String>),
                WriteValue::Text(text) => query.bind(text.clone()),
                // Read as the column's type from `boolean`, as `CAST`
                // allows (ADR-0040).
                WriteValue::Boolean(flag) => query.bind(*flag),
                // PostgreSQL reads `bytea` as its `\x` text.
                WriteValue::Bytes(_) => {
                    return Err(DriverError::validation(
                        "PostgreSQL writes binary values as their \\x text",
                    ));
                }
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
    fn origins_are_the_relation_and_the_attribute_from_the_row_description() {
        assert_eq!(
            origin(Some(Oid(16384)), Some(2)),
            ColumnOrigin::Stored(SourceColumn {
                relation: "16384".into(),
                column: "2".into()
            })
        );
        // An expression has neither; a system column is not editable.
        assert_eq!(origin(None, None), ColumnOrigin::Expression);
        assert_eq!(origin(Some(Oid(16384)), Some(-1)), ColumnOrigin::Expression);
        assert_eq!(origin(Some(Oid(0)), Some(0)), ColumnOrigin::Expression);
    }
}
