//! XLSX: one sheet of an `.xlsx` workbook through `calamine`'s cells reader
//! ([proposal](../../docs/plans/csv-and-xlsx-reading-dependencies.md#memory-and-streaming)).
//!
//! The cells reader streams the sheet, but opening the workbook reads its
//! shared strings table whole. So the table's size is checked in the zip
//! first, and a workbook whose table is over [`MAX_SHARED_STRINGS_BYTES`]
//! is refused before `calamine` sees it. `worksheet_range`, which collects
//! every cell, is never used.
//!
//! Cells arrive in file order, which is row-major in files written by Excel
//! and the usual libraries; a cell whose row goes backwards stops the read
//! at its address, never a silent reordering. Values follow ADR-0040,
//! section 3: a string is text, a number the shortest decimal that reads
//! back as the stored double, a boolean a boolean, a date or time its
//! calendar text without a time zone, an empty cell NULL, and an error cell
//! stops the read. A formula's cached value is read, never recalculated.
use super::{Flow, ImportValue, ReadEvent, ReadSummary, Record, check_cancel, file_error};
use crate::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode};
use crate::tr;
use calamine::{DataRef, ExcelDateTime, Reader as _, Xlsx, XlsxError};
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

/// The largest shared strings table an import opens, uncompressed: the
/// per-page memory ceiling of [Results](../../docs/reference/results.md#limits).
pub const MAX_SHARED_STRINGS_BYTES: u64 = 256 * 1024 * 1024;

/// How much of the shared strings table is read to find the count of
/// strings it declares.
const SHARED_STRINGS_HEAD: u64 = 64 * 1024;

/// Excel's last day, 9999-12-31, as a serial number plus one: dates at or
/// past it, or before the epoch, are not dates Excel holds.
const EXCEL_DATE_END: f64 = 2_958_466.0;

/// The rows and columns a sheet holds.
const EXCEL_ROWS: u32 = 1_048_576;
const EXCEL_COLUMNS: u32 = 16_384;

/// How a sheet is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XlsxOptions {
    pub sheet: String,
    /// The sheet's row that names the columns, one-based; rows above it are
    /// skipped. `None`: every row is data.
    pub header_row: Option<u32>,
    /// [`MAX_SHARED_STRINGS_BYTES`] unless a test says otherwise.
    pub max_shared_strings: u64,
}

impl XlsxOptions {
    pub fn new(sheet: impl Into<String>) -> Self {
        Self {
            sheet: sheet.into(),
            header_row: Some(1),
            max_shared_strings: MAX_SHARED_STRINGS_BYTES,
        }
    }
}

/// The names of the workbook's sheets, in its order, after the shared
/// strings check.
pub fn sheets(path: &Path) -> DriverResult<Vec<String>> {
    Ok(open(path, MAX_SHARED_STRINGS_BYTES)?.sheet_names())
}

fn open(path: &Path, limit: u64) -> DriverResult<Xlsx<BufReader<File>>> {
    let mut file = File::open(path).map_err(|error| file_error(&error))?;
    check_shared_strings(BufReader::new(&file), limit)?;
    file.rewind().map_err(|error| file_error(&error))?;
    Xlsx::new(BufReader::new(file)).map_err(|error| xlsx_error(&error))
}

/// Refuses a workbook whose shared strings table, which `calamine` reads
/// whole when it opens the workbook, is larger than `limit` uncompressed,
/// or declares more strings than its size can hold (`calamine` reserves
/// room for the declared count up front). The zip's declared size is a
/// bound: the zip reader fails an entry that inflates past it.
pub(super) fn check_shared_strings(reader: impl Read + Seek, limit: u64) -> DriverResult<()> {
    let mut archive = zip::ZipArchive::new(reader).map_err(|_| not_a_workbook())?;
    let tables: Vec<usize> = archive
        .file_names()
        .enumerate()
        .filter(|(_, name)| {
            name.as_ref()
                .is_ok_and(|name| name.to_ascii_lowercase().ends_with("sharedstrings.xml"))
        })
        .map(|(index, _)| index)
        .collect();
    for index in tables {
        let size = archive
            .by_index_raw(index)
            .map_err(|_| not_a_workbook())?
            .size();
        if size > limit {
            return Err(DriverError::validation(tr!(
                "The workbook's shared text takes {0} MiB, more than the {1} MiB an import reads. Save the sheet as CSV and import that file.",
                size.div_ceil(1024 * 1024),
                limit / (1024 * 1024)
            )));
        }
        // Deflate is compiled for `calamine`, which needs it too.
        let mut head = Vec::new();
        archive
            .by_index(index)
            .map_err(|_| not_a_workbook())?
            .take(SHARED_STRINGS_HEAD)
            .read_to_end(&mut head)
            .map_err(|_| not_a_workbook())?;
        // The shortest string entry, `<si/>`, takes five bytes. A table
        // whose count cannot be read here is refused too: `calamine` might
        // read one.
        if declared_count(&head).is_none_or(|count| count.is_some_and(|count| count > size / 5 + 1))
        {
            return Err(DriverError::validation(tr!(
                "The workbook declares more shared text than it holds; it may be damaged. Open it in Excel and save it again."
            )));
        }
    }
    Ok(())
}

/// The `uniqueCount` the `<sst>` element of a shared strings table
/// declares, read as an XML reader reads it: after an optional byte order
/// mark, processing instructions (the XML declaration), comments and white
/// space, the first element must be `sst` (prefixed or not), and its start
/// tag must end within `head`. `Some(None)` when it declares no count;
/// `None` when the table does not start so, or its count is not a plain
/// number, since `calamine` reserves room for the count it reads, and what
/// this cannot read it might.
fn declared_count(head: &[u8]) -> Option<Option<u64>> {
    let mut rest = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    loop {
        rest = rest.trim_ascii_start();
        if let Some(after) = rest.strip_prefix(b"<?") {
            rest = &after[find(after, b"?>")? + 2..];
        } else if let Some(after) = rest.strip_prefix(b"<!--") {
            rest = &after[find(after, b"-->")? + 3..];
        } else {
            break;
        }
    }
    let mut tag = rest.strip_prefix(b"<")?;
    let name_end = tag
        .iter()
        .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))?;
    let name = &tag[..name_end];
    if name != b"sst" && !name.ends_with(b":sst") {
        return None;
    }
    tag = &tag[name_end..];
    let mut count = None;
    loop {
        tag = tag.trim_ascii_start();
        if tag.starts_with(b">") || tag.starts_with(b"/>") {
            return Some(count);
        }
        let key_end = tag
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || *byte == b'=')?;
        let key = &tag[..key_end];
        tag = tag[key_end..]
            .trim_ascii_start()
            .strip_prefix(b"=")?
            .trim_ascii_start();
        let quote = *tag.first().filter(|byte| matches!(byte, b'"' | b'\''))?;
        tag = &tag[1..];
        let value_end = tag.iter().position(|byte| *byte == quote)?;
        let value = &tag[..value_end];
        tag = &tag[value_end + 1..];
        if key == b"uniqueCount" && count.is_none() {
            if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                return None;
            }
            count = Some(std::str::from_utf8(value).ok()?.parse().ok()?);
        }
    }
}

/// Where `needle` first starts in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn not_a_workbook() -> DriverError {
    DriverError::validation(tr!("The file is not an .xlsx workbook, or it is damaged."))
}

/// The error for a workbook `calamine` could not read, without the file's
/// contents.
fn xlsx_error(error: &XlsxError) -> DriverError {
    match error {
        XlsxError::Io(error) => file_error(error),
        XlsxError::Password => DriverError::validation(tr!(
            "The workbook is protected with a password. Save a copy without one and import that."
        )),
        XlsxError::Zip(_) => not_a_workbook(),
        XlsxError::WorksheetNotFound(name) => DriverError::new(
            ErrorCode::NotFound,
            tr!("The workbook has no sheet {0}.", name),
        ),
        XlsxError::NotAWorksheet(_) => DriverError::validation(tr!(
            "That sheet holds a chart, not cells. Choose another sheet."
        )),
        _ => DriverError::validation(tr!(
            "The workbook could not be read; it may be damaged. Open it in Excel and save it again."
        )),
    }
}

/// The A1 address of the cell at zero-based `row` and `column`.
pub fn cell_address(row: u32, column: u32) -> String {
    let mut letters = Vec::new();
    let mut rest = u64::from(column) + 1;
    while rest > 0 {
        let digit = (rest - 1) % 26;
        letters.push(char::from(b'A' + digit as u8));
        rest = (rest - 1) / 26;
    }
    letters.reverse();
    let mut address: String = letters.into_iter().collect();
    address.push_str(&(u64::from(row) + 1).to_string());
    address
}

/// The text a date, time or duration cell is bound as: `YYYY-MM-DD
/// HH:MM:SS[.mmm]`, or `HH:MM:SS[.mmm]` for a time without a date (a
/// serial below 1) and for a duration (whose hours may pass 24). Excel
/// keeps milliseconds at most, and no time zone. `None` for a date outside
/// what Excel holds, which `calamine` would clamp to its first or last day.
pub(super) fn datetime_text(value: &ExcelDateTime) -> Option<String> {
    let serial = value.as_f64();
    if !serial.is_finite() {
        return None;
    }
    if value.is_duration() {
        let millis = (serial * 86_400_000.0).round();
        if millis.abs() >= 1e15 {
            return None;
        }
        let millis = millis as i64;
        let sign = if millis < 0 { "-" } else { "" };
        let millis = millis.unsigned_abs();
        return Some(clock(
            sign,
            millis / 3_600_000,
            millis / 60_000 % 60,
            millis / 1000 % 60,
            millis % 1000,
        ));
    }
    if !(0.0..EXCEL_DATE_END).contains(&serial) {
        return None;
    }
    let (year, month, day, hour, minute, second, milli) = value.to_ymd_hms_milli();
    let time = clock(
        "",
        u64::from(hour),
        u64::from(minute),
        u64::from(second),
        u64::from(milli),
    );
    if serial < 1.0 {
        Some(time)
    } else {
        Some(format!("{year:04}-{month:02}-{day:02} {time}"))
    }
}

fn clock(sign: &str, hours: u64, minutes: u64, seconds: u64, millis: u64) -> String {
    if millis == 0 {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
    }
}

/// The value a cell is bound as; see the module's comment. `numbers` turns
/// on at a number.
fn cell_value(
    data: &DataRef<'_>,
    row: u32,
    column: u32,
    numbers: &mut bool,
) -> DriverResult<ImportValue> {
    Ok(match data {
        DataRef::Empty => ImportValue::Null,
        DataRef::String(text) | DataRef::DateTimeIso(text) | DataRef::DurationIso(text) => {
            ImportValue::Text(text.clone())
        }
        DataRef::SharedString(text) => ImportValue::Text((*text).to_owned()),
        DataRef::Float(number) => {
            // Excel holds no infinity or NaN: such a number (`1E999`) comes
            // from a damaged or hand-made file.
            if !number.is_finite() {
                return Err(DriverError::validation(tr!(
                    "Cell {0} holds a number Excel cannot hold. Fix it in the workbook, or leave its row out.",
                    cell_address(row, column)
                )));
            }
            *numbers = true;
            // Rust's `{}` writes the shortest decimal that reads back as the
            // same double, never in exponent form: 0.1 stays 0.1.
            ImportValue::Text(format!("{number}"))
        }
        DataRef::Int(number) => {
            *numbers = true;
            ImportValue::Text(number.to_string())
        }
        DataRef::Bool(value) => ImportValue::Boolean(*value),
        DataRef::DateTime(value) => ImportValue::Text(datetime_text(value).ok_or_else(|| {
            DriverError::validation(tr!(
                "Cell {0} holds a date outside the years Excel holds (1900 to 9999).",
                cell_address(row, column)
            ))
        })?),
        DataRef::Error(error) => {
            return Err(DriverError::validation(tr!(
                "Cell {0} holds the error {1}. Fix it in the workbook, or leave its row out.",
                cell_address(row, column),
                error
            )));
        }
    })
}

/// A header cell's text: the value's text, `TRUE` or `FALSE`, or empty.
fn header_name(value: ImportValue) -> String {
    match value {
        ImportValue::Null => String::new(),
        ImportValue::Text(text) => text,
        ImportValue::Boolean(true) => "TRUE".into(),
        ImportValue::Boolean(false) => "FALSE".into(),
    }
}

/// Puts a sheet's cells back into rows, one row at a time.
struct Rows {
    header_row: Option<u32>,
    /// The sheet's declared width, when it declares more than one cell.
    declared_width: Option<usize>,
    /// The sheet's declared last row, one-based, for progress.
    declared_rows: Option<u32>,
    /// The zero-based row being collected, and its last column.
    current: Option<(u32, u32)>,
    values: Vec<ImportValue>,
    /// The columns' count, once sent.
    width: Option<usize>,
    header_seen: bool,
    numbers: bool,
    rows: u64,
}

impl Rows {
    fn is_skipped(&self, row: u32) -> bool {
        self.header_row
            .is_some_and(|header| u64::from(row) + 1 < u64::from(header))
    }

    fn is_header(&self, row: u32) -> bool {
        self.header_row
            .is_some_and(|header| u64::from(row) + 1 == u64::from(header))
    }

    /// Takes the cell at `row` and `column`, sending the row before it once
    /// it is complete.
    fn cell(
        &mut self,
        row: u32,
        column: u32,
        data: &DataRef<'_>,
        visit: &mut dyn FnMut(ReadEvent) -> DriverResult<Flow>,
    ) -> DriverResult<Flow> {
        // A position past what a sheet holds comes from a damaged file (or
        // a reference `calamine` wrapped around); it is never read.
        if row >= EXCEL_ROWS || column >= EXCEL_COLUMNS {
            return Err(DriverError::validation(tr!(
                "Cell {0} lies outside what a sheet holds; the workbook may be damaged. Open it in Excel and save it again.",
                cell_address(row, column)
            )));
        }
        match self.current {
            Some((current, _)) if row < current => {
                return Err(DriverError::validation(tr!(
                    "Cell {0} comes after row {1} in the file: its rows are out of order. Open the workbook in Excel and save it again.",
                    cell_address(row, column),
                    current + 1
                )));
            }
            Some((current, last)) if row == current && column <= last => {
                return Err(DriverError::validation(tr!(
                    "Cell {0} comes after column {1} of its row in the file: its cells are out of order. Open the workbook in Excel and save it again.",
                    cell_address(row, column),
                    cell_address(row, last)
                )));
            }
            _ => {}
        }
        // A cell of a later row completes the row before it.
        if let Some((current, _)) = self.current
            && row > current
            && self.flush(current, visit)? == Flow::Stop
        {
            return Ok(Flow::Stop);
        }
        self.current = Some((row, column));
        if self.is_skipped(row) {
            return Ok(Flow::Continue);
        }
        let header = self.is_header(row);
        let mut numbers = false;
        let value = cell_value(data, row, column, &mut numbers)?;
        if !header {
            self.numbers |= numbers;
        }
        let index = column as usize;
        if !header
            && let Some(width) = self.width
            && index >= width
        {
            if value == ImportValue::Null {
                return Ok(Flow::Continue);
            }
            return Err(DriverError::validation(tr!(
                "Cell {0} holds a value right of the sheet's {1} columns. Give that column a header, or remove the value.",
                cell_address(row, column),
                width
            )));
        }
        if self.values.len() <= index {
            self.values.resize(index + 1, ImportValue::Null);
        }
        if let Some(slot) = self.values.get_mut(index) {
            *slot = value;
        }
        Ok(Flow::Continue)
    }

    /// Sends the zero-based `row`, now complete: the header's names, or a
    /// row of data. A row without any value is left out, as a CSV's blank
    /// line is.
    fn flush(
        &mut self,
        row: u32,
        visit: &mut dyn FnMut(ReadEvent) -> DriverResult<Flow>,
    ) -> DriverResult<Flow> {
        let mut values = std::mem::take(&mut self.values);
        if self.is_skipped(row) {
            return Ok(Flow::Continue);
        }
        if self.is_header(row) {
            let mut names: Vec<String> = values.into_iter().map(header_name).collect();
            while names.last().is_some_and(String::is_empty) {
                names.pop();
            }
            self.header_seen = true;
            return self.columns(names, visit);
        }
        if let Some(header) = self.header_row
            && !self.header_seen
        {
            return Err(DriverError::validation(tr!(
                "Row {0}, the header row, is empty. Choose the row that names the columns.",
                header
            )));
        }
        if values.iter().all(|value| *value == ImportValue::Null) {
            return Ok(Flow::Continue);
        }
        let width = match self.width {
            Some(width) => width,
            None => {
                let width = values.len().max(self.declared_width.unwrap_or(0));
                if self.columns(vec![String::new(); width], visit)? == Flow::Stop {
                    return Ok(Flow::Stop);
                }
                width
            }
        };
        values.resize(width, ImportValue::Null);
        let line = u64::from(row) + 1;
        let read = self
            .declared_rows
            .map(|rows| (line as f64 / f64::from(rows.max(1))).min(1.0));
        self.rows += 1;
        visit(ReadEvent::Row(Record { line, values, read }))
    }

    fn columns(
        &mut self,
        names: Vec<String>,
        visit: &mut dyn FnMut(ReadEvent) -> DriverResult<Flow>,
    ) -> DriverResult<Flow> {
        self.width = Some(names.len());
        visit(ReadEvent::Columns(names))
    }

    /// Sends the last row, and the columns if nothing did.
    fn finish(
        &mut self,
        visit: &mut dyn FnMut(ReadEvent) -> DriverResult<Flow>,
    ) -> DriverResult<Flow> {
        if let Some((row, _)) = self.current.take()
            && self.flush(row, visit)? == Flow::Stop
        {
            return Ok(Flow::Stop);
        }
        if self.width.is_none() {
            return self.columns(Vec::new(), visit);
        }
        Ok(Flow::Continue)
    }
}

/// Reads the sheet `options.sheet` of the workbook at `path`; see
/// [`super::read`].
pub(super) fn read(
    path: &Path,
    options: &XlsxOptions,
    cancel: &CancellationToken,
    visit: &mut dyn FnMut(ReadEvent) -> DriverResult<Flow>,
) -> DriverResult<ReadSummary> {
    let mut workbook = open(path, options.max_shared_strings)?;
    let mut cells = workbook
        .worksheet_cells_reader(&options.sheet)
        .map_err(|error| xlsx_error(&error))?;
    let dimensions = cells.dimensions();
    // Some writers declare `A1` whatever the sheet holds: one cell says
    // nothing.
    let declared = dimensions.end != dimensions.start;
    let mut rows = Rows {
        header_row: options.header_row,
        declared_width: declared.then_some(dimensions.end.1 as usize + 1),
        declared_rows: declared.then_some(dimensions.end.0.saturating_add(1)),
        current: None,
        values: Vec::new(),
        width: None,
        header_seen: false,
        numbers: false,
        rows: 0,
    };
    let mut summary = ReadSummary::default();
    let mut count: u64 = 0;
    let flow = loop {
        check_cancel(cancel, count)?;
        count += 1;
        let Some(cell) = cells.next_cell().map_err(|error| xlsx_error(&error))? else {
            break rows.finish(visit)?;
        };
        let (row, column) = cell.get_position();
        if rows.cell(row, column, cell.get_value(), visit)? == Flow::Stop {
            break Flow::Stop;
        }
    };
    summary.rows = rows.rows;
    summary.numbers = rows.numbers;
    summary.stopped = flow == Flow::Stop;
    Ok(summary)
}
