//! Query results in memory: a compact, read-only table of display text.
//!
//! All cell text lives in one UTF-8 buffer; each cell is a range of it and
//! NULLs are bits in a bitmap, so a result costs a handful of allocations
//! instead of one per cell, and its size in memory is known exactly. Values
//! keep the database's text: exact numbers, temporal values and the text
//! `NULL` are never reinterpreted ([Results](../docs/reference/results.md)).
use crate::drivers::{Cell, ColumnInfo, ColumnOrigin, Row, RowSink, SinkControl};
use std::fmt::Write as _;

/// The largest row limit a connection may set.
pub const MAX_ROW_LIMIT: u32 = 100_000;

/// The row limit of a new connection.
pub const DEFAULT_ROW_LIMIT: u32 = 1_000;

/// How much memory one result may take before reading stops (256 MiB).
pub const DEFAULT_MEMORY_CEILING: usize = 256 * 1024 * 1024;

/// Name and database type of a result column, where its values come from,
/// and how they arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultColumn {
    pub name: String,
    pub data_type: String,
    /// The column's source, as the driver described it (ADR-0039).
    pub origin: ColumnOrigin,
    /// How this page's cells of the column arrived: `Some(true)` as bytes
    /// (shown as `0x…`), `Some(false)` as text or all NULL, `None` mixed
    /// (possible in SQLite). Set by the [`ResultBuilder`].
    pub binary: Option<bool>,
}

impl ResultColumn {
    pub fn new(name: impl Into<String>, data_type: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data_type: data_type.into(),
            origin: ColumnOrigin::Unknown,
            binary: Some(false),
        }
    }

    /// Whether `other` is the same column: its name and type, whatever its
    /// cells were on another page.
    pub fn same_as(&self, other: &Self) -> bool {
        self.name == other.name && self.data_type == other.data_type
    }

    /// The column as a driver describes it, for asking where it can be
    /// written back to.
    pub fn info(&self) -> ColumnInfo {
        let mut info = ColumnInfo::new(self.name.clone(), self.data_type.clone());
        info.origin = self.origin.clone();
        info
    }

    fn heap_bytes(&self) -> usize {
        let origin = match &self.origin {
            ColumnOrigin::Stored(source) => source.relation.len() + source.column.len(),
            _ => 0,
        };
        self.name.len() + self.data_type.len() + origin
    }
}

/// Which limit stopped reading a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultLimit {
    /// The row limit (the number of rows kept).
    Rows(u32),
    /// The memory ceiling, in bytes.
    Memory(usize),
}

/// A result as display text, stored compactly.
///
/// Cells are addressed by row and column; [`ResultData::value`] returns
/// `None` for NULL and for a position outside the result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResultData {
    columns: Vec<ResultColumn>,
    text: String,
    /// End offset in `text` of each cell, row by row. A cell starts where the
    /// previous one ends. `u32` halves the index; the memory ceiling keeps
    /// the text far below 4 GiB.
    ends: Vec<u32>,
    /// One bit per cell, set for NULL.
    nulls: Vec<u64>,
    rows: usize,
}

impl ResultData {
    pub fn new(columns: Vec<ResultColumn>) -> Self {
        Self {
            columns,
            ..Self::default()
        }
    }

    /// A result from text rows, where `None` is NULL. Short rows are padded
    /// with NULL and extra cells are ignored, so every row has one cell per
    /// column.
    pub fn from_rows<'a, R, C>(columns: Vec<ResultColumn>, rows: R) -> Self
    where
        R: IntoIterator<Item = C>,
        C: IntoIterator<Item = Option<&'a str>>,
    {
        let mut data = Self::new(columns);
        for row in rows {
            data.push_row(row);
        }
        data.shrink_to_fit();
        data
    }

    pub fn columns(&self) -> &[ResultColumn] {
        &self.columns
    }

    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    pub fn row_count(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The text of a cell; `None` for NULL or a position outside the result.
    pub fn value(&self, row: usize, column: usize) -> Option<&str> {
        let index = self.index(row, column)?;
        if self.is_null_at(index) {
            return None;
        }
        let start = match index.checked_sub(1) {
            Some(previous) => *self.ends.get(previous)? as usize,
            None => 0,
        };
        let end = *self.ends.get(index)? as usize;
        self.text.get(start..end)
    }

    /// Whether a cell is NULL. A position outside the result is not.
    pub fn is_null(&self, row: usize, column: usize) -> bool {
        self.index(row, column)
            .is_some_and(|index| self.is_null_at(index))
    }

    /// The cells of one row, in column order.
    pub fn row(&self, row: usize) -> impl Iterator<Item = Option<&str>> + '_ {
        let columns = if row < self.rows {
            self.columns.len()
        } else {
            0
        };
        (0..columns).map(move |column| self.value(row, column))
    }

    /// Appends a row of text cells (`None` is NULL), padded or cut to the
    /// number of columns.
    pub fn push_row<'a>(&mut self, cells: impl IntoIterator<Item = Option<&'a str>>) {
        let mut cells = cells.into_iter();
        for _ in 0..self.columns.len() {
            match cells.next().flatten() {
                Some(text) => {
                    self.text.push_str(text);
                    self.end_cell(false);
                }
                None => self.end_cell(true),
            }
        }
        self.rows += 1;
    }

    /// Appends a row of driver cells, converting each to its display text
    /// without an allocation per cell.
    pub fn push_cells(&mut self, cells: &[Cell]) {
        for column in 0..self.columns.len() {
            let null = match cells.get(column) {
                None | Some(Cell::Null) => true,
                Some(cell) => {
                    write_cell(&mut self.text, cell);
                    false
                }
            };
            self.end_cell(null);
        }
        self.rows += 1;
    }

    /// Bytes of cell text, NULLs counting none: what a copy of every cell
    /// holds before separators and quotes.
    pub fn text_bytes(&self) -> usize {
        self.text.len()
    }

    /// Bytes this result holds on the heap: cell text, offsets, the NULL
    /// bitmap and the column names and types.
    pub fn memory_bytes(&self) -> usize {
        self.text.capacity()
            + self.ends.capacity() * size_of::<u32>()
            + self.nulls.capacity() * size_of::<u64>()
            + self.columns.capacity() * size_of::<ResultColumn>()
            + self
                .columns
                .iter()
                .map(ResultColumn::heap_bytes)
                .sum::<usize>()
    }

    /// Bytes the result keeps once shrunk; what counts against the ceiling.
    fn kept_bytes(&self) -> usize {
        self.text.len()
            + self.ends.len() * size_of::<u32>()
            + self.nulls.len() * size_of::<u64>()
            + self.columns.len() * size_of::<ResultColumn>()
            + self
                .columns
                .iter()
                .map(ResultColumn::heap_bytes)
                .sum::<usize>()
    }

    /// Releases spare capacity, so [`ResultData::memory_bytes`] is what the
    /// data needs. Call it once the result is complete.
    pub fn shrink_to_fit(&mut self) {
        self.text.shrink_to_fit();
        self.ends.shrink_to_fit();
        self.nulls.shrink_to_fit();
    }

    fn index(&self, row: usize, column: usize) -> Option<usize> {
        (row < self.rows && column < self.columns.len()).then(|| row * self.columns.len() + column)
    }

    fn is_null_at(&self, index: usize) -> bool {
        self.nulls
            .get(index / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    fn end_cell(&mut self, null: bool) {
        let index = self.ends.len();
        if index / 64 >= self.nulls.len() {
            self.nulls.push(0);
        }
        if null && let Some(word) = self.nulls.get_mut(index / 64) {
            *word |= 1 << (index % 64);
        }
        // The memory ceiling, capped below 4 GiB, keeps this from saturating.
        self.ends
            .push(u32::try_from(self.text.len()).unwrap_or(u32::MAX));
    }
}

/// Writes a driver cell as display text. NULL writes nothing; callers mark
/// it in the bitmap.
fn write_cell(text: &mut String, cell: &Cell) {
    // Writing to a String cannot fail.
    let _ = match cell {
        Cell::Null => Ok(()),
        Cell::Boolean(value) => write!(text, "{value}"),
        Cell::Integer(value) => write!(text, "{value}"),
        Cell::UnsignedInteger(value) => write!(text, "{value}"),
        Cell::Float(value) => write!(text, "{value}"),
        Cell::Decimal(value) | Cell::Text(value) | Cell::Temporal(value) => {
            text.push_str(value);
            Ok(())
        }
        Cell::Binary(bytes) => {
            text.push_str("0x");
            bytes.iter().try_for_each(|byte| write!(text, "{byte:02x}"))
        }
    };
}

/// The display text of one driver cell; `NULL` for a NULL cell. Prefer
/// [`ResultData::push_cells`], which writes into the result's buffer.
pub fn cell_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".into(),
        cell => {
            let mut text = String::new();
            write_cell(&mut text, cell);
            text
        }
    }
}

/// A [`RowSink`] that builds a [`ResultData`] as rows arrive, so driver
/// cells are converted and dropped one row at a time on the runtime that
/// reads them.
///
/// Reading stops at the memory ceiling even below the row limit; the driver
/// itself stops at the row limit.
#[derive(Debug)]
pub struct ResultBuilder {
    data: ResultData,
    max_rows: u32,
    max_bytes: usize,
    limit: Option<ResultLimit>,
    /// Per column: whether a cell arrived as text, and as bytes.
    kinds: Vec<(bool, bool)>,
}

impl ResultBuilder {
    /// Keeps at most `max_rows` rows; a row beyond them marks the row limit as
    /// reached and stops reading. `max_bytes` is capped at 4 GiB minus one,
    /// the most one result can index.
    pub fn new(max_rows: u32, max_bytes: usize) -> Self {
        Self {
            data: ResultData::default(),
            max_rows,
            max_bytes: max_bytes.min(u32::MAX as usize),
            limit: None,
            kinds: Vec::new(),
        }
    }

    /// A builder for the next page of a result whose columns arrived with
    /// the first page.
    pub fn page(columns: Vec<ResultColumn>, max_rows: u32, max_bytes: usize) -> Self {
        let mut builder = Self::new(max_rows, max_bytes);
        builder.kinds = vec![(false, false); columns.len()];
        builder.data = ResultData::new(columns);
        builder
    }

    /// The result and the limit that stopped it, if any. A result with
    /// exactly `max_rows` rows did not hit the row limit; only a row beyond
    /// it does. Drivers stop at the limit themselves and say whether more
    /// rows exist, so this marks only rows the driver sent beyond it.
    pub fn finish(mut self) -> (ResultData, Option<ResultLimit>) {
        for (column, (text, bytes)) in self.data.columns.iter_mut().zip(&self.kinds) {
            column.binary = match (text, bytes) {
                (true, true) => None,
                (_, bytes) => Some(*bytes),
            };
        }
        self.data.shrink_to_fit();
        (self.data, self.limit)
    }
}

impl RowSink for ResultBuilder {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl {
        self.kinds = vec![(false, false); columns.len()];
        self.data = ResultData::new(
            columns
                .into_iter()
                .map(|column| {
                    let mut kept = ResultColumn::new(column.name, column.data_type);
                    kept.origin = column.origin;
                    kept
                })
                .collect(),
        );
        SinkControl::Continue
    }

    fn row(&mut self, row: Row) -> SinkControl {
        if self.limit.is_some() {
            return SinkControl::Stop;
        }
        if self.data.rows >= self.max_rows as usize {
            self.limit = Some(ResultLimit::Rows(self.max_rows));
            return SinkControl::Stop;
        }
        let checkpoint = (
            self.data.text.len(),
            self.data.ends.len(),
            self.data.nulls.len(),
        );
        self.data.push_cells(&row);
        for (kind, cell) in self.kinds.iter_mut().zip(&row) {
            match cell {
                Cell::Null => {}
                Cell::Binary(_) => kind.1 = true,
                _ => kind.0 = true,
            }
        }
        drop(row);
        // Capacity grows in steps; what counts against the ceiling is what
        // the result keeps once it is shrunk.
        if self.data.kept_bytes() > self.max_bytes {
            // The row that crossed the ceiling is not kept.
            self.data.text.truncate(checkpoint.0);
            self.data.ends.truncate(checkpoint.1);
            self.data.nulls.truncate(checkpoint.2);
            self.clear_bits_from(checkpoint.1);
            self.data.rows -= 1;
            self.limit = Some(ResultLimit::Memory(self.max_bytes));
            return SinkControl::Stop;
        }
        SinkControl::Continue
    }
}

impl ResultBuilder {
    /// Clears NULL bits at and after `cell`, left by a row that was removed.
    fn clear_bits_from(&mut self, cell: usize) {
        if let Some(word) = self.data.nulls.get_mut(cell / 64) {
            *word &= (1u64 << (cell % 64)) - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(names: &[&str]) -> Vec<ResultColumn> {
        names
            .iter()
            .map(|name| ResultColumn::new(*name, "text"))
            .collect()
    }

    fn info(name: &str, data_type: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: data_type.into(),
            nullable: Some(true),
            origin: Default::default(),
        }
    }

    #[test]
    fn values_nulls_empty_text_and_unicode_round_trip() {
        let data = ResultData::from_rows(
            columns(&["a", "b", "c"]),
            [
                vec![Some("1"), None, Some("")],
                vec![Some("NULL"), Some("ação ↵ 東京 🙂"), Some("  ")],
                vec![None, None, None],
            ],
        );
        assert_eq!(data.row_count(), 3);
        assert_eq!(data.column_count(), 3);
        assert_eq!(data.value(0, 0), Some("1"));
        assert_eq!(data.value(0, 1), None);
        assert!(data.is_null(0, 1));
        // Empty text and the text NULL stay values, distinct from NULL.
        assert_eq!(data.value(0, 2), Some(""));
        assert!(!data.is_null(0, 2));
        assert_eq!(data.value(1, 0), Some("NULL"));
        assert!(!data.is_null(1, 0));
        assert_eq!(data.value(1, 1), Some("ação ↵ 東京 🙂"));
        assert_eq!(data.value(1, 2), Some("  "));
        assert_eq!(data.row(2).collect::<Vec<_>>(), [None, None, None]);
        // Outside the result there is nothing, and it is not NULL.
        assert_eq!(data.value(3, 0), None);
        assert_eq!(data.value(0, 3), None);
        assert!(!data.is_null(3, 0));
        assert_eq!(data.row(3).count(), 0);
    }

    #[test]
    fn rows_are_padded_or_cut_to_the_columns() {
        let data = ResultData::from_rows(
            columns(&["a", "b"]),
            [vec![Some("x")], vec![Some("1"), Some("2"), Some("3")]],
        );
        assert_eq!(data.row(0).collect::<Vec<_>>(), [Some("x"), None]);
        assert_eq!(data.row(1).collect::<Vec<_>>(), [Some("1"), Some("2")]);
    }

    #[test]
    fn the_null_bitmap_spans_many_words() {
        let rows = (0..100).map(|row| {
            (0..3)
                .map(move |column| ((row + column) % 7 != 0).then_some("v"))
                .collect::<Vec<_>>()
        });
        let data = ResultData::from_rows(columns(&["a", "b", "c"]), rows);
        for row in 0..100 {
            for column in 0..3 {
                assert_eq!(data.is_null(row, column), (row + column) % 7 == 0);
            }
        }
    }

    #[test]
    fn driver_cells_become_their_exact_text() {
        let mut data = ResultData::new(columns(&["v"; 9]));
        data.push_cells(&[
            Cell::Null,
            Cell::Boolean(true),
            Cell::Integer(-9_223_372_036_854_775_808),
            Cell::UnsignedInteger(18_446_744_073_709_551_615),
            Cell::Float(1.5),
            Cell::Decimal("12345678901234567890.0123456789".into()),
            Cell::Text("NULL".into()),
            Cell::Binary(vec![0, 0xab, 0xff]),
            Cell::Temporal("2026-11-02 10:00:00+03".into()),
        ]);
        assert_eq!(
            data.row(0).collect::<Vec<_>>(),
            [
                None,
                Some("true"),
                Some("-9223372036854775808"),
                Some("18446744073709551615"),
                Some("1.5"),
                Some("12345678901234567890.0123456789"),
                Some("NULL"),
                Some("0x00abff"),
                Some("2026-11-02 10:00:00+03"),
            ]
        );
        assert_eq!(cell_text(&Cell::Null), "NULL");
        assert_eq!(cell_text(&Cell::Binary(vec![1, 2])), "0x0102");
    }

    #[test]
    fn memory_is_counted_exactly_once_shrunk() {
        let data = ResultData::from_rows(columns(&["a", "b"]), [vec![Some("abc"), None]]);
        let expected = 3
            + 2 * size_of::<u32>()
            + size_of::<u64>()
            + 2 * size_of::<ResultColumn>()
            + 2
            + 2 * "text".len();
        assert_eq!(data.memory_bytes(), expected);
    }

    #[test]
    fn the_builder_records_the_row_limit() {
        let mut builder = ResultBuilder::new(2, DEFAULT_MEMORY_CEILING);
        assert_eq!(
            builder.columns(vec![info("id", "int8")]),
            SinkControl::Continue
        );
        assert_eq!(builder.row(vec![Cell::Integer(1)]), SinkControl::Continue);
        assert_eq!(builder.row(vec![Cell::Integer(2)]), SinkControl::Continue);
        // A row beyond the limit is not kept and stops reading.
        assert_eq!(builder.row(vec![Cell::Integer(3)]), SinkControl::Stop);
        let (data, limit) = builder.finish();
        assert_eq!(data.row_count(), 2);
        assert_eq!(data.value(1, 0), Some("2"));
        assert_eq!(data.columns(), [ResultColumn::new("id", "int8")]);
        assert_eq!(limit, Some(ResultLimit::Rows(2)));

        // Exactly the limit, or fewer rows, did not hit it.
        for rows in [1, 2] {
            let mut builder = ResultBuilder::new(2, DEFAULT_MEMORY_CEILING);
            builder.columns(vec![info("id", "int8")]);
            for row in 0..rows {
                assert_eq!(builder.row(vec![Cell::Integer(row)]), SinkControl::Continue);
            }
            let (data, limit) = builder.finish();
            assert_eq!((data.row_count(), limit), (rows as usize, None));
        }
    }

    #[test]
    fn the_builder_keeps_each_columns_origin_and_how_its_cells_arrived() {
        use crate::drivers::SourceColumn;
        let mut builder = ResultBuilder::new(10, DEFAULT_MEMORY_CEILING);
        let mut stored = info("id", "int8");
        stored.origin = ColumnOrigin::Stored(SourceColumn {
            relation: "16384".into(),
            column: "1".into(),
        });
        builder.columns(vec![
            stored.clone(),
            info("raw", "blob"),
            info("any", ""),
            info("none", "text"),
        ]);
        builder.row(vec![
            Cell::Integer(1),
            Cell::Binary(vec![1]),
            Cell::Binary(vec![2]),
            Cell::Null,
        ]);
        builder.row(vec![
            Cell::Integer(2),
            Cell::Null,
            Cell::Text("x".into()),
            Cell::Null,
        ]);
        let (data, _) = builder.finish();
        let columns = data.columns();
        assert_eq!(columns[0].origin, stored.origin);
        assert_eq!(columns[0].info().origin, stored.origin);
        let binary: Vec<Option<bool>> = columns.iter().map(|column| column.binary).collect();
        assert_eq!(binary, [Some(false), Some(true), None, Some(false)]);
        // Another page of the same columns is the same column.
        let mut other = columns[1].clone();
        other.binary = Some(false);
        assert!(other.same_as(&columns[1]));
    }

    #[test]
    fn the_builder_stops_at_the_memory_ceiling_below_the_row_limit() {
        // Each row keeps 100 bytes of text, one offset and part of a bitmap word.
        let mut builder = ResultBuilder::new(MAX_ROW_LIMIT, 1_000);
        builder.columns(vec![info("note", "text")]);
        let mut sent = 0;
        while builder.row(vec![Cell::Text("x".repeat(100)), Cell::Null]) == SinkControl::Continue {
            sent += 1;
        }
        let (data, limit) = builder.finish();
        assert_eq!(limit, Some(ResultLimit::Memory(1_000)));
        assert_eq!(data.row_count(), sent);
        assert!(sent > 0 && sent < 10, "{sent}");
        // The rejected row left nothing behind.
        assert_eq!(data.value(sent, 0), None);
        assert_eq!(data.value(sent - 1, 0).map(str::len), Some(100));
        assert!(data.memory_bytes() <= 1_000, "{}", data.memory_bytes());
    }
}
