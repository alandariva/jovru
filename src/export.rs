//! Writing a result to a file the user chose ([Results](../docs/results.md#export)).
//!
//! Every format is written row by row, so an export never holds more than
//! the page it is writing. [`ExportFile`] writes into a temporary file next
//! to the destination and puts it in the destination's place only once every
//! row is written, flushed and synced: a failed or cancelled export leaves no
//! half file and never touches the file it would have replaced.
//!
//! Values are written as the database returned them, through the same rules
//! as **Copy as** ([`crate::copy_as`]) and the CSV copy
//! ([`crate::cell_format::csv_field`]): exact numbers are never parsed into
//! floats (XLSX, whose cells are floats, gets a number only when no digit can
//! change), NULL stays distinct from empty text wherever the format can tell
//! them apart, and nothing is changed to defend spreadsheets from formulas
//! ([Security](../docs/security.md#untrusted-content)).
//!
//! The functions of [`ExportFile`] block on disk I/O; callers run them on the
//! runtime's blocking pool ([`crate::backend::export`]).
use crate::cell_format::{ValueFamily, family};
use crate::copy_as::{json_keys, json_value, literal, markdown_alignment, markdown_cell};
use crate::drivers::{DriverError, DriverResult, ErrorCode, SqlDialect};
use crate::result::{ResultColumn, ResultData};
use crate::tr;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The file formats an export writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportFormat {
    /// Delimited text, as the CSV copy writes it by default.
    Csv,
    /// An array with one object per row, laid out as **Copy as JSON**.
    Json,
    /// One JSON object per line.
    JsonLines,
    /// One `INSERT` per row, as **Copy as INSERT** writes them.
    Insert,
    /// A Markdown table.
    Markdown,
    /// An Excel workbook with one sheet.
    Xlsx,
}

impl ExportFormat {
    pub const ALL: [Self; 6] = [
        Self::Csv,
        Self::Xlsx,
        Self::Json,
        Self::JsonLines,
        Self::Insert,
        Self::Markdown,
    ];

    /// The file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::JsonLines => "jsonl",
            Self::Insert => "sql",
            Self::Markdown => "md",
            Self::Xlsx => "xlsx",
        }
    }

    /// The name stored in the preferences.
    fn key(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::JsonLines => "jsonl",
            Self::Insert => "insert",
            Self::Markdown => "markdown",
            Self::Xlsx => "xlsx",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.key() == key)
    }

    /// Whether the format can leave out the column names.
    pub fn has_optional_header(self) -> bool {
        matches!(self, Self::Csv | Self::Xlsx)
    }
}

/// What separates the fields of a CSV export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Separator {
    /// `;`, which spreadsheets in languages with a decimal comma read.
    Semicolon,
    Comma,
    Tab,
}

impl Separator {
    pub const ALL: [Self; 3] = [Self::Semicolon, Self::Comma, Self::Tab];

    pub fn char(self) -> char {
        match self {
            Self::Semicolon => ';',
            Self::Comma => ',',
            Self::Tab => '\t',
        }
    }

    fn from_char(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|separator| text.chars().eq([separator.char()]))
    }
}

/// Which CSV values are quoted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Quoting {
    /// Every value, as the CSV copy does, so no spreadsheet reads a code as
    /// a number.
    All,
    /// Only values holding the separator, a quote or a line break (RFC
    /// 4180), and empty text, which would otherwise read as NULL.
    Needed,
}

/// How an export writes its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// CSV only.
    pub separator: Separator,
    /// CSV only.
    pub quoting: Quoting,
    /// CSV and XLSX: the column names first. The other formats always name
    /// the columns.
    pub header: bool,
    /// CSV only: a UTF-8 byte order mark first, which tells spreadsheets the
    /// encoding when the file is opened with a double click.
    pub bom: bool,
}

impl Default for ExportOptions {
    /// As the CSV copy writes: `;`, every value quoted, with headers.
    fn default() -> Self {
        Self {
            format: ExportFormat::Csv,
            separator: Separator::Semicolon,
            quoting: Quoting::All,
            header: true,
            bom: false,
        }
    }
}

/// How `INSERT`s name the table and the columns, quoted by the tab's
/// connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqlTarget {
    pub dialect: Option<SqlDialect>,
    /// The table, already quoted; a placeholder when the result is not a
    /// table's.
    pub table: String,
    /// The column names, quoted when the dialect needs it, in result order.
    pub columns: Vec<String>,
}

/// The rows that fit in an Excel sheet, the header included.
pub const XLSX_MAX_ROWS: u64 = 1_048_576;
/// The columns that fit in an Excel sheet.
pub const XLSX_MAX_COLUMNS: usize = 16_384;
/// Excel keeps 15 significant digits of a number: a value with more is
/// written as text, so no digit changes.
const XLSX_DIGITS: usize = 15;

/// Writes the text formats one row at a time.
#[derive(Debug)]
pub struct Encoder {
    kind: Kind,
    names: Vec<String>,
    families: Vec<ValueFamily>,
    rows: u64,
}

#[derive(Debug)]
enum Kind {
    Csv {
        separator: char,
        quoting: Quoting,
        header: bool,
    },
    Json {
        keys: Vec<String>,
        lines: bool,
    },
    Insert {
        /// `INSERT INTO t (a, b) VALUES (`
        prefix: String,
        dialect: Option<SqlDialect>,
    },
    Markdown,
}

impl Encoder {
    /// An encoder for `columns`; `None` for XLSX, which is not text.
    pub fn new(options: &ExportOptions, columns: &[ResultColumn], sql: &SqlTarget) -> Option<Self> {
        let names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
        let kind = match options.format {
            ExportFormat::Csv => Kind::Csv {
                separator: options.separator.char(),
                quoting: options.quoting,
                header: options.header,
            },
            ExportFormat::Json | ExportFormat::JsonLines => Kind::Json {
                keys: json_keys(names.iter().map(String::as_str)),
                lines: options.format == ExportFormat::JsonLines,
            },
            ExportFormat::Insert => {
                // A column the caller did not quote is quoted as it was named.
                let quoted: Vec<&str> = names
                    .iter()
                    .enumerate()
                    .map(|(index, name)| sql.columns.get(index).unwrap_or(name).as_str())
                    .collect();
                Kind::Insert {
                    prefix: format!("INSERT INTO {} ({}) VALUES (", sql.table, quoted.join(", ")),
                    dialect: sql.dialect,
                }
            }
            ExportFormat::Markdown => Kind::Markdown,
            ExportFormat::Xlsx => return None,
        };
        Some(Self {
            kind,
            names,
            families: columns
                .iter()
                .map(|column| family(&column.data_type))
                .collect(),
            rows: 0,
        })
    }

    /// What comes before the rows: the CSV header, the opening bracket, the
    /// Markdown header.
    pub fn begin(&self, out: &mut String) {
        match &self.kind {
            Kind::Csv {
                separator,
                quoting,
                header,
            } => {
                if *header {
                    for (index, name) in self.names.iter().enumerate() {
                        if index > 0 {
                            out.push(*separator);
                        }
                        push_csv_field(out, Some(name), *separator, *quoting);
                    }
                    out.push('\n');
                }
            }
            Kind::Json { lines: false, .. } => out.push('['),
            Kind::Json { lines: true, .. } | Kind::Insert { .. } => {}
            Kind::Markdown => {
                out.push('|');
                for name in &self.names {
                    out.push(' ');
                    out.push_str(&markdown_cell(name));
                    out.push_str(" |");
                }
                out.push_str("\n|");
                for family in &self.families {
                    out.push(' ');
                    out.push_str(markdown_alignment(*family));
                    out.push_str(" |");
                }
                out.push('\n');
            }
        }
    }

    /// One row; `None` is NULL. Missing cells are NULL and extra cells are
    /// ignored, as in [`ResultData`].
    pub fn row<'a>(&mut self, cells: impl IntoIterator<Item = Option<&'a str>>, out: &mut String) {
        let mut cells = cells.into_iter();
        let mut next = || cells.next().flatten();
        let count = self.names.len();
        match &self.kind {
            Kind::Csv {
                separator, quoting, ..
            } => {
                for index in 0..count {
                    if index > 0 {
                        out.push(*separator);
                    }
                    push_csv_field(out, next(), *separator, *quoting);
                }
                out.push('\n');
            }
            Kind::Json { keys, lines } => {
                if !lines {
                    out.push_str(if self.rows == 0 { "\n  " } else { ",\n  " });
                }
                out.push('{');
                for (index, (key, family)) in keys.iter().zip(&self.families).enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(key);
                    out.push_str(": ");
                    out.push_str(&json_value(next(), *family));
                }
                out.push('}');
                if *lines {
                    out.push('\n');
                }
            }
            Kind::Insert { prefix, dialect } => {
                out.push_str(prefix);
                for (index, family) in self.families.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&literal(next(), *family, *dialect));
                }
                out.push_str(");\n");
            }
            Kind::Markdown => {
                out.push('|');
                for _ in 0..count {
                    out.push(' ');
                    match next() {
                        Some(value) => out.push_str(&markdown_cell(value)),
                        None => out.push_str("NULL"),
                    }
                    out.push_str(" |");
                }
                out.push('\n');
            }
        }
        self.rows += 1;
    }

    /// Every row of `data`.
    pub fn rows(&mut self, data: &ResultData, out: &mut String) {
        for row in 0..data.row_count() {
            self.row(data.row(row), out);
        }
    }

    /// What comes after the rows: the closing bracket of a JSON array.
    pub fn end(&self, out: &mut String) {
        if let Kind::Json { lines: false, .. } = self.kind {
            out.push_str(if self.rows == 0 { "]\n" } else { "\n]\n" });
        }
    }
}

/// One CSV field. NULL is an empty field; a quote inside a value is
/// doubled.
fn push_csv_field(out: &mut String, value: Option<&str>, separator: char, quoting: Quoting) {
    let Some(value) = value else {
        return;
    };
    let quote =
        quoting == Quoting::All || value.is_empty() || value.contains([separator, '"', '\n', '\r']);
    if !quote {
        out.push_str(value);
        return;
    }
    out.push('"');
    let mut rest = value;
    while let Some(at) = rest.find('"') {
        out.push_str(&rest[..=at]);
        out.push('"');
        rest = &rest[at + 1..];
    }
    out.push_str(rest);
    out.push('"');
}

/// The number Excel would hold for `text`, when it keeps every digit:
/// a plain decimal (optionally with an exponent) of at most 15 significant
/// digits whose value is finite. Anything else is written as text.
fn xlsx_number(text: &str) -> Option<f64> {
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if (integer.is_empty() && fraction.is_empty())
        || !digits(integer)
        || !digits(fraction)
        || exponent.is_some_and(|exponent| {
            let exponent = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
            exponent.is_empty() || !digits(exponent)
        })
    {
        return None;
    }
    let significant = format!("{integer}{fraction}");
    let significant = significant.trim_start_matches('0').trim_end_matches('0');
    if significant.len() > XLSX_DIGITS {
        return None;
    }
    text.parse::<f64>().ok().filter(|number| number.is_finite())
}

/// The booleans a boolean column may hold as text.
fn boolean(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "t" | "true" | "1" => Some(true),
        "f" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// A sheet written in Excel's constant memory mode: each row goes to a
/// temporary file as soon as the next one starts.
struct Sheet {
    workbook: rust_xlsxwriter::Workbook,
    families: Vec<ValueFamily>,
    /// The next row of the sheet, the header included.
    next_row: u64,
}

impl std::fmt::Debug for Sheet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sheet")
            .field("next_row", &self.next_row)
            .finish_non_exhaustive()
    }
}

impl Sheet {
    fn new(dir: &Path, columns: &[ResultColumn], header: bool) -> DriverResult<Self> {
        if columns.len() > XLSX_MAX_COLUMNS {
            return Err(DriverError::validation(tr!(
                "An Excel sheet holds at most 16,384 columns. Export as CSV or JSON instead."
            )));
        }
        let mut workbook = rust_xlsxwriter::Workbook::new();
        // Its temporary files go where the export goes, which the user chose
        // and has room for; they are removed when the workbook is dropped.
        workbook.set_tempdir(dir).map_err(xlsx_error)?;
        let bold = rust_xlsxwriter::Format::new().set_bold();
        let sheet = workbook
            .add_worksheet_with_constant_memory()
            .map_err(xlsx_error)?;
        sheet.set_name("Result").map_err(xlsx_error)?;
        let mut next_row = 0;
        if header {
            for (index, column) in columns.iter().enumerate() {
                let index = u16::try_from(index).map_err(|_| too_many_columns())?;
                sheet
                    .write_string_with_format(0, index, column.name.as_str(), &bold)
                    .map_err(xlsx_error)?;
            }
            sheet.set_freeze_panes(1, 0).map_err(xlsx_error)?;
            next_row = 1;
        }
        Ok(Self {
            workbook,
            families: columns
                .iter()
                .map(|column| family(&column.data_type))
                .collect(),
            next_row,
        })
    }

    fn rows(&mut self, data: &ResultData) -> DriverResult<()> {
        if self.next_row + data.row_count() as u64 > XLSX_MAX_ROWS {
            return Err(DriverError::validation(tr!(
                "An Excel sheet holds at most 1,048,576 rows. Export as CSV or JSON instead."
            )));
        }
        let sheet = self.workbook.worksheet_from_index(0).map_err(xlsx_error)?;
        for row in 0..data.row_count() {
            // Checked above against Excel's row count, far below u32::MAX.
            let at = u32::try_from(self.next_row).map_err(|_| too_many_rows())?;
            for (column, value) in data.row(row).enumerate() {
                let Some(value) = value else {
                    // NULL leaves the cell empty.
                    continue;
                };
                let index = u16::try_from(column).map_err(|_| too_many_columns())?;
                let family = self
                    .families
                    .get(column)
                    .copied()
                    .unwrap_or(ValueFamily::Text);
                let written = match family {
                    ValueFamily::Number => match xlsx_number(value.trim()) {
                        Some(number) => sheet.write_number(at, index, number),
                        None => sheet.write_string(at, index, value),
                    },
                    ValueFamily::Boolean => match boolean(value) {
                        Some(flag) => sheet.write_boolean(at, index, flag),
                        None => sheet.write_string(at, index, value),
                    },
                    // Dates and times stay the database's text, never
                    // converted between time zones.
                    _ => sheet.write_string(at, index, value),
                };
                written.map_err(xlsx_error)?;
            }
            self.next_row += 1;
        }
        Ok(())
    }

    fn save(mut self, path: &Path) -> DriverResult<()> {
        self.workbook.save(path).map_err(xlsx_error)
    }
}

fn too_many_rows() -> DriverError {
    DriverError::validation(tr!(
        "An Excel sheet holds at most 1,048,576 rows. Export as CSV or JSON instead."
    ))
}

fn too_many_columns() -> DriverError {
    DriverError::validation(tr!(
        "An Excel sheet holds at most 16,384 columns. Export as CSV or JSON instead."
    ))
}

fn xlsx_error(error: rust_xlsxwriter::XlsxError) -> DriverError {
    use rust_xlsxwriter::XlsxError as X;
    match error {
        X::MaxStringLengthExceeded => DriverError::validation(tr!(
            "A value has more than 32,767 characters, more than an Excel cell holds. Export as CSV or JSON instead."
        )),
        X::RowColumnLimitError => too_many_rows(),
        X::TempFileError(_, error) | X::IoError(error) => file_error(&error),
        _ => DriverError::new(ErrorCode::Internal, tr!("Could not write the Excel file.")),
    }
}

/// A disk error as the user can act on it. The message never holds the
/// path, which may name the user's folders.
pub fn file_error(error: &std::io::Error) -> DriverError {
    use std::io::ErrorKind as K;
    match error.kind() {
        K::NotFound => DriverError::new(
            ErrorCode::NotFound,
            tr!("The folder of the file no longer exists. Choose another."),
        ),
        K::PermissionDenied => DriverError::new(
            ErrorCode::PermissionDenied,
            tr!(
                "Could not write there: no permission, or the file is open in another program. Close it or choose another file."
            ),
        ),
        K::StorageFull | K::QuotaExceeded => DriverError::new(
            ErrorCode::Internal,
            tr!("The disk is full. Free some space or choose another place."),
        ),
        K::ReadOnlyFilesystem => DriverError::new(
            ErrorCode::PermissionDenied,
            tr!("That place is read-only. Choose another."),
        ),
        _ => DriverError::new(ErrorCode::Internal, tr!("Could not write the file.")),
    }
}

/// What a finished export wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub rows: u64,
    /// The size of the file.
    pub bytes: u64,
}

enum Output {
    Text {
        file: BufWriter<File>,
        encoder: Encoder,
        buffer: String,
    },
    Xlsx(Box<Sheet>),
}

/// An export being written: a temporary file in the destination's folder
/// that becomes the destination in [`ExportFile::finish`]. Dropped before
/// that, it removes the temporary file.
pub struct ExportFile {
    target: PathBuf,
    temp: PathBuf,
    /// `None` once finished, or while dropping.
    output: Option<Output>,
    rows: u64,
    bytes: u64,
}

impl std::fmt::Debug for ExportFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportFile")
            .field("rows", &self.rows)
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

/// Text is handed to the file in pieces of about this size.
const FLUSH_BYTES: usize = 1 << 20;

impl ExportFile {
    /// Creates the temporary file next to `target` and writes what comes
    /// before the rows. Fails before anything is read when the folder is
    /// missing or cannot be written.
    pub fn create(
        target: &Path,
        options: &ExportOptions,
        columns: &[ResultColumn],
        sql: &SqlTarget,
    ) -> DriverResult<Self> {
        let name = target
            .file_name()
            .ok_or_else(|| DriverError::validation(tr!("Choose a file to export to.")))?;
        let dir = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let temp = dir.join(temp_name(name));
        // `create_new` never reuses a file that happens to have the name.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| file_error(&error))?;
        let mut export = Self {
            target: target.to_path_buf(),
            temp,
            output: None,
            rows: 0,
            bytes: 0,
        };
        let output = match Encoder::new(options, columns, sql) {
            Some(encoder) => {
                let mut buffer = String::new();
                if options.bom && options.format == ExportFormat::Csv {
                    buffer.push('\u{feff}');
                }
                encoder.begin(&mut buffer);
                let mut file = BufWriter::with_capacity(FLUSH_BYTES, file);
                export.bytes += write_out(&mut file, &mut buffer)?;
                Output::Text {
                    file,
                    encoder,
                    buffer,
                }
            }
            None => {
                // The workbook writes the file itself when saved; the empty
                // one holds the name until then.
                drop(file);
                Output::Xlsx(Box::new(Sheet::new(&dir, columns, options.header)?))
            }
        };
        export.output = Some(output);
        Ok(export)
    }

    /// Writes every row of `data`, whose columns are the export's.
    pub fn write(&mut self, data: &ResultData) -> DriverResult<()> {
        match self.output.as_mut() {
            Some(Output::Text {
                file,
                encoder,
                buffer,
            }) => {
                for row in 0..data.row_count() {
                    encoder.row(data.row(row), buffer);
                    if buffer.len() >= FLUSH_BYTES {
                        self.bytes += write_out(file, buffer)?;
                    }
                }
                self.bytes += write_out(file, buffer)?;
            }
            Some(Output::Xlsx(sheet)) => sheet.rows(data)?,
            None => return Err(finished()),
        }
        self.rows += data.row_count() as u64;
        Ok(())
    }

    /// Rows written so far.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Bytes written so far; an Excel file's size is known when it is saved.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Writes what comes after the rows, flushes and syncs the file and puts
    /// it in the destination's place, replacing the file there.
    pub fn finish(mut self) -> DriverResult<Written> {
        match self.output.take() {
            Some(Output::Text {
                mut file,
                encoder,
                mut buffer,
            }) => {
                encoder.end(&mut buffer);
                let result = file
                    .write_all(buffer.as_bytes())
                    .and_then(|()| file.flush())
                    .and_then(|()| file.get_ref().sync_all());
                self.bytes += buffer.len() as u64;
                drop(file);
                result.map_err(|error| file_error(&error))?;
            }
            Some(Output::Xlsx(sheet)) => {
                sheet.save(&self.temp)?;
                // Windows flushes only a file opened for writing; this one
                // is not truncated.
                OpenOptions::new()
                    .write(true)
                    .open(&self.temp)
                    .and_then(|file| file.sync_all())
                    .map_err(|error| file_error(&error))?;
                self.bytes = std::fs::metadata(&self.temp)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
            }
            None => return Err(finished()),
        }
        std::fs::rename(&self.temp, &self.target).map_err(|error| file_error(&error))?;
        // Nothing is left to remove.
        self.temp = PathBuf::new();
        Ok(Written {
            rows: self.rows,
            bytes: self.bytes,
        })
    }
}

/// Hands the buffered text to the file and empties the buffer; returns the
/// bytes handed over.
fn write_out(file: &mut BufWriter<File>, buffer: &mut String) -> DriverResult<u64> {
    if buffer.is_empty() {
        return Ok(0);
    }
    file.write_all(buffer.as_bytes())
        .map_err(|error| file_error(&error))?;
    let bytes = buffer.len() as u64;
    buffer.clear();
    Ok(bytes)
}

impl Drop for ExportFile {
    fn drop(&mut self) {
        // The file is closed first: an open file cannot be removed on
        // Windows.
        drop(self.output.take());
        if !self.temp.as_os_str().is_empty() {
            // Best effort: a temporary file left behind holds no complete
            // export and has a name no export reuses.
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

fn finished() -> DriverError {
    DriverError::new(ErrorCode::Internal, tr!("The export already ended."))
}

/// A name no other export, here or in another window, uses at the same
/// time.
fn temp_name(name: &std::ffi::OsStr) -> std::ffi::OsString {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut temp = name.to_os_string();
    temp.push(format!(
        ".{}-{}.partial",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    temp
}

/// A file name for an export of `stem` (a table or a tab title) in
/// `format`. Names from the database are never read as paths: characters
/// that a file name cannot hold become `_`, and names Windows reserves get a
/// `_` in front ([Security](../docs/security.md#untrusted-content)).
pub fn suggested_file_name(stem: &str, format: ExportFormat) -> String {
    let cleaned: String = stem
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '_'
            } else {
                ch
            }
        })
        .take(100)
        .collect();
    // Windows drops trailing dots and spaces; leading dots hide the file.
    let cleaned = cleaned
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .to_owned();
    let mut name = if cleaned.is_empty() {
        "result".to_owned()
    } else {
        cleaned
    };
    let base = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((base.starts_with("COM") || base.starts_with("LPT"))
            && base.len() == 4
            && base.as_bytes().get(3).is_some_and(u8::is_ascii_digit));
    if reserved {
        name.insert(0, '_');
    }
    format!("{name}.{}", format.extension())
}

/// Where the save dialog opens before any export chose a folder: the user's
/// Downloads, Documents or home folder.
pub fn default_directory() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::document_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// What the export bar remembers between exports: the options and the folder
/// of the last file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportPreferences {
    pub options: ExportOptions,
    pub directory: Option<PathBuf>,
}

/// The setting the preferences are kept under in `config.db`.
pub const PREFERENCES_KEY: &str = "export.preferences";

impl ExportPreferences {
    /// Reads what [`ExportPreferences::to_value`] wrote; anything missing or
    /// unknown takes its default, so older or newer settings keep loading.
    pub fn from_value(value: Option<&serde_json::Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let defaults = ExportOptions::default();
        let text = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
        let flag = |key: &str, default: bool| {
            value
                .get(key)
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(default)
        };
        Self {
            options: ExportOptions {
                format: text("format")
                    .and_then(ExportFormat::from_key)
                    .unwrap_or(defaults.format),
                separator: text("separator")
                    .and_then(Separator::from_char)
                    .unwrap_or(defaults.separator),
                quoting: match text("quoting") {
                    Some("needed") => Quoting::Needed,
                    Some("all") => Quoting::All,
                    _ => defaults.quoting,
                },
                header: flag("header", defaults.header),
                bom: flag("bom", defaults.bom),
            },
            directory: text("directory")
                .filter(|directory| !directory.is_empty())
                .map(PathBuf::from),
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "format": self.options.format.key(),
            "separator": self.options.separator.char().to_string(),
            "quoting": match self.options.quoting {
                Quoting::All => "all",
                Quoting::Needed => "needed",
            },
            "header": self.options.header,
            "bom": self.options.bom,
            "directory": self.directory.as_ref().map(|directory| directory.to_string_lossy()),
        })
    }
}

/// The writes of the export preferences: the last one asked for wins, even
/// from another tab.
pub static PREFERENCE_WRITES: crate::connection_store::LatestWrite =
    crate::connection_store::LatestWrite::new();

/// Reads the export preferences kept in the data folder `dir`.
pub async fn load_preferences(dir: &Path) -> DriverResult<ExportPreferences> {
    let repository = jykre_storage::ConfigRepository::open(dir).await?;
    let value = repository.setting(PREFERENCES_KEY).await;
    repository.close().await;
    Ok(ExportPreferences::from_value(value?.as_ref()))
}

/// Saves the export preferences for the write `ticket` of
/// [`PREFERENCE_WRITES`]; preferences superseded by newer ones are not
/// written.
pub async fn save_preferences(
    dir: &Path,
    preferences: ExportPreferences,
    ticket: u64,
) -> DriverResult<()> {
    PREFERENCE_WRITES
        .run(ticket, async {
            let repository = jykre_storage::ConfigRepository::open(dir).await?;
            let result = repository
                .set_setting(PREFERENCES_KEY, &preferences.to_value())
                .await;
            repository.close().await;
            result
        })
        .await
        .unwrap_or(Ok(()))
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
