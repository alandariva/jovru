//! Editing a result in the grid, outside the views
//! ([ADR-0039](../docs/adr/0039-editing-result-data-through-the-driver.md)).
//!
//! An [`EditSession`] holds a result tab's pending edits over the rows it
//! read: changed cells, rows marked for deletion and inserted rows. Nothing
//! is written while they are pending; [`EditSession::request`] turns them
//! into the driver's [`WriteRequest`], and the outcome of the write comes
//! back to [`EditSession::written`] or [`EditSession::rolled_back`].
//!
//! Rows are addressed by their position in the whole result (the page's
//! first row plus the row in the page), so the edits of every page are kept.
//! The values of a row's key are taken when the row is first edited, so a
//! write never needs the page again. Pending edits live in memory only.
use crate::drivers::write::{
    EditableTable, MAX_CHANGES, ReadOnlyReason, RowChange, WriteRequest, WriteValue, hex_bytes,
};
use crate::tr;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// A value the user gave a cell. NULL is chosen with **Set to NULL**; typing
/// `NULL` gives the four-letter text, and clearing a cell the empty text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditValue {
    Null,
    Text(String),
}

impl EditValue {
    /// The text the cell shows, `None` for NULL.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Null => None,
            Self::Text(text) => Some(text),
        }
    }
}

/// Where a change comes from: a row the result read, or an inserted one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    /// The row's position in the whole result.
    Row(usize),
    /// The position among the inserted rows.
    Inserted(usize),
}

/// Why an edit, or a write, was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The column cannot be set: computed, generated, or not of the table.
    ReadOnlyColumn { column: String },
    /// The column's cells arrived both as bytes and as text.
    MixedColumn { column: String },
    /// The row's key holds NULL, so no statement can name it.
    KeyIsNull,
    /// A binary column takes `0x` and hexadecimal digits.
    NotBytes { column: String },
    /// The row is marked for deletion.
    Deleted,
    /// The table declares no key: the user names the key columns first.
    NeedsKey,
    /// A key the user named cannot identify a row by this column.
    NotAKeyColumn { column: String },
    /// The key can be named only before the first edit.
    KeyAfterEdits,
    /// More changed rows than one write may hold.
    TooMany { changes: usize },
    /// An inserted row has no value at all.
    EmptyInsert { row: usize },
    /// There is nothing to write.
    Nothing,
    /// An inserted row the last write wrote: read the rows again to edit it.
    AlreadyWritten,
}

/// Why a result is read-only, in words (ADR-0039, section 1).
pub fn read_only_text(reason: &ReadOnlyReason) -> String {
    match reason {
        ReadOnlyReason::DriverCannotTell => tr!(
            "This connection's results cannot be edited: its driver does not report which table a column comes from."
        )
        .into(),
        ReadOnlyReason::ComputedOnly => tr!(
            "This result cannot be edited: its columns are computed, not read from a table."
        )
        .into(),
        ReadOnlyReason::SeveralTables => {
            tr!("This result cannot be edited: its columns come from more than one table.").into()
        }
        ReadOnlyReason::NotATable { kind } => tr!(
            "This result cannot be edited: it reads a {0}, not a table.",
            kind
        ),
        ReadOnlyReason::NotTransactional => tr!(
            "This result cannot be edited: its table's engine has no transactions, so a failed write could not be undone."
        )
        .into(),
        ReadOnlyReason::NoKey => tr!(
            "This result cannot be edited: its table has no primary key and no unique key on NOT NULL columns."
        )
        .into(),
        ReadOnlyReason::KeyNotInResult { missing } => tr!(
            "This result cannot be edited: the key columns {0} are not in it. Select them too.",
            missing.join(", ")
        ),
        ReadOnlyReason::KeyType { column } => tr!(
            "This result cannot be edited: the key column {0} holds values whose text does not identify one row (an approximate number or a time-zone-dependent time).",
            column
        ),
        ReadOnlyReason::NotASingleTableSelect => tr!(
            "This result cannot be edited: only a SELECT of plain columns from one table, without joins, grouping or computed columns, can be edited here."
        )
        .into(),
        ReadOnlyReason::TableNotFound { name } => tr!(
            "This result cannot be edited: the catalog has no table {0}.",
            name
        ),
        ReadOnlyReason::ColumnsDoNotMatch => tr!(
            "This result cannot be edited: its columns are not the table columns the SQL names."
        )
        .into(),
        _ => tr!("This result cannot be edited.").into(),
    }
}

/// Why an edit or a write was refused, in words.
pub fn refused_text(refused: &Refused) -> String {
    match refused {
        Refused::ReadOnlyColumn { column } => tr!(
            "The column {0} cannot be edited: it is computed or generated, or its type cannot be written here.",
            column
        ),
        Refused::MixedColumn { column } => tr!(
            "The column {0} cannot be edited: its values arrived both as bytes and as text.",
            column
        ),
        Refused::KeyIsNull => tr!(
            "This row cannot be changed: its key is NULL, so no statement can name it."
        )
        .into(),
        Refused::NotBytes { column } => tr!(
            "The column {0} holds bytes: type 0x followed by hexadecimal digits.",
            column
        ),
        Refused::Deleted => {
            tr!("This row is marked for deletion. Delete it again to keep it.").into()
        }
        Refused::NeedsKey => tr!(
            "This table declares no key. Select the cells of the columns that identify a row and choose them as the key first."
        )
        .into(),
        Refused::NotAKeyColumn { column } => tr!(
            "The column {0} cannot identify a row: its values cannot be compared exactly.",
            column
        ),
        Refused::KeyAfterEdits => {
            tr!("The key can be chosen only before the first edit. Discard the edits first.").into()
        }
        Refused::TooMany { changes } => tr!(
            "{0} changed rows are more than one write may hold ({1}). Write fewer at a time.",
            crate::cell_format::count(*changes),
            crate::cell_format::count(MAX_CHANGES)
        ),
        Refused::EmptyInsert { row } => tr!(
            "Inserted row {0} has no value. Give it one, or delete it.",
            row
        ),
        Refused::Nothing => tr!("There are no changes to write.").into(),
        Refused::AlreadyWritten => {
            tr!("This row was written. Read the rows again (↻) to edit it.").into()
        }
    }
}

/// How a row of the result is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMark {
    Unchanged,
    /// Some of its cells have pending values.
    Changed,
    /// Marked for deletion.
    Deleted,
    /// Written: its cells show what was written.
    Written,
    /// Deleted by the last write.
    WrittenDeleted,
    /// The change the last write rolled back for: it matched other than
    /// one row.
    Failed,
}

/// What a cell shows over the value read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay<'a> {
    /// A value not written yet.
    Pending(&'a EditValue),
    /// The value the last write wrote, which the database may have stored
    /// differently (↻ reads it back).
    Written(&'a EditValue),
}

impl<'a> Overlay<'a> {
    pub fn value(self) -> &'a EditValue {
        match self {
            Self::Pending(value) | Self::Written(value) => value,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct RowEdit {
    /// The key's values when the row was first edited, in key order.
    key: Vec<WriteValue>,
    /// The key as text, for the confirmation.
    key_text: Vec<Option<String>>,
    /// Column → (value read, new value).
    cells: BTreeMap<usize, (Option<String>, EditValue)>,
    deleted: bool,
}

/// One line of the confirmation: what a change does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeLine {
    pub kind: ChangeKind,
    /// `column = value` pairs of the key, as read; empty for an insert.
    pub key: Vec<(String, Option<String>)>,
    /// Column, value read (`None` for an insert) and new value.
    pub cells: Vec<(String, Option<Option<String>>, EditValue)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Update,
    Delete,
    Insert,
}

/// A result tab's pending edits over its table.
#[derive(Clone, Debug)]
pub struct EditSession {
    target: EditableTable,
    /// How each column's cells arrived on the pages read
    /// ([`crate::result::ResultColumn::binary`]).
    binary: Vec<Option<bool>>,
    rows: BTreeMap<usize, RowEdit>,
    inserted: Vec<BTreeMap<usize, EditValue>>,
    /// What the last write wrote, drawn until the rows are read again.
    written_rows: BTreeMap<usize, RowEdit>,
    written_inserted: Vec<BTreeMap<usize, EditValue>>,
    failed: Option<Origin>,
    /// Bumped with every change, for the grid's cache.
    version: u64,
}

impl EditSession {
    /// Edits over a result whose rows `target` names, whose columns' cells
    /// arrived as `binary` says.
    pub fn new(target: EditableTable, binary: Vec<Option<bool>>) -> Self {
        Self {
            target,
            binary,
            rows: BTreeMap::new(),
            inserted: Vec::new(),
            written_rows: BTreeMap::new(),
            written_inserted: Vec::new(),
            failed: None,
            version: 1,
        }
    }

    pub fn target(&self) -> &EditableTable {
        &self.target
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// The table declares no key and the user has not named one yet.
    pub fn needs_key(&self) -> bool {
        self.target.key.is_empty()
    }

    /// Names `columns` as the key, for a table that declares none
    /// ([`crate::drivers::write::KeyKind::Chosen`]); only before any edit.
    pub fn set_key(&mut self, columns: Vec<usize>) -> Result<(), Refused> {
        if self.pending() > 0 {
            return Err(Refused::KeyAfterEdits);
        }
        if columns.is_empty() {
            return Err(Refused::NeedsKey);
        }
        for column in &columns {
            let name = self.column_name(*column);
            match self.target.column(*column) {
                Some(table_column) if table_column.can_key => {}
                _ => return Err(Refused::NotAKeyColumn { column: name }),
            }
        }
        self.target.key = columns;
        self.bump();
        Ok(())
    }

    /// Changed rows waiting to be written: rows with new values or marked
    /// for deletion, and inserted rows.
    pub fn pending(&self) -> usize {
        self.rows.len() + self.inserted.len()
    }

    /// Rows inserted and not written yet; the grid draws them after the
    /// result's rows.
    pub fn inserted(&self) -> usize {
        self.inserted.len()
    }

    fn bump(&mut self) {
        self.version += 1;
    }

    fn column_name(&self, column: usize) -> String {
        self.target
            .column(column)
            .map(|column| column.name.clone())
            .unwrap_or_default()
    }

    /// Whether the column's values travel as bytes: the table's column is
    /// binary, or its cells arrived as bytes.
    fn binary_column(&self, column: usize) -> bool {
        self.target
            .column(column)
            .is_some_and(|column| column.binary)
            || self.binary.get(column) == Some(&Some(true))
    }

    /// Whether `column` can be given a value here.
    pub fn editable_column(&self, column: usize) -> Result<(), Refused> {
        let name = self.column_name(column);
        if self.target.writable(column).is_none() {
            return Err(Refused::ReadOnlyColumn { column: name });
        }
        if self.binary.get(column) == Some(&None) {
            return Err(Refused::MixedColumn { column: name });
        }
        Ok(())
    }

    /// The value written for a cell's text, checked: bytes for a binary
    /// column.
    fn write_value(&self, column: usize, value: &EditValue) -> Result<WriteValue, Refused> {
        match value {
            EditValue::Null => Ok(WriteValue::Null),
            EditValue::Text(text) if self.binary_column(column) => hex_bytes(text)
                .map(WriteValue::Bytes)
                .ok_or_else(|| Refused::NotBytes {
                    column: self.column_name(column),
                }),
            EditValue::Text(text) => Ok(WriteValue::Text(text.clone())),
        }
    }

    /// The row's key as written, from its values as read.
    fn key_of(
        &self,
        read: &dyn Fn(usize) -> Option<String>,
    ) -> Result<(Vec<WriteValue>, Vec<Option<String>>), Refused> {
        if self.needs_key() {
            return Err(Refused::NeedsKey);
        }
        let mut key = Vec::with_capacity(self.target.key.len());
        let mut text = Vec::with_capacity(self.target.key.len());
        for column in &self.target.key {
            let Some(value) = read(*column) else {
                return Err(Refused::KeyIsNull);
            };
            if self.binary.get(*column) == Some(&None) {
                return Err(Refused::MixedColumn {
                    column: self.column_name(*column),
                });
            }
            key.push(self.write_value(*column, &EditValue::Text(value.clone()))?);
            text.push(Some(value));
        }
        Ok((key, text))
    }

    /// Gives result row `row` (in the whole result) the value `value` in
    /// `column`. `read` gives the row's values as read (`None` for NULL). A
    /// value equal to the one read drops the pending one.
    pub fn set(
        &mut self,
        row: usize,
        read: &dyn Fn(usize) -> Option<String>,
        column: usize,
        value: EditValue,
    ) -> Result<(), Refused> {
        self.editable_column(column)?;
        self.write_value(column, &value)?;
        if self.rows.get(&row).is_some_and(|edit| edit.deleted) {
            return Err(Refused::Deleted);
        }
        let before = read(column);
        let unchanged = matches!((&before, &value), (Some(read), EditValue::Text(text)) if read == text)
            || (before.is_none() && value == EditValue::Null);
        if !self.rows.contains_key(&row) {
            if unchanged {
                return Ok(());
            }
            let (key, key_text) = self.key_of(read)?;
            self.rows.insert(
                row,
                RowEdit {
                    key,
                    key_text,
                    ..RowEdit::default()
                },
            );
        }
        if let Some(edit) = self.rows.get_mut(&row) {
            if unchanged {
                edit.cells.remove(&column);
            } else {
                edit.cells.insert(column, (before, value));
            }
            if edit.cells.is_empty() && !edit.deleted {
                self.rows.remove(&row);
            }
        }
        self.failed = None;
        self.bump();
        Ok(())
    }

    /// Gives inserted row `index` the value `value` in `column`.
    pub fn set_inserted(
        &mut self,
        index: usize,
        column: usize,
        value: EditValue,
    ) -> Result<(), Refused> {
        self.editable_column(column)?;
        self.write_value(column, &value)?;
        let Some(row) = self.inserted.get_mut(index) else {
            return Err(Refused::AlreadyWritten);
        };
        row.insert(column, value);
        self.failed = None;
        self.bump();
        Ok(())
    }

    /// Marks result row `row` for deletion, dropping its pending values, or
    /// takes the mark away.
    pub fn toggle_delete(
        &mut self,
        row: usize,
        read: &dyn Fn(usize) -> Option<String>,
    ) -> Result<(), Refused> {
        match self.rows.get_mut(&row) {
            Some(edit) if edit.deleted => {
                self.rows.remove(&row);
            }
            Some(edit) => {
                edit.deleted = true;
                edit.cells.clear();
            }
            None => {
                let (key, key_text) = self.key_of(read)?;
                self.rows.insert(
                    row,
                    RowEdit {
                        key,
                        key_text,
                        cells: BTreeMap::new(),
                        deleted: true,
                    },
                );
            }
        }
        self.failed = None;
        self.bump();
        Ok(())
    }

    /// Adds an empty row to insert; its columns take the table's default
    /// until given a value. Returns its index.
    pub fn insert_row(&mut self) -> usize {
        self.inserted.push(BTreeMap::new());
        self.bump();
        self.inserted.len() - 1
    }

    /// Takes inserted row `index` away.
    pub fn remove_inserted(&mut self, index: usize) {
        if index < self.inserted.len() {
            self.inserted.remove(index);
            self.failed = None;
            self.bump();
        }
    }

    /// Drops every pending edit.
    pub fn discard(&mut self) {
        self.rows.clear();
        self.inserted.clear();
        self.failed = None;
        self.bump();
    }

    /// What result row `row`'s cell in `column` shows over the value read.
    pub fn overlay(&self, row: usize, column: usize) -> Option<Overlay<'_>> {
        if let Some((_, value)) = self.rows.get(&row).and_then(|edit| edit.cells.get(&column)) {
            return Some(Overlay::Pending(value));
        }
        self.written_rows
            .get(&row)
            .and_then(|edit| edit.cells.get(&column))
            .map(|(_, value)| Overlay::Written(value))
    }

    /// What inserted row `index` (pending, then those written) shows in
    /// `column`; `None` where it takes the table's default.
    pub fn inserted_value(&self, index: usize, column: usize) -> Option<Overlay<'_>> {
        if let Some(row) = self.inserted.get(index) {
            return row.get(&column).map(Overlay::Pending);
        }
        self.written_inserted
            .get(index - self.inserted.len())
            .and_then(|row| row.get(&column))
            .map(Overlay::Written)
    }

    /// Inserted rows drawn after the result's: pending, then written.
    pub fn drawn_inserted(&self) -> usize {
        self.inserted.len() + self.written_inserted.len()
    }

    /// How result row `row` is drawn.
    pub fn row_mark(&self, row: usize) -> RowMark {
        if self.failed == Some(Origin::Row(row)) {
            return RowMark::Failed;
        }
        match (self.rows.get(&row), self.written_rows.get(&row)) {
            (Some(edit), _) if edit.deleted => RowMark::Deleted,
            (Some(_), _) => RowMark::Changed,
            (None, Some(edit)) if edit.deleted => RowMark::WrittenDeleted,
            (None, Some(_)) => RowMark::Written,
            (None, None) => RowMark::Unchanged,
        }
    }

    /// How inserted row `index` is drawn.
    pub fn inserted_mark(&self, index: usize) -> RowMark {
        if self.failed == Some(Origin::Inserted(index)) {
            RowMark::Failed
        } else if index < self.inserted.len() {
            RowMark::Changed
        } else {
            RowMark::Written
        }
    }

    /// The changes in the order they are written: deletions, then updates,
    /// then inserts, with where each comes from.
    fn ordered(&self) -> Vec<(Origin, &RowEdit)> {
        let deleted = self
            .rows
            .iter()
            .filter(|(_, edit)| edit.deleted)
            .map(|(row, edit)| (Origin::Row(*row), edit));
        let updated = self
            .rows
            .iter()
            .filter(|(_, edit)| !edit.deleted)
            .map(|(row, edit)| (Origin::Row(*row), edit));
        deleted.chain(updated).collect()
    }

    /// The request that writes the pending edits, and where each of its
    /// changes comes from, in order: deletions first (so an update or an
    /// insert can take a deleted row's key), then updates, then inserts.
    pub fn request(
        &self,
        timeout: Option<Duration>,
    ) -> Result<(WriteRequest, Vec<Origin>), Refused> {
        let total = self.pending();
        if total == 0 {
            return Err(Refused::Nothing);
        }
        if total > MAX_CHANGES {
            return Err(Refused::TooMany { changes: total });
        }
        if self.needs_key() {
            return Err(Refused::NeedsKey);
        }
        let mut changes = Vec::with_capacity(total);
        let mut origins = Vec::with_capacity(total);
        for (origin, edit) in self.ordered() {
            let change = if edit.deleted {
                RowChange::Delete {
                    key: edit.key.clone(),
                }
            } else {
                let mut set = Vec::with_capacity(edit.cells.len());
                for (column, (_, value)) in &edit.cells {
                    set.push((*column, self.write_value(*column, value)?));
                }
                RowChange::Update {
                    key: edit.key.clone(),
                    set,
                }
            };
            changes.push(change);
            origins.push(origin);
        }
        for (index, row) in self.inserted.iter().enumerate() {
            if row.is_empty() {
                return Err(Refused::EmptyInsert { row: index + 1 });
            }
            let mut values = Vec::with_capacity(row.len());
            for (column, value) in row {
                values.push((*column, self.write_value(*column, value)?));
            }
            changes.push(RowChange::Insert { values });
            origins.push(Origin::Inserted(index));
        }
        Ok((
            WriteRequest {
                target: self.target.clone(),
                changes,
                timeout,
                scope: crate::drivers::write::WriteScope::Atomic,
            },
            origins,
        ))
    }

    /// The pending edits as the confirmation lists them, in the order they
    /// are written, at most `limit` of them.
    pub fn lines(&self, limit: usize) -> Vec<ChangeLine> {
        let name = |column: usize| self.column_name(column);
        let key = |edit: &RowEdit| -> Vec<(String, Option<String>)> {
            self.target
                .key
                .iter()
                .zip(&edit.key_text)
                .map(|(column, value)| (name(*column), value.clone()))
                .collect()
        };
        let mut lines: Vec<ChangeLine> = self
            .ordered()
            .into_iter()
            .take(limit)
            .map(|(_, edit)| ChangeLine {
                kind: if edit.deleted {
                    ChangeKind::Delete
                } else {
                    ChangeKind::Update
                },
                key: key(edit),
                cells: edit
                    .cells
                    .iter()
                    .map(|(column, (before, after))| {
                        (name(*column), Some(before.clone()), after.clone())
                    })
                    .collect(),
            })
            .collect();
        let room = limit.saturating_sub(lines.len());
        lines.extend(self.inserted.iter().take(room).map(|row| {
            ChangeLine {
                kind: ChangeKind::Insert,
                key: Vec::new(),
                cells: row
                    .iter()
                    .map(|(column, value)| (name(*column), None, value.clone()))
                    .collect(),
            }
        }));
        lines
    }

    /// The write succeeded: the pending edits become what the grid shows as
    /// written, until the rows are read again.
    pub fn written(&mut self) {
        let rows = std::mem::take(&mut self.rows);
        for (row, edit) in rows {
            self.written_rows.insert(row, edit);
        }
        let inserted = std::mem::take(&mut self.inserted);
        self.written_inserted.extend(inserted);
        self.failed = None;
        self.bump();
    }

    /// The write was rolled back because change `index` of the request
    /// matched other than one row: that change's row is marked, and every
    /// edit stays pending.
    pub fn rolled_back(&mut self, origins: &[Origin], index: usize) {
        self.failed = origins.get(index).copied();
        self.bump();
    }

    /// Where the row a write rolled back for is, when one is marked.
    pub fn failed(&self) -> Option<Origin> {
        self.failed
    }

    /// The rows of the result with pending edits.
    pub fn edited_rows(&self) -> BTreeSet<usize> {
        self.rows.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::write::{EditableColumn, KeyKind, WriteMode};
    use crate::drivers::{ObjectKind, ObjectPath};

    fn column(name: &str, writable: bool, binary: bool) -> Option<EditableColumn> {
        Some(EditableColumn {
            name: name.into(),
            declared_type: "text".into(),
            nullable: true,
            writable,
            has_default: true,
            binary,
            can_key: true,
        })
    }

    /// `id` (the key), `note`, a computed column and `raw` (bytes).
    fn session() -> EditSession {
        EditSession::new(
            EditableTable {
                table: ObjectPath::default().child(ObjectKind::Table, "items"),
                writes: WriteMode::Direct,
                columns: vec![
                    column("id", true, false),
                    column("note", true, false),
                    None,
                    column("raw", true, true),
                ],
                key: vec![0],
                key_kind: KeyKind::PrimaryKey,
                side_effects: vec![],
            },
            vec![Some(false), Some(false), Some(false), Some(true)],
        )
    }

    fn row(values: &[Option<&str>]) -> impl Fn(usize) -> Option<String> + use<> {
        let values: Vec<Option<String>> = values.iter().map(|v| v.map(str::to_owned)).collect();
        move |column| values.get(column).cloned().flatten()
    }

    fn text(value: &str) -> EditValue {
        EditValue::Text(value.into())
    }

    #[test]
    fn null_empty_text_and_the_text_null_stay_apart() {
        let mut edits = session();
        let one = row(&[Some("1"), Some("a"), None, None]);
        let two = row(&[Some("2"), Some("b"), None, None]);
        let three = row(&[Some("3"), None, None, None]);
        edits.set(0, &one, 1, EditValue::Null).unwrap();
        edits.set(1, &two, 1, text("")).unwrap();
        edits.set(2, &three, 1, text("NULL")).unwrap();
        let (request, origins) = edits.request(None).unwrap();
        assert_eq!(origins, [Origin::Row(0), Origin::Row(1), Origin::Row(2)]);
        let set: Vec<WriteValue> = request
            .changes
            .iter()
            .map(|change| match change {
                RowChange::Update { set, .. } => set[0].1.clone(),
                _ => WriteValue::Bytes(vec![]),
            })
            .collect();
        assert_eq!(
            set,
            [
                WriteValue::Null,
                WriteValue::Text(String::new()),
                WriteValue::Text("NULL".into())
            ]
        );
        assert_eq!(
            edits.overlay(0, 1),
            Some(Overlay::Pending(&EditValue::Null))
        );
    }

    #[test]
    fn a_value_back_to_what_was_read_is_no_change() {
        let mut edits = session();
        let one = row(&[Some("1"), Some("a"), None, None]);
        edits.set(0, &one, 1, text("b")).unwrap();
        assert_eq!(edits.pending(), 1);
        edits.set(0, &one, 1, text("a")).unwrap();
        assert_eq!(edits.pending(), 0);
        // A NULL set to NULL is no change either.
        edits.set(0, &one, 3, EditValue::Null).unwrap();
        assert_eq!(edits.pending(), 0);
        assert_eq!(edits.request(None).unwrap_err(), Refused::Nothing);
    }

    #[test]
    fn deleting_a_row_drops_its_pending_values_and_deletions_come_first() {
        let mut edits = session();
        let one = row(&[Some("1"), Some("a"), None, None]);
        let two = row(&[Some("2"), Some("b"), None, None]);
        edits.set(0, &one, 1, text("x")).unwrap();
        edits.set(1, &two, 1, text("y")).unwrap();
        edits.toggle_delete(1, &two).unwrap();
        assert_eq!(edits.row_mark(1), RowMark::Deleted);
        assert_eq!(edits.overlay(1, 1), None);
        assert_eq!(edits.set(1, &two, 1, text("z")), Err(Refused::Deleted));
        let inserted = edits.insert_row();
        edits.set_inserted(inserted, 0, text("9")).unwrap();
        let (request, origins) = edits.request(None).unwrap();
        assert_eq!(
            origins,
            [Origin::Row(1), Origin::Row(0), Origin::Inserted(0)]
        );
        assert_eq!(
            request.changes,
            [
                RowChange::Delete {
                    key: vec![WriteValue::Text("2".into())]
                },
                RowChange::Update {
                    key: vec![WriteValue::Text("1".into())],
                    set: vec![(1, WriteValue::Text("x".into()))]
                },
                RowChange::Insert {
                    values: vec![(0, WriteValue::Text("9".into()))]
                },
            ]
        );
        // Undoing the deletion leaves nothing pending for that row.
        edits.toggle_delete(1, &two).unwrap();
        assert_eq!(edits.row_mark(1), RowMark::Unchanged);
    }

    #[test]
    fn what_cannot_be_written_is_refused_as_it_is_typed() {
        let mut edits = session();
        let one = row(&[Some("1"), Some("a"), None, Some("0x00ff")]);
        let nameless = row(&[None, Some("a"), None, None]);
        assert!(matches!(
            edits.set(0, &one, 2, text("x")),
            Err(Refused::ReadOnlyColumn { .. })
        ));
        assert_eq!(
            edits.set(0, &nameless, 1, text("x")),
            Err(Refused::KeyIsNull)
        );
        assert!(matches!(
            edits.set(0, &one, 3, text("plain")),
            Err(Refused::NotBytes { .. })
        ));
        edits.set(0, &one, 3, text("0x0102")).unwrap();
        let (request, _) = edits.request(None).unwrap();
        assert_eq!(
            request.changes,
            [RowChange::Update {
                key: vec![WriteValue::Text("1".into())],
                set: vec![(3, WriteValue::Bytes(vec![1, 2]))]
            }]
        );
        let empty = edits.insert_row();
        assert_eq!(
            edits.request(None).unwrap_err(),
            Refused::EmptyInsert { row: empty + 1 }
        );
    }

    #[test]
    fn a_column_whose_cells_were_mixed_cannot_be_edited() {
        let mut edits = session();
        edits.binary[1] = None;
        let one = row(&[Some("1"), Some("a"), None, None]);
        assert!(matches!(
            edits.set(0, &one, 1, text("x")),
            Err(Refused::MixedColumn { .. })
        ));
    }

    #[test]
    fn a_table_without_a_key_takes_one_named_before_editing() {
        let mut edits = session();
        edits.target.key.clear();
        edits.target.key_kind = KeyKind::Chosen;
        let one = row(&[Some("1"), Some("a"), None, None]);
        assert_eq!(edits.set(0, &one, 1, text("x")), Err(Refused::NeedsKey));
        assert!(matches!(
            edits.set_key(vec![2]),
            Err(Refused::NotAKeyColumn { .. })
        ));
        edits.set_key(vec![0, 1]).unwrap();
        edits.set(0, &one, 3, text("0x01")).unwrap();
        assert_eq!(edits.set_key(vec![0]), Err(Refused::KeyAfterEdits));
        let (request, _) = edits.request(None).unwrap();
        assert!(matches!(
            &request.changes[0],
            RowChange::Update { key, .. } if key.len() == 2
        ));
    }

    #[test]
    fn a_write_shows_what_it_wrote_and_a_rollback_marks_its_row() {
        let mut edits = session();
        let one = row(&[Some("1"), Some("a"), None, None]);
        edits.set(0, &one, 1, text("x")).unwrap();
        let index = edits.insert_row();
        edits.set_inserted(index, 1, text("new")).unwrap();
        let (_, origins) = edits.request(None).unwrap();
        edits.rolled_back(&origins, 0);
        assert_eq!(edits.row_mark(0), RowMark::Failed);
        assert_eq!(edits.pending(), 2, "edits stay pending");
        let version = edits.version();
        edits.written();
        assert!(edits.version() > version);
        assert_eq!(edits.pending(), 0);
        assert_eq!(edits.row_mark(0), RowMark::Written);
        assert_eq!(edits.overlay(0, 1), Some(Overlay::Written(&text("x"))));
        assert_eq!(edits.drawn_inserted(), 1);
        assert_eq!(
            edits.inserted_value(0, 1),
            Some(Overlay::Written(&text("new")))
        );
    }

    #[test]
    fn the_confirmation_lists_each_change_with_its_key_and_values() {
        let mut edits = session();
        let one = row(&[Some("1"), Some("NULL"), None, None]);
        edits.set(0, &one, 1, EditValue::Null).unwrap();
        let lines = edits.lines(10);
        assert_eq!(
            lines,
            [ChangeLine {
                kind: ChangeKind::Update,
                key: vec![("id".into(), Some("1".into()))],
                cells: vec![("note".into(), Some(Some("NULL".into())), EditValue::Null)],
            }]
        );
    }

    #[test]
    fn more_changes_than_one_write_holds_are_refused() {
        let mut edits = session();
        for index in 0..=MAX_CHANGES {
            let id = index.to_string();
            let read = row(&[Some(id.as_str()), Some("a"), None, None]);
            edits.set(index, &read, 1, text("b")).unwrap();
        }
        assert_eq!(
            edits.request(None).unwrap_err(),
            Refused::TooMany {
                changes: MAX_CHANGES + 1
            }
        );
    }
}
