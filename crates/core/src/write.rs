//! Writing changes to a result's rows back to their table
//! ([ADR-0039](../../../docs/adr/0039-editing-result-data-through-the-driver.md)).
//!
//! A driver with [`crate::DriverCapabilities::row_writes`] tells what a
//! result can be written back to ([`crate::Session::edit_target`]) and
//! writes the changes ([`crate::Session::write`]) or generates them as
//! statements for the user to run ([`crate::Session::write_script`]). The
//! text of every statement is built here ([`statement`]), from the
//! driver's quoting and placeholders, so values are never written into it.
use crate::{DriverError, DriverResult, ObjectPath, ParameterKind};
use std::time::Duration;

/// The most changed rows one write may hold (decision E-7).
pub const MAX_CHANGES: usize = 10_000;

/// What a result's rows can be written back to.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    Editable(EditableTable),
    ReadOnly(ReadOnlyReason),
}

impl EditTarget {
    /// The table, when the result is editable.
    pub fn editable(self) -> Option<EditableTable> {
        match self {
            Self::Editable(table) => Some(table),
            Self::ReadOnly(_) => None,
        }
    }
}

/// How changes reach the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// [`crate::Session::write`] writes them, atomically.
    Direct,
    /// [`crate::Session::write_script`] generates the statements, which the
    /// user reviews and runs in a new query tab (ADR-0039, section 9).
    Script,
}

/// Which key identifies a row.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    PrimaryKey,
    /// A unique index whose columns are all NOT NULL, without expressions or
    /// a partial predicate.
    UniqueNotNull,
    /// A primary key the catalog declares but the database does not enforce
    /// (Unity Catalog).
    Informational,
    /// Columns the user named, for a table that declares no key.
    Chosen,
}

/// A table a result's rows can be written back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableTable {
    /// The table as a structural path, resolved from the catalog.
    pub table: ObjectPath,
    pub writes: WriteMode,
    /// One entry per result column: `Some` for a column of `table` (its
    /// [`EditableColumn::writable`] says whether it can be set), `None` for
    /// one that is not (computed, or a type the driver cannot bind there).
    pub columns: Vec<Option<EditableColumn>>,
    /// The result columns, by position, that form the row key, in key order.
    /// Empty only with [`WriteMode::Script`] when the table declares none:
    /// the user then names the key columns.
    pub key: Vec<usize>,
    pub key_kind: KeyKind,
    /// What a write here can change beyond the rows written.
    pub side_effects: Vec<SideEffect>,
}

impl EditableTable {
    /// The table's column at result position `column`.
    pub fn column(&self, column: usize) -> Option<&EditableColumn> {
        self.columns.get(column).and_then(Option::as_ref)
    }

    /// The table's column at result position `column`, when it can be set.
    pub fn writable(&self, column: usize) -> Option<&EditableColumn> {
        self.column(column).filter(|column| column.writable)
    }
}

/// A column of the table that can be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableColumn {
    /// The table column's own name, not the result's alias.
    pub name: String,
    /// The type as the catalog writes it (`numeric(12,2)`, `varchar(80)`).
    /// Only the driver reads it, to bind values.
    pub declared_type: String,
    pub nullable: bool,
    /// It can be set. False for a generated column or an identity the table
    /// always generates, which may still be part of the key.
    pub writable: bool,
    /// An insert may leave it out and get the table's default.
    pub has_default: bool,
    /// Its values are bound as bytes, not text.
    pub binary: bool,
    /// Its text identifies one value, so it may be part of a key the user
    /// names.
    pub can_key: bool,
}

/// What a write to the table can change beyond the rows it writes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffect {
    /// A trigger of the table.
    Trigger { name: String },
    /// A foreign key of another table that references this one and acts on
    /// its rows when a referenced row is deleted or its key updated.
    Reference {
        /// The referencing table, as the catalog names it.
        table: String,
        /// The foreign key's name.
        name: String,
        on_delete: Option<ReferenceAction>,
        on_update: Option<ReferenceAction>,
    },
}

/// What a foreign key does to the rows that reference a changed row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceAction {
    Cascade,
    SetNull,
    SetDefault,
}

/// Why a result cannot be written back: the first rule it failed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// The driver does not report where a column comes from.
    DriverCannotTell,
    /// No column comes from a stored relation.
    ComputedOnly,
    /// The columns come from more than one relation.
    SeveralTables,
    /// The relation is not a table that can be written (a view, a foreign
    /// table); `kind` names what it is.
    NotATable { kind: String },
    /// The table's engine has no transactions (MySQL MyISAM).
    NotTransactional,
    /// The table has no primary key nor a unique key on NOT NULL columns.
    NoKey,
    /// These key columns are not in the result.
    KeyNotInResult { missing: Vec<String> },
    /// This key column's text does not identify one value.
    KeyType { column: String },
    /// The statement is not a SELECT from one table whose columns are plain
    /// references or `*` (ADR-0039, section 9).
    NotASingleTableSelect,
    /// The catalog has no table by the name the SQL gives.
    TableNotFound { name: String },
    /// The result's columns are not the ones the SQL names.
    ColumnsDoNotMatch,
}

/// Changes to write to one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRequest {
    pub target: EditableTable,
    pub changes: Vec<RowChange>,
    /// The tab's statement timeout, for the whole write.
    pub timeout: Option<Duration>,
    /// Whether the write is atomic on its own or joins the transaction
    /// [`crate::Session::begin_writes`] opened (ADR-0040).
    pub scope: WriteScope,
}

/// Which transaction a write runs in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WriteScope {
    /// ADR-0039: the request is atomic on its own (its own transaction, or a
    /// savepoint inside an open one).
    #[default]
    Atomic,
    /// Inside the transaction [`crate::Session::begin_writes`] opened
    /// (ADR-0040): nothing is committed, and a change that fails, affects
    /// another number of rows than one or (MySQL) has a value adjusted fails
    /// the write with that change's index ([`DriverError::change`]),
    /// leaving the transaction for [`crate::Session::end_writes`] to end.
    Caller,
}

/// One row's change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowChange {
    /// `key` holds one value per key column, in key order. `set` pairs a
    /// result column's position with its new value.
    Update {
        key: Vec<WriteValue>,
        set: Vec<(usize, WriteValue)>,
    },
    /// Columns left out take their default.
    Insert {
        values: Vec<(usize, WriteValue)>,
    },
    Delete {
        key: Vec<WriteValue>,
    },
}

/// A value to write, bound by the driver and never written into the SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteValue {
    Null,
    /// The exact text: a cell's text as read, or what the user typed. The
    /// database converts it to the column's type.
    Text(String),
    Bytes(Vec<u8>),
    /// An XLSX boolean (ADR-0040), bound as the database's boolean:
    /// PostgreSQL's `boolean`, `1` or `0` in MySQL and SQLite.
    Boolean(bool),
}

/// How a write ended.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Every change affected exactly one row. `committed` is false when the
    /// session's transaction holds them uncommitted.
    Written { rows: u64, committed: bool },
    /// Change `index` affected `matched` rows, not one. Everything the
    /// request did was rolled back, and nothing of it remains.
    RolledBack { index: usize, matched: u64 },
}

/// The statements [`crate::Session::write_script`] generates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteScript {
    /// One statement per change, in order, each ending with `;`.
    pub statements: Vec<String>,
    /// The values of the statements' `:name` markers. `value: None` is NULL.
    pub parameters: Vec<ScriptParameter>,
}

/// A value of a generated statement's `:name` marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptParameter {
    pub name: String,
    pub kind: ParameterKind,
    pub value: Option<String>,
}

impl WriteRequest {
    /// Checks what can be checked without the database: at least one and at
    /// most [`MAX_CHANGES`] changes, every column named is a writable column
    /// of the target, keys have one value per key column and no NULL, an
    /// update sets something and a column once, an insert sets something.
    pub fn validate(&self) -> DriverResult<()> {
        if self.changes.is_empty() {
            return Err(DriverError::validation("There are no changes to write"));
        }
        if self.changes.len() > MAX_CHANGES {
            return Err(DriverError::validation(format!(
                "A write may hold at most {MAX_CHANGES} changed rows"
            )));
        }
        // An insert needs no key: an import's target has none (ADR-0040).
        let keyed = self
            .changes
            .iter()
            .any(|change| !matches!(change, RowChange::Insert { .. }));
        if keyed && self.target.key.is_empty() {
            return Err(DriverError::validation(
                "Name the key columns that identify a row first",
            ));
        }
        if self
            .target
            .key
            .iter()
            .any(|column| self.target.column(*column).is_none())
        {
            return Err(DriverError::validation(
                "A key column is not a column of the table",
            ));
        }
        if self.timeout.is_some_and(|timeout| timeout.is_zero()) {
            return Err(DriverError::validation("Timeout must be greater than zero"));
        }
        for change in &self.changes {
            match change {
                RowChange::Update { key, set } => {
                    self.check_key(key)?;
                    if set.is_empty() {
                        return Err(DriverError::validation("An update changes no column"));
                    }
                    self.check_columns(set)?;
                }
                RowChange::Insert { values } => {
                    if values.is_empty() {
                        return Err(DriverError::validation(
                            "An inserted row needs at least one value",
                        ));
                    }
                    self.check_columns(values)?;
                }
                RowChange::Delete { key } => self.check_key(key)?,
            }
        }
        Ok(())
    }

    fn check_key(&self, key: &[WriteValue]) -> DriverResult<()> {
        if key.len() != self.target.key.len() {
            return Err(DriverError::validation(
                "A change does not give one value per key column",
            ));
        }
        if key.contains(&WriteValue::Null) {
            return Err(DriverError::validation(
                "A row whose key holds NULL cannot be changed",
            ));
        }
        Ok(())
    }

    fn check_columns(&self, values: &[(usize, WriteValue)]) -> DriverResult<()> {
        for (index, (column, _)) in values.iter().enumerate() {
            if self.target.writable(*column).is_none() {
                return Err(DriverError::validation(
                    "A change writes a column that cannot be written",
                ));
            }
            if values[..index].iter().any(|(other, _)| other == column) {
                return Err(DriverError::validation(
                    "A change writes the same column twice",
                ));
            }
        }
        Ok(())
    }
}

/// One statement of a write: its text, with a placeholder for each value,
/// and the values in the order of their placeholders, each with the result
/// column it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub sql: String,
    pub values: Vec<(usize, WriteValue)>,
}

/// The statement that writes `change` to `table` (its quoted, qualified
/// name):
///
/// - `UPDATE t SET c1 = <v>, … WHERE k1 = <v> AND …`
/// - `DELETE FROM t WHERE k1 = <v> AND …`
/// - `INSERT INTO t (c1, …) VALUES (<v>, …)`
///
/// Column names are the table's own, quoted with `quote`. Each `<v>` is
/// what `placeholder` gives for the value's one-based number and its
/// column: a parameter and the conversion that keeps it exact, never the
/// value. Call after [`WriteRequest::validate`].
pub fn statement(
    table: &str,
    target: &EditableTable,
    change: &RowChange,
    quote: &dyn Fn(&str) -> String,
    placeholder: &mut dyn FnMut(usize, &EditableColumn) -> String,
) -> DriverResult<Statement> {
    struct Builder<'a> {
        target: &'a EditableTable,
        quote: &'a dyn Fn(&str) -> String,
        placeholder: &'a mut dyn FnMut(usize, &EditableColumn) -> String,
        values: Vec<(usize, WriteValue)>,
    }
    impl Builder<'_> {
        /// The quoted name of the column at `position` and the placeholder
        /// of its value, which is recorded.
        fn next(&mut self, position: usize, value: &WriteValue) -> DriverResult<(String, String)> {
            let column = self.target.column(position).ok_or_else(|| {
                DriverError::validation("A change writes a column that cannot be written")
            })?;
            self.values.push((position, value.clone()));
            let bound = (self.placeholder)(self.values.len(), column);
            Ok(((self.quote)(&column.name), bound))
        }

        fn condition(&mut self, key: &[WriteValue]) -> DriverResult<String> {
            let mut parts = Vec::with_capacity(key.len());
            for (position, value) in self.target.key.iter().zip(key) {
                let (name, bound) = self.next(*position, value)?;
                parts.push(format!("{name} = {bound}"));
            }
            Ok(parts.join(" AND "))
        }
    }
    let mut builder = Builder {
        target,
        quote,
        placeholder,
        values: Vec::new(),
    };
    let sql = match change {
        RowChange::Update { key, set } => {
            let mut assignments = Vec::with_capacity(set.len());
            for (position, value) in set {
                let (name, bound) = builder.next(*position, value)?;
                assignments.push(format!("{name} = {bound}"));
            }
            let condition = builder.condition(key)?;
            format!(
                "UPDATE {table} SET {} WHERE {condition}",
                assignments.join(", ")
            )
        }
        RowChange::Delete { key } => {
            let condition = builder.condition(key)?;
            format!("DELETE FROM {table} WHERE {condition}")
        }
        RowChange::Insert { values: inserted } => {
            let mut names = Vec::with_capacity(inserted.len());
            let mut bound = Vec::with_capacity(inserted.len());
            for (position, value) in inserted {
                let (name, placeholder) = builder.next(*position, value)?;
                names.push(name);
                bound.push(placeholder);
            }
            format!(
                "INSERT INTO {table} ({}) VALUES ({})",
                names.join(", "),
                bound.join(", ")
            )
        }
    };
    Ok(Statement {
        sql,
        values: builder.values,
    })
}

/// [`statement`] with `?` for every value: what the confirmation shows.
pub fn preview(
    table: &str,
    target: &EditableTable,
    change: &RowChange,
    quote: &dyn Fn(&str) -> String,
) -> DriverResult<String> {
    statement(table, target, change, quote, &mut |_, _| "?".to_owned()).map(|built| built.sql)
}

/// The one relation the stored columns of a result come from, as the
/// driver identifies it ([`crate::SourceColumn::relation`]), or why there
/// is none: no column says (`DriverCannotTell`), only computed columns
/// (`ComputedOnly`), or several relations (`SeveralTables`).
pub fn single_relation(columns: &[crate::ColumnInfo]) -> Result<&str, ReadOnlyReason> {
    use crate::ColumnOrigin;
    let mut relation: Option<&str> = None;
    let mut unknown = true;
    for column in columns {
        match &column.origin {
            ColumnOrigin::Stored(source) => {
                unknown = false;
                match relation {
                    Some(found) if found != source.relation => {
                        return Err(ReadOnlyReason::SeveralTables);
                    }
                    _ => relation = Some(&source.relation),
                }
            }
            ColumnOrigin::Expression => unknown = false,
            _ => {}
        }
    }
    match relation {
        Some(relation) => Ok(relation),
        None if unknown => Err(ReadOnlyReason::DriverCannotTell),
        None => Err(ReadOnlyReason::ComputedOnly),
    }
}

/// What a driver read from its catalog about a table, for [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTable {
    pub table: ObjectPath,
    /// Every column the driver can bind, in table order, each with the
    /// identifier the result's origins use for it
    /// ([`crate::SourceColumn::column`]).
    pub columns: Vec<(String, EditableColumn)>,
    /// The primary key's column names, in key order; empty without one.
    pub primary_key: Vec<String>,
    /// Unique keys that may identify a row (every column NOT NULL, no
    /// expression, no partial predicate), by column names, in key order.
    pub unique_keys: Vec<Vec<String>>,
    pub side_effects: Vec<SideEffect>,
}

/// The target of a result whose columns come from `catalog`'s table:
/// `sources` holds, per result column, the identifier of the table column it
/// reads ([`crate::SourceColumn::column`]), or `None` for a computed one.
/// The key is the primary key, else the first unique key whose columns are
/// all in the result (decision E-4); a key whose text does not identify one
/// value ([`EditableColumn::can_key`]) keeps the result read-only.
pub fn resolve(catalog: CatalogTable, sources: &[Option<String>], writes: WriteMode) -> EditTarget {
    let columns: Vec<Option<EditableColumn>> = sources
        .iter()
        .map(|source| {
            let source = source.as_ref()?;
            catalog
                .columns
                .iter()
                .find(|(id, _)| id == source)
                .map(|(_, column)| column.clone())
        })
        .collect();
    let (candidates, key_kind) = if catalog.primary_key.is_empty() {
        (catalog.unique_keys.clone(), KeyKind::UniqueNotNull)
    } else {
        (vec![catalog.primary_key.clone()], KeyKind::PrimaryKey)
    };
    let Some(first) = candidates.first() else {
        return EditTarget::ReadOnly(ReadOnlyReason::NoKey);
    };
    let position = |name: &String| {
        columns
            .iter()
            .position(|column| column.as_ref().is_some_and(|column| &column.name == name))
    };
    let found = candidates
        .iter()
        .find_map(|names| names.iter().map(position).collect::<Option<Vec<usize>>>());
    let Some(key) = found else {
        let missing = first
            .iter()
            .filter(|name| position(name).is_none())
            .cloned()
            .collect();
        return EditTarget::ReadOnly(ReadOnlyReason::KeyNotInResult { missing });
    };
    if let Some(column) = key
        .iter()
        .filter_map(|position| columns.get(*position).and_then(Option::as_ref))
        .find(|column| !column.can_key)
    {
        return EditTarget::ReadOnly(ReadOnlyReason::KeyType {
            column: column.name.clone(),
        });
    }
    EditTarget::Editable(EditableTable {
        table: catalog.table,
        writes,
        columns,
        key,
        key_kind,
        side_effects: catalog.side_effects,
    })
}

/// The target an import inserts into (ADR-0040, [`crate::Session::table_target`]):
/// every column of `catalog`'s table, in table order, and no key, since an
/// import only inserts.
pub fn insert_target(catalog: CatalogTable) -> EditTarget {
    let key_kind = if catalog.primary_key.is_empty() {
        KeyKind::UniqueNotNull
    } else {
        KeyKind::PrimaryKey
    };
    EditTarget::Editable(EditableTable {
        table: catalog.table,
        writes: WriteMode::Direct,
        columns: catalog
            .columns
            .into_iter()
            .map(|(_, column)| Some(column))
            .collect(),
        key: Vec::new(),
        key_kind,
        side_effects: catalog.side_effects,
    })
}

/// Whether the table `catalog` describes, read again before a write, is
/// still the one `target` was resolved against: the same path, every
/// column of the target with the same type and as writable, and the key
/// still a key of the table. Otherwise the write fails with [`changed`].
pub fn still_matches(target: &EditableTable, catalog: &CatalogTable) -> bool {
    let columns_match = target.columns.iter().flatten().all(|column| {
        catalog.columns.iter().any(|(_, fresh)| {
            fresh.name == column.name
                && fresh.declared_type == column.declared_type
                && fresh.writable == column.writable
                && fresh.binary == column.binary
        })
    });
    let key: Option<Vec<String>> = target
        .key
        .iter()
        .map(|position| target.column(*position).map(|column| column.name.clone()))
        .collect();
    let key_match = match (target.key_kind, key) {
        (_, None) => false,
        // An insert target has no key to keep.
        (_, Some(key)) if key.is_empty() => true,
        (KeyKind::PrimaryKey, Some(key)) => catalog.primary_key == key,
        (KeyKind::UniqueNotNull, Some(key)) => {
            catalog.primary_key.is_empty() && catalog.unique_keys.contains(&key)
        }
        // A key the database does not enforce: the columns are enough.
        (_, Some(_)) => true,
    };
    catalog.table == target.table && columns_match && key_match
}

/// The error of a write whose table changed since the result was read.
pub fn changed() -> DriverError {
    DriverError::validation("The table changed since this result was read; run it again")
}

/// The bytes of a binary cell's text as the result shows it (`0x` and two
/// hexadecimal digits per byte); `None` for any other text.
pub fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    if digits.len() % 2 != 0 {
        return None;
    }
    (0..digits.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(digits.get(at..at + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectKind;

    fn column(name: &str) -> Option<EditableColumn> {
        Some(EditableColumn {
            name: name.into(),
            declared_type: "text".into(),
            nullable: true,
            writable: true,
            has_default: false,
            binary: false,
            can_key: true,
        })
    }

    fn target() -> EditableTable {
        EditableTable {
            table: ObjectPath::default()
                .child(ObjectKind::Schema, "public")
                .child(ObjectKind::Table, "items"),
            writes: WriteMode::Direct,
            columns: vec![column("id"), None, column("na\"me")],
            key: vec![0],
            key_kind: KeyKind::PrimaryKey,
            side_effects: Vec::new(),
        }
    }

    fn quote(name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    fn text(value: &str) -> WriteValue {
        WriteValue::Text(value.into())
    }

    #[test]
    fn statements_hold_placeholders_and_never_a_value() {
        let target = target();
        let mut numbered = |number: usize, column: &EditableColumn| {
            format!("CAST(${number} AS {})", column.declared_type)
        };
        let update = RowChange::Update {
            key: vec![text("7")],
            set: vec![(2, text("O'Brien; DROP TABLE x"))],
        };
        let built = statement(
            "\"public\".\"items\"",
            &target,
            &update,
            &quote,
            &mut numbered,
        )
        .unwrap();
        assert_eq!(
            built.sql,
            "UPDATE \"public\".\"items\" SET \"na\"\"me\" = CAST($1 AS text) WHERE \"id\" = CAST($2 AS text)"
        );
        assert_eq!(
            built.values,
            [(2, text("O'Brien; DROP TABLE x")), (0, text("7"))]
        );
        assert!(!built.sql.contains("O'Brien"));
        let insert = RowChange::Insert {
            values: vec![(0, text("8")), (2, WriteValue::Null)],
        };
        assert_eq!(
            preview("t", &target, &insert, &quote).unwrap(),
            "INSERT INTO t (\"id\", \"na\"\"me\") VALUES (?, ?)"
        );
        let delete = RowChange::Delete {
            key: vec![text("9")],
        };
        assert_eq!(
            preview("t", &target, &delete, &quote).unwrap(),
            "DELETE FROM t WHERE \"id\" = ?"
        );
    }

    #[test]
    fn requests_are_checked_before_anything_runs() {
        let request = |changes: Vec<RowChange>| WriteRequest {
            target: target(),
            changes,
            timeout: None,
            scope: WriteScope::Atomic,
        };
        assert!(request(vec![]).validate().is_err());
        assert!(
            request(vec![RowChange::Delete {
                key: vec![WriteValue::Null]
            }])
            .validate()
            .is_err(),
            "a NULL key identifies no row"
        );
        assert!(
            request(vec![RowChange::Update {
                key: vec![text("1")],
                set: vec![(1, text("x"))]
            }])
            .validate()
            .is_err(),
            "a computed column is not written"
        );
        assert!(
            request(vec![RowChange::Update {
                key: vec![text("1")],
                set: vec![(2, text("x")), (2, text("y"))]
            }])
            .validate()
            .is_err()
        );
        assert!(
            request(vec![RowChange::Insert { values: vec![] }])
                .validate()
                .is_err()
        );
        let many = (0..=MAX_CHANGES)
            .map(|_| RowChange::Delete {
                key: vec![text("1")],
            })
            .collect();
        assert!(request(many).validate().is_err());
        assert!(
            request(vec![RowChange::Update {
                key: vec![text("1")],
                set: vec![(2, WriteValue::Null)]
            }])
            .validate()
            .is_ok()
        );
        let mut chosen = request(vec![RowChange::Delete {
            key: vec![text("1")],
        }]);
        chosen.target.key.clear();
        assert!(chosen.validate().is_err(), "a write needs a key");
    }

    fn stored(relation: &str, column: &str) -> crate::ColumnInfo {
        let mut info = crate::ColumnInfo::new(column, "text");
        info.origin = crate::ColumnOrigin::Stored(crate::SourceColumn {
            relation: relation.into(),
            column: column.into(),
        });
        info
    }

    #[test]
    fn a_result_comes_from_one_relation_or_says_why_not() {
        let computed = {
            let mut info = crate::ColumnInfo::new("n", "int");
            info.origin = crate::ColumnOrigin::Expression;
            info
        };
        assert_eq!(
            single_relation(&[stored("1", "a"), computed.clone(), stored("1", "b")]),
            Ok("1")
        );
        assert_eq!(
            single_relation(&[stored("1", "a"), stored("2", "b")]),
            Err(ReadOnlyReason::SeveralTables)
        );
        assert_eq!(
            single_relation(std::slice::from_ref(&computed)),
            Err(ReadOnlyReason::ComputedOnly)
        );
        assert_eq!(
            single_relation(&[crate::ColumnInfo::new("x", "int")]),
            Err(ReadOnlyReason::DriverCannotTell)
        );
    }

    fn catalog() -> CatalogTable {
        let mut real = column("score").unwrap();
        real.can_key = false;
        CatalogTable {
            table: target().table,
            columns: vec![
                ("1".into(), column("id").unwrap()),
                ("2".into(), column("code").unwrap()),
                ("3".into(), real),
            ],
            primary_key: vec!["id".into()],
            unique_keys: vec![vec!["code".into()]],
            side_effects: vec![],
        }
    }

    #[test]
    fn the_key_is_the_primary_key_else_a_unique_one_in_the_result() {
        let sources = |ids: &[Option<&str>]| -> Vec<Option<String>> {
            ids.iter().map(|id| id.map(str::to_owned)).collect()
        };
        let table = resolve(
            catalog(),
            &sources(&[None, Some("2"), Some("1")]),
            WriteMode::Direct,
        )
        .editable()
        .unwrap();
        assert_eq!(table.key, [2]);
        assert_eq!(table.key_kind, KeyKind::PrimaryKey);
        assert!(table.columns[0].is_none());
        assert!(still_matches(&table, &catalog()));
        let mut dropped = catalog();
        dropped.primary_key.clear();
        assert!(!still_matches(&table, &dropped), "the key is gone");
        let mut retyped = catalog();
        retyped.columns[1].1.declared_type = "integer".into();
        assert!(!still_matches(&table, &retyped));

        assert_eq!(
            resolve(catalog(), &sources(&[Some("2")]), WriteMode::Direct),
            EditTarget::ReadOnly(ReadOnlyReason::KeyNotInResult {
                missing: vec!["id".into()]
            })
        );
        let mut unique_only = catalog();
        unique_only.primary_key.clear();
        let table = resolve(
            unique_only.clone(),
            &sources(&[Some("2")]),
            WriteMode::Direct,
        )
        .editable()
        .unwrap();
        assert_eq!(
            (table.key, table.key_kind),
            (vec![0], KeyKind::UniqueNotNull)
        );
        unique_only.unique_keys = vec![vec!["score".into()]];
        assert_eq!(
            resolve(
                unique_only.clone(),
                &sources(&[Some("3")]),
                WriteMode::Direct
            ),
            EditTarget::ReadOnly(ReadOnlyReason::KeyType {
                column: "score".into()
            })
        );
        unique_only.unique_keys.clear();
        assert_eq!(
            resolve(unique_only, &sources(&[Some("1")]), WriteMode::Direct),
            EditTarget::ReadOnly(ReadOnlyReason::NoKey)
        );
    }

    #[test]
    fn an_import_target_has_every_column_and_no_key() {
        let target = insert_target(catalog()).editable().unwrap();
        assert!(target.key.is_empty());
        assert_eq!(target.columns.len(), 3);
        assert!(still_matches(&target, &catalog()), "no key to keep");
        let request = |changes: Vec<RowChange>| WriteRequest {
            target: target.clone(),
            changes,
            timeout: None,
            scope: WriteScope::Caller,
        };
        assert!(
            request(vec![RowChange::Insert {
                values: vec![(0, WriteValue::Boolean(true))]
            }])
            .validate()
            .is_ok(),
            "inserts need no key"
        );
        assert!(
            request(vec![RowChange::Delete {
                key: vec![text("1")]
            }])
            .validate()
            .is_err(),
            "anything else does"
        );
        let error = DriverError::validation("x").at_change(41);
        assert_eq!(error.change(), Some(41));
        assert_eq!(DriverError::validation("x").change(), None);
        assert!(
            std::mem::size_of::<DriverError>() < 128,
            "an error stays small"
        );
    }

    #[test]
    fn binary_text_reads_back_as_its_bytes() {
        assert_eq!(hex_bytes("0x00abFF"), Some(vec![0, 0xab, 0xff]));
        assert_eq!(hex_bytes("0x"), Some(vec![]));
        assert_eq!(hex_bytes("0xabc"), None);
        assert_eq!(hex_bytes("abcd"), None);
        assert_eq!(hex_bytes("0xzz"), None);
    }
}
