//! Metadata from each database's catalog views (`[db].sys.*`), read through
//! a connection of its own. Catalog views already show only what the user
//! has some permission on.

use crate::net::{self, Connection, bracketed, map_error};
use jykre_core::*;
use std::collections::HashSet;
use std::time::Duration;

/// Schemas left out: `guest`, `INFORMATION_SCHEMA`, `sys` and the schemas
/// of the fixed database roles (`db_owner`…).
const USER_SCHEMAS: &str =
    "s.schema_id NOT IN (2, 3, 4) AND s.schema_id NOT BETWEEN 16384 AND 16399";
const DATABASES_SQL: &str = "SELECT name FROM sys.databases WHERE database_id > 4 AND state = 0 AND HAS_DBACCESS(name) = 1 ORDER BY name";

/// A column's type as it is declared: `nvarchar(80)`, `decimal(10,2)`,
/// `varbinary(max)`, `datetime2(3)`; an alias type by its name.
fn type_expression(t: &str, c: &str) -> String {
    format!(
        "CASE WHEN {t}.is_user_defined = 1 THEN {t}.name \
         WHEN {t}.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN {t}.name + N'(' + CASE WHEN {c}.max_length = -1 THEN N'max' ELSE CAST({c}.max_length AS nvarchar(10)) END + N')' \
         WHEN {t}.name IN (N'nvarchar', N'nchar') THEN {t}.name + N'(' + CASE WHEN {c}.max_length = -1 THEN N'max' ELSE CAST({c}.max_length / 2 AS nvarchar(10)) END + N')' \
         WHEN {t}.name IN (N'decimal', N'numeric') THEN {t}.name + N'(' + CAST({c}.precision AS nvarchar(10)) + N',' + CAST({c}.scale AS nvarchar(10)) + N')' \
         WHEN {t}.name IN (N'datetime2', N'time', N'datetimeoffset') THEN {t}.name + N'(' + CAST({c}.scale AS nvarchar(10)) + N')' \
         ELSE {t}.name END"
    )
}

/// Schemas, relations and columns of database `db`, with defaults, comments
/// (`MS_Description`) and each column's position in the primary key.
/// `@P1` schema, `@P2` relation, `@P3` column, each may be NULL.
fn relations_sql(db: &str, names_only: bool) -> String {
    let db = bracketed(db);
    let names_only = u8::from(names_only);
    let dtype = type_expression("t", "c");
    format!(
        "SELECT s.name AS ns, o.name AS rel, CAST(RTRIM(o.type) AS nvarchar(2)) AS reltype, \
         CAST(ep.value AS nvarchar(max)) AS relcomment, c.name AS col, {dtype} AS dtype, \
         c.is_nullable AS isnull, c.column_id AS pos, dc.definition AS dflt, \
         CAST(cep.value AS nvarchar(max)) AS colcomment, CAST(pk.key_ordinal AS int) AS pk \
         FROM {db}.sys.schemas s \
         LEFT JOIN {db}.sys.objects o ON o.schema_id = s.schema_id AND o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
         LEFT JOIN {db}.sys.extended_properties ep ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = N'MS_Description' \
         LEFT JOIN {db}.sys.columns c ON c.object_id = o.object_id AND {names_only} = 0 AND (@P3 IS NULL OR c.name = @P3) \
         LEFT JOIN {db}.sys.types t ON t.user_type_id = c.user_type_id \
         LEFT JOIN {db}.sys.default_constraints dc ON dc.object_id = c.default_object_id \
         LEFT JOIN {db}.sys.extended_properties cep ON cep.class = 1 AND cep.major_id = c.object_id AND cep.minor_id = c.column_id AND cep.name = N'MS_Description' \
         LEFT JOIN (SELECT ic.object_id, ic.column_id, ic.key_ordinal FROM {db}.sys.indexes i JOIN {db}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id WHERE i.is_primary_key = 1) pk ON pk.object_id = c.object_id AND pk.column_id = c.column_id \
         WHERE {USER_SCHEMAS} AND (@P1 IS NULL OR s.name = @P1) AND (@P2 IS NULL OR o.name = @P2) \
         ORDER BY s.name, o.name, c.column_id"
    )
}

/// One row per column of each foreign key of database `db`, in key order.
/// `@P1` schema and `@P2` table, each may be NULL.
fn foreign_keys_sql(db: &str) -> String {
    let db = bracketed(db);
    format!(
        "SELECT s.name AS ns, o.name AS rel, fk.name AS name, rs.name AS refns, ro.name AS refrel, pc.name AS col, rc.name AS refcol \
         FROM {db}.sys.foreign_keys fk \
         JOIN {db}.sys.objects o ON o.object_id = fk.parent_object_id \
         JOIN {db}.sys.schemas s ON s.schema_id = o.schema_id \
         JOIN {db}.sys.objects ro ON ro.object_id = fk.referenced_object_id \
         JOIN {db}.sys.schemas rs ON rs.schema_id = ro.schema_id \
         JOIN {db}.sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN {db}.sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
         JOIN {db}.sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
         WHERE (@P1 IS NULL OR s.name = @P1) AND (@P2 IS NULL OR o.name = @P2) \
         ORDER BY s.name, o.name, fk.name, fkc.constraint_column_id"
    )
}

fn text(row: &tiberius::Row, column: &str) -> DriverResult<Option<String>> {
    Ok(row
        .try_get::<&str, _>(column)
        .map_err(map_error)?
        .map(str::to_owned))
}

fn required(row: &tiberius::Row, column: &str) -> DriverResult<String> {
    text(row, column)?
        .ok_or_else(|| DriverError::new(ErrorCode::Protocol, "Invalid database value"))
}

fn number(row: &tiberius::Row, column: &str) -> DriverResult<Option<i32>> {
    row.try_get::<i32, _>(column).map_err(map_error)
}

/// Collects objects inside the scope and hands them to the sink in
/// batches, so a large schema is not delivered one row at a time.
struct Batches<'a> {
    sink: &'a mut dyn IntrospectionSink,
    scope: &'a ObjectPath,
    pending: Vec<MetadataObject>,
}

impl<'a> Batches<'a> {
    const SIZE: usize = 500;

    fn new(sink: &'a mut dyn IntrospectionSink, scope: &'a ObjectPath) -> Self {
        Self {
            sink,
            scope,
            pending: Vec::new(),
        }
    }

    async fn push(&mut self, object: MetadataObject) -> DriverResult<()> {
        if object.path.is_within(self.scope) {
            self.pending.push(object);
        }
        if self.pending.len() >= Self::SIZE {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> DriverResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.sink.batch(std::mem::take(&mut self.pending)).await
    }
}

/// Whether the user may use database `db`: `None` when it does not exist.
async fn database_access(connection: &mut Connection, db: &str) -> DriverResult<Option<bool>> {
    let rows = connection
        .query("SELECT HAS_DBACCESS(@P1) AS access", &[Some(db)])
        .await?;
    Ok(match rows.first() {
        Some(row) => number(row, "access")?.map(|access| access == 1),
        None => None,
    })
}

pub async fn introspect(
    input: &ResolvedConnection,
    scope: &ObjectPath,
    level: IntrospectionLevel,
    sink: &mut dyn IntrospectionSink,
    cancel: CancellationToken,
) -> DriverResult<IntrospectionReport> {
    let names_only = level == IntrospectionLevel::Names;
    let mut connection = net::connect(input, &cancel).await?;
    net::bounded(&cancel, Some(Duration::from_secs(300)), async {
        let databases = match scope.name_of(ObjectKind::Database) {
            Some(db) => vec![db.to_owned()],
            None => database_names(&mut connection).await?,
        };
        let mut report = IntrospectionReport::default();
        let mut out = Batches::new(sink, scope);
        for db in databases {
            match database_access(&mut connection, &db).await? {
                Some(true) => {}
                Some(false) => {
                    report.warnings.push(IntrospectionWarning {
                        path: ObjectPath::default().child(ObjectKind::Database, db),
                        message: "No permission to use this database".into(),
                    });
                    continue;
                }
                None => continue,
            }
            introspect_database(&mut connection, &db, scope, names_only, &mut out).await?;
        }
        out.flush().await?;
        Ok(report)
    })
    .await
}

async fn introspect_database(
    connection: &mut Connection,
    db: &str,
    scope: &ObjectPath,
    names_only: bool,
    out: &mut Batches<'_>,
) -> DriverResult<()> {
    let schema = scope.name_of(ObjectKind::Schema);
    let relation = scope.relation();
    let column = scope.name_of(ObjectKind::Column);
    let database = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), db);
    out.push(database.clone()).await?;
    let rows = connection
        .query(&relations_sql(db, names_only), &[schema, relation, column])
        .await?;
    // Relations delivered so far: keys only hang under a visible one.
    let mut seen = HashSet::new();
    for row in &rows {
        let schema = MetadataObject::new(ObjectKind::Schema, &database.path, required(row, "ns")?);
        if seen.insert(schema.path.clone()) {
            out.push(schema.clone()).await?;
        }
        let Some(name) = text(row, "rel")? else {
            continue;
        };
        let kind = if text(row, "reltype")?.as_deref() == Some("V") {
            ObjectKind::View
        } else {
            ObjectKind::Table
        };
        let mut relation = MetadataObject::new(kind, &schema.path, name);
        if seen.insert(relation.path.clone()) {
            relation.comment = text(row, "relcomment")?;
            out.push(relation.clone()).await?;
        }
        let Some(name) = text(row, "col")? else {
            continue;
        };
        let mut column = MetadataObject::new(ObjectKind::Column, &relation.path, name);
        column.data_type = text(row, "dtype")?;
        column.nullable = row.try_get::<bool, _>("isnull").map_err(map_error)?;
        column.ordinal = number(row, "pos")?.and_then(|value| u32::try_from(value).ok());
        column.default_value = text(row, "dflt")?;
        column.comment = text(row, "colcomment")?;
        column.primary_key = number(row, "pk")?.and_then(|value| u32::try_from(value).ok());
        out.push(column).await?;
    }
    if names_only || column.is_some() {
        return Ok(());
    }
    let keys = connection
        .query(&foreign_keys_sql(db), &[schema, relation])
        .await?;
    // One row per key column, in key order: consecutive rows of the same
    // constraint make one key.
    let mut current: Option<MetadataObject> = None;
    for row in &keys {
        let table = database
            .path
            .child(ObjectKind::Schema, required(row, "ns")?)
            .child(ObjectKind::Table, required(row, "rel")?);
        let name = required(row, "name")?;
        let path = table.child(ObjectKind::ForeignKey, name.as_str());
        if current.as_ref().is_none_or(|key| key.path != path) {
            if let Some(key) = current.take() {
                out.push(key).await?;
            }
            if !seen.contains(&table) {
                continue;
            }
            let mut key = MetadataObject::new(ObjectKind::ForeignKey, &table, name);
            key.references = Some(ForeignKeyTarget {
                relation: database
                    .path
                    .child(ObjectKind::Schema, required(row, "refns")?)
                    .child(ObjectKind::Table, required(row, "refrel")?),
                columns: Vec::new(),
                referenced: Vec::new(),
            });
            current = Some(key);
        }
        if let Some(target) = current.as_mut().and_then(|key| key.references.as_mut()) {
            target.columns.push(required(row, "col")?);
            target.referenced.push(required(row, "refcol")?);
        }
    }
    if let Some(key) = current {
        out.push(key).await?;
    }
    Ok(())
}

async fn database_names(connection: &mut Connection) -> DriverResult<Vec<String>> {
    connection
        .rows(DATABASES_SQL)
        .await?
        .iter()
        .map(|row| required(row, "name"))
        .collect()
}

/// The root holds the databases the user may use, without the system ones;
/// a database holds its schemas, without the system and role schemas.
pub async fn discover(
    input: &ResolvedConnection,
    parent: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Vec<MetadataObject>> {
    let db = match parent.0.as_slice() {
        [] => None,
        [only] if only.kind == ObjectKind::Database => Some(only.name.clone()),
        _ => return Ok(Vec::new()),
    };
    let mut connection = net::connect(input, &cancel).await?;
    net::bounded(&cancel, Some(Duration::from_secs(60)), async {
        let Some(db) = db else {
            return Ok(database_names(&mut connection)
                .await?
                .into_iter()
                .map(|name| MetadataObject::new(ObjectKind::Database, parent, name))
                .collect());
        };
        let sql = format!(
            "SELECT s.name AS name FROM {}.sys.schemas s WHERE {USER_SCHEMAS} ORDER BY s.name",
            bracketed(&db)
        );
        connection
            .rows(&sql)
            .await?
            .iter()
            .map(|row| {
                Ok(MetadataObject::new(
                    ObjectKind::Schema,
                    parent,
                    required(row, "name")?,
                ))
            })
            .collect()
    })
    .await
}

/// `[db].[schema].[name]`, as `OBJECT_ID` reads it.
fn object_name(db: &str, schema: &str, name: &str) -> String {
    format!(
        "{}.{}.{}",
        bracketed(db),
        bracketed(schema),
        bracketed(name)
    )
}

fn item(kind: InspectedKind, name: String, detail: String) -> InspectionItem {
    InspectionItem { kind, name, detail }
}

/// Indexes, constraints, defaults and triggers of a table; functions,
/// procedures, sequences and types of a schema.
pub async fn inspect(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Inspection> {
    let (Some(db), Some(schema)) = (
        path.name_of(ObjectKind::Database),
        path.name_of(ObjectKind::Schema),
    ) else {
        return Err(DriverError::validation(
            "Choose a schema, a table or a view",
        ));
    };
    let mut connection = net::connect(input, &cancel).await?;
    net::bounded(&cancel, Some(Duration::from_secs(60)), async {
        match path.relation() {
            Some(relation) => {
                inspect_relation(&mut connection, &object_name(db, schema, relation), db).await
            }
            None => inspect_schema(&mut connection, db, schema).await,
        }
    })
    .await
}

async fn inspect_relation(
    connection: &mut Connection,
    object: &str,
    db: &str,
) -> DriverResult<Inspection> {
    let db = bracketed(db);
    let mut items = Vec::new();
    let indexes = connection
        .query(
            &format!(
                "SELECT i.name AS name, CASE WHEN i.is_unique = 1 THEN N'UNIQUE ' ELSE N'' END + i.type_desc COLLATE DATABASE_DEFAULT + N' (' + STRING_AGG(CAST(c.name AS nvarchar(max)), N', ') WITHIN GROUP (ORDER BY ic.key_ordinal) + N')' AS detail \
                 FROM {db}.sys.indexes i JOIN {db}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0 \
                 JOIN {db}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE i.object_id = OBJECT_ID(@P1) AND i.is_primary_key = 0 AND i.is_unique_constraint = 0 AND i.type > 0 \
                 GROUP BY i.name, i.is_unique, i.type_desc ORDER BY i.name"
            ),
            &[Some(object)],
        )
        .await?;
    for row in &indexes {
        items.push(item(
            InspectedKind::Index,
            required(row, "name")?,
            text(row, "detail")?.unwrap_or_default(),
        ));
    }
    let keys = connection
        .query(
            &format!(
                "SELECT k.name AS name, RTRIM(k.type) AS kind, STRING_AGG(CAST(c.name AS nvarchar(max)), N', ') WITHIN GROUP (ORDER BY ic.key_ordinal) AS detail \
                 FROM {db}.sys.key_constraints k JOIN {db}.sys.index_columns ic ON ic.object_id = k.parent_object_id AND ic.index_id = k.unique_index_id \
                 JOIN {db}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE k.parent_object_id = OBJECT_ID(@P1) GROUP BY k.name, k.type ORDER BY k.type, k.name"
            ),
            &[Some(object)],
        )
        .await?;
    for row in &keys {
        let kind = if text(row, "kind")?.as_deref() == Some("PK") {
            InspectedKind::PrimaryKey
        } else {
            InspectedKind::Unique
        };
        items.push(item(
            kind,
            required(row, "name")?,
            text(row, "detail")?.unwrap_or_default(),
        ));
    }
    let checks = connection
        .query(
            &format!(
                "SELECT name, definition AS detail FROM {db}.sys.check_constraints WHERE parent_object_id = OBJECT_ID(@P1) ORDER BY name"
            ),
            &[Some(object)],
        )
        .await?;
    for row in &checks {
        items.push(item(
            InspectedKind::Check,
            required(row, "name")?,
            text(row, "detail")?.unwrap_or_default(),
        ));
    }
    let foreign = connection
        .query(
            &format!(
                "SELECT fk.name AS name, N'(' + STRING_AGG(CAST(pc.name AS nvarchar(max)), N', ') WITHIN GROUP (ORDER BY fkc.constraint_column_id) + N') → ' + QUOTENAME(rs.name) + N'.' + QUOTENAME(ro.name) + N' (' + STRING_AGG(CAST(rc.name AS nvarchar(max)), N', ') WITHIN GROUP (ORDER BY fkc.constraint_column_id) + N')' AS detail \
                 FROM {db}.sys.foreign_keys fk JOIN {db}.sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                 JOIN {db}.sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                 JOIN {db}.sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                 JOIN {db}.sys.objects ro ON ro.object_id = fk.referenced_object_id JOIN {db}.sys.schemas rs ON rs.schema_id = ro.schema_id \
                 WHERE fk.parent_object_id = OBJECT_ID(@P1) GROUP BY fk.name, rs.name, ro.name ORDER BY fk.name"
            ),
            &[Some(object)],
        )
        .await?;
    for row in &foreign {
        items.push(item(
            InspectedKind::ForeignKey,
            required(row, "name")?,
            text(row, "detail")?.unwrap_or_default(),
        ));
    }
    let defaults = connection
        .query(
            &format!(
                "SELECT d.name AS name, c.name + N' = ' + d.definition AS detail FROM {db}.sys.default_constraints d \
                 JOIN {db}.sys.columns c ON c.object_id = d.parent_object_id AND c.column_id = d.parent_column_id \
                 WHERE d.parent_object_id = OBJECT_ID(@P1) ORDER BY d.name"
            ),
            &[Some(object)],
        )
        .await?;
    for row in &defaults {
        items.push(item(
            InspectedKind::Default,
            required(row, "name")?,
            text(row, "detail")?.unwrap_or_default(),
        ));
    }
    let triggers = connection
        .query(
            &format!(
                "SELECT t.name AS name, CASE WHEN t.is_instead_of_trigger = 1 THEN N'INSTEAD OF ' ELSE N'AFTER ' END + \
                 (SELECT STRING_AGG(e.type_desc COLLATE DATABASE_DEFAULT, N', ') FROM {db}.sys.trigger_events e WHERE e.object_id = t.object_id) + \
                 CASE WHEN t.is_disabled = 1 THEN N' (disabled)' ELSE N'' END + N': ' + COALESCE(m.definition, N'') AS detail \
                 FROM {db}.sys.triggers t LEFT JOIN {db}.sys.sql_modules m ON m.object_id = t.object_id \
                 WHERE t.parent_id = OBJECT_ID(@P1) ORDER BY t.name"
            ),
            &[Some(object)],
        )
        .await?;
    for row in &triggers {
        items.push(item(
            InspectedKind::Trigger,
            required(row, "name")?,
            text(row, "detail")?.unwrap_or_default(),
        ));
    }
    Ok(Inspection { items })
}

async fn inspect_schema(
    connection: &mut Connection,
    db: &str,
    schema: &str,
) -> DriverResult<Inspection> {
    let db = bracketed(db);
    let dtype = type_expression("t", "p");
    let mut items = Vec::new();
    let routines = connection
        .query(
            &format!(
                "SELECT o.name AS name, RTRIM(o.type) AS kind, \
                 (SELECT STRING_AGG(CAST(p.name + N' ' + {dtype} + CASE WHEN p.is_output = 1 THEN N' OUTPUT' ELSE N'' END AS nvarchar(max)), N', ') WITHIN GROUP (ORDER BY p.parameter_id) \
                  FROM {db}.sys.parameters p JOIN {db}.sys.types t ON t.user_type_id = p.user_type_id WHERE p.object_id = o.object_id AND p.parameter_id > 0) AS params, \
                 (SELECT {dtype} FROM {db}.sys.parameters p JOIN {db}.sys.types t ON t.user_type_id = p.user_type_id WHERE p.object_id = o.object_id AND p.parameter_id = 0) AS result \
                 FROM {db}.sys.objects o JOIN {db}.sys.schemas s ON s.schema_id = o.schema_id \
                 WHERE s.name = @P1 AND o.type IN ('P', 'PC', 'FN', 'FS', 'IF', 'TF', 'FT') AND o.is_ms_shipped = 0 ORDER BY o.name"
            ),
            &[Some(schema)],
        )
        .await?;
    for row in &routines {
        let kind = if matches!(text(row, "kind")?.as_deref(), Some("P" | "PC")) {
            InspectedKind::Procedure
        } else {
            InspectedKind::Function
        };
        let mut detail = format!("({})", text(row, "params")?.unwrap_or_default());
        if let Some(result) = text(row, "result")? {
            detail.push_str(&format!(" → {result}"));
        }
        items.push(item(kind, required(row, "name")?, detail));
    }
    let sequences = connection
        .query(
            &format!(
                "SELECT q.name AS name, t.name + N' START ' + CAST(q.start_value AS nvarchar(64)) + N' INCREMENT ' + CAST(q.increment AS nvarchar(64)) AS detail \
                 FROM {db}.sys.sequences q JOIN {db}.sys.schemas s ON s.schema_id = q.schema_id JOIN {db}.sys.types t ON t.user_type_id = q.user_type_id \
                 WHERE s.name = @P1 ORDER BY q.name"
            ),
            &[Some(schema)],
        )
        .await?;
    for row in &sequences {
        items.push(item(
            InspectedKind::Sequence,
            required(row, "name")?,
            text(row, "detail")?.unwrap_or_default(),
        ));
    }
    let base = type_expression("b", "t");
    let types = connection
        .query(
            &format!(
                "SELECT t.name AS name, CASE WHEN t.is_table_type = 1 THEN N'table type' ELSE {base} END AS detail \
                 FROM {db}.sys.types t JOIN {db}.sys.schemas s ON s.schema_id = t.schema_id \
                 LEFT JOIN {db}.sys.types b ON b.user_type_id = t.system_type_id \
                 WHERE t.is_user_defined = 1 AND s.name = @P1 ORDER BY t.name"
            ),
            &[Some(schema)],
        )
        .await?;
    for row in &types {
        items.push(item(
            InspectedKind::Type,
            required(row, "name")?,
            text(row, "detail")?.unwrap_or_default(),
        ));
    }
    Ok(Inspection { items })
}

/// A view's source as it was written, or a table's DDL built from the
/// catalog.
pub async fn definition(
    input: &ResolvedConnection,
    path: &ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<String> {
    let (Some(db), Some(schema), Some(relation)) = (
        path.name_of(ObjectKind::Database),
        path.name_of(ObjectKind::Schema),
        path.relation(),
    ) else {
        return Err(DriverError::validation("Choose a table or a view"));
    };
    let object = object_name(db, schema, relation);
    let mut connection = net::connect(input, &cancel).await?;
    net::bounded(&cancel, Some(Duration::from_secs(60)), async {
        let bracketed_db = bracketed(db);
        if path.name_of(ObjectKind::View).is_some() {
            let rows = connection
                .query(
                    &format!(
                        "SELECT definition FROM {bracketed_db}.sys.sql_modules WHERE object_id = OBJECT_ID(@P1)"
                    ),
                    &[Some(&object)],
                )
                .await?;
            return match rows.first() {
                Some(row) => text(row, "definition")?.ok_or_else(|| {
                    DriverError::new(
                        ErrorCode::PermissionDenied,
                        "The view's definition is encrypted or not visible",
                    )
                }),
                None => Err(DriverError::new(ErrorCode::NotFound, "View not found")),
            };
        }
        table_definition(&mut connection, &bracketed_db, &object, schema, relation).await
    })
    .await
}

async fn table_definition(
    connection: &mut Connection,
    db: &str,
    object: &str,
    schema: &str,
    relation: &str,
) -> DriverResult<String> {
    let dtype = type_expression("t", "c");
    let columns = connection
        .query(
            &format!(
                "SELECT c.name AS name, {dtype} AS dtype, c.is_nullable AS nullable, \
                 CASE WHEN ic.object_id IS NULL THEN NULL ELSE N'IDENTITY(' + CAST(ic.seed_value AS nvarchar(64)) + N',' + CAST(ic.increment_value AS nvarchar(64)) + N')' END AS ident, \
                 cc.definition AS computed, d.name AS dname, d.definition AS dflt \
                 FROM {db}.sys.columns c JOIN {db}.sys.types t ON t.user_type_id = c.user_type_id \
                 LEFT JOIN {db}.sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
                 LEFT JOIN {db}.sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
                 LEFT JOIN {db}.sys.default_constraints d ON d.object_id = c.default_object_id \
                 WHERE c.object_id = OBJECT_ID(@P1) ORDER BY c.column_id"
            ),
            &[Some(object)],
        )
        .await?;
    if columns.is_empty() {
        return Err(DriverError::new(ErrorCode::NotFound, "Table not found"));
    }
    let mut lines = Vec::new();
    for row in &columns {
        let name = bracketed(&required(row, "name")?);
        if let Some(computed) = text(row, "computed")? {
            lines.push(format!("    {name} AS {computed}"));
            continue;
        }
        let mut line = format!("    {name} {}", required(row, "dtype")?);
        if let Some(identity) = text(row, "ident")? {
            line.push_str(&format!(" {identity}"));
        }
        if let (Some(constraint), Some(default)) = (text(row, "dname")?, text(row, "dflt")?) {
            line.push_str(&format!(
                " CONSTRAINT {} DEFAULT {default}",
                bracketed(&constraint)
            ));
        }
        let nullable = row
            .try_get::<bool, _>("nullable")
            .map_err(map_error)?
            .unwrap_or(true);
        line.push_str(if nullable { " NULL" } else { " NOT NULL" });
        lines.push(line);
    }
    let keys = connection
        .query(
            &format!(
                "SELECT k.name AS name, CASE WHEN k.type = 'PK' THEN N'PRIMARY KEY ' ELSE N'UNIQUE ' END + i.type_desc COLLATE DATABASE_DEFAULT + N' (' + STRING_AGG(QUOTENAME(c.name), N', ') WITHIN GROUP (ORDER BY ic.key_ordinal) + N')' AS detail \
                 FROM {db}.sys.key_constraints k JOIN {db}.sys.indexes i ON i.object_id = k.parent_object_id AND i.index_id = k.unique_index_id \
                 JOIN {db}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0 \
                 JOIN {db}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE k.parent_object_id = OBJECT_ID(@P1) GROUP BY k.name, k.type, i.type_desc ORDER BY k.type, k.name"
            ),
            &[Some(object)],
        )
        .await?;
    let checks = connection
        .query(
            &format!(
                "SELECT name, N'CHECK ' + definition AS detail FROM {db}.sys.check_constraints WHERE parent_object_id = OBJECT_ID(@P1) ORDER BY name"
            ),
            &[Some(object)],
        )
        .await?;
    let foreign = connection
        .query(
            &format!(
                "SELECT fk.name AS name, N'FOREIGN KEY (' + STRING_AGG(QUOTENAME(pc.name), N', ') WITHIN GROUP (ORDER BY fkc.constraint_column_id) + N') REFERENCES ' + QUOTENAME(rs.name) + N'.' + QUOTENAME(ro.name) + N' (' + STRING_AGG(QUOTENAME(rc.name), N', ') WITHIN GROUP (ORDER BY fkc.constraint_column_id) + N')' AS detail \
                 FROM {db}.sys.foreign_keys fk JOIN {db}.sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                 JOIN {db}.sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                 JOIN {db}.sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                 JOIN {db}.sys.objects ro ON ro.object_id = fk.referenced_object_id JOIN {db}.sys.schemas rs ON rs.schema_id = ro.schema_id \
                 WHERE fk.parent_object_id = OBJECT_ID(@P1) GROUP BY fk.name, rs.name, ro.name ORDER BY fk.name"
            ),
            &[Some(object)],
        )
        .await?;
    for row in keys.iter().chain(&checks).chain(&foreign) {
        if let Some(detail) = text(row, "detail")? {
            lines.push(format!(
                "    CONSTRAINT {} {detail}",
                bracketed(&required(row, "name")?)
            ));
        }
    }
    Ok(format!(
        "CREATE TABLE {}.{} (\n{}\n);",
        bracketed(schema),
        bracketed(relation),
        lines.join(",\n")
    ))
}
