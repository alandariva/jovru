//! Reading a CSV or XLSX file for an import into a table
//! ([ADR-0040](../../docs/adr/0040-importing-files-into-a-table.md)).
//!
//! This module knows no window and no session. It turns a file into rows of
//! [`ImportValue`]s, one at a time, with bounded memory:
//!
//! - [`delimited`] reads CSV through a decoder of the chosen encoding that
//!   stops at undecodable bytes instead of replacing them;
//! - [`workbook`] reads one sheet of an `.xlsx` through `calamine`'s cells
//!   reader, after checking the size of its shared strings table;
//! - [`mapping`] decides which file column fills which table column;
//! - [`draft`] writes the `CREATE TABLE` that **New table from this file…**
//!   opens in a query tab;
//! - [`pipeline`] cuts the mapped rows into batches, joins the reader to the
//!   writer through a bounded channel and runs an import all or nothing
//!   against a [`pipeline::BatchWriter`], the seam ADR-0039's write path
//!   implements.
//!
//! Every function here blocks on file I/O or CPU work; callers run them on
//! the runtime's blocking pool. Values read from a file are never logged.
use crate::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode};
use crate::tr;
use std::path::Path;

pub mod delimited;
pub mod draft;
pub mod mapping;
pub mod pipeline;
pub mod workbook;

#[cfg(test)]
mod tests;

pub use delimited::{CsvOptions, Delimiter};
pub use encoding_rs::Encoding;
pub use workbook::XlsxOptions;

/// A value read from the file, as it will be bound: text exactly as the file
/// holds it (the database converts it to the column's type), NULL, or an
/// XLSX boolean.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportValue {
    Null,
    Text(String),
    Boolean(bool),
}

impl ImportValue {
    /// The bytes the value holds, for the batch size: its text, one byte for
    /// a boolean, none for NULL.
    pub fn size(&self) -> usize {
        match self {
            Self::Null => 0,
            Self::Text(text) => text.len(),
            Self::Boolean(_) => 1,
        }
    }
}

/// What an empty CSV field means, chosen per import (decision I-4). An
/// XLSX's empty cell is always NULL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum EmptyFields {
    /// NULL, as `jykre::export` writes it.
    #[default]
    Null,
    /// Empty text.
    EmptyText,
}

/// The kinds of file an import reads (decision I-5: `.xlsx` only among
/// workbooks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileKind {
    Csv,
    Xlsx,
}

/// What kind of file `path` is, by its extension. `.xlsx` is a workbook;
/// the other spreadsheet formats are refused with a reason; anything else
/// is read as delimited text.
pub fn file_kind(path: &Path) -> DriverResult<FileKind> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "xlsx" => Ok(FileKind::Xlsx),
        "xls" | "xlsb" | "xlsm" | "ods" => Err(DriverError::validation(tr!(
            "Only .xlsx workbooks can be imported. Save the sheet as .xlsx or CSV and choose that file."
        ))),
        _ => Ok(FileKind::Csv),
    }
}

/// What the dialog learns from a file as soon as it is chosen.
#[derive(Clone, Debug, PartialEq)]
pub struct FileInfo {
    pub kind: FileKind,
    /// A workbook's sheets, in its order.
    pub sheets: Vec<String>,
    /// The delimiter a CSV's first line suggests.
    pub delimiter: Delimiter,
    /// The encoding a CSV's byte-order mark names, which overrides the one
    /// chosen.
    pub bom: Option<&'static Encoding>,
}

/// Bytes of a CSV read to suggest its delimiter.
const SAMPLE_BYTES: u64 = 64 * 1024;

/// What `path` is: its kind, a workbook's sheets (after the shared strings
/// check), or a CSV's byte-order mark and the delimiter its first line
/// suggests. The sample is decoded leniently, for that guess only; the
/// import itself never replaces a byte.
pub fn inspect(path: &Path) -> DriverResult<FileInfo> {
    let kind = file_kind(path)?;
    if kind == FileKind::Xlsx {
        return Ok(FileInfo {
            kind,
            sheets: workbook::sheets(path)?,
            delimiter: Delimiter::default(),
            bom: None,
        });
    }
    use std::io::Read as _;
    let mut head = Vec::new();
    std::fs::File::open(path)
        .map_err(|error| file_error(&error))?
        .take(SAMPLE_BYTES)
        .read_to_end(&mut head)
        .map_err(|error| file_error(&error))?;
    let bom = Encoding::for_bom(&head);
    let (encoding, skip) = bom.unwrap_or((encoding_rs::UTF_8, 0));
    let (sample, _) = encoding.decode_without_bom_handling(head.get(skip..).unwrap_or_default());
    Ok(FileInfo {
        kind,
        sheets: Vec::new(),
        delimiter: Delimiter::guess(&sample),
        bom: bom.map(|(encoding, _)| encoding),
    })
}

/// Runs `work`, which blocks on the file, on the runtime's blocking pool:
/// how the dialog reads files off the UI thread.
pub async fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> DriverResult<T> + Send + 'static,
) -> DriverResult<T> {
    crate::backend::run(async move {
        tokio::task::spawn_blocking(work).await.map_err(|_| {
            DriverError::new(
                ErrorCode::Internal,
                tr!("Reading the file stopped unexpectedly."),
            )
        })?
    })
    .await
}

/// How to read the chosen file.
#[derive(Clone, Debug, PartialEq)]
pub enum ReadOptions {
    Csv(CsvOptions),
    Xlsx(XlsxOptions),
}

impl ReadOptions {
    /// The header row, one-based; `None` when the file has none.
    pub fn header_row(&self) -> Option<u32> {
        match self {
            Self::Csv(options) => options.header_row,
            Self::Xlsx(options) => options.header_row,
        }
    }
}

/// One row of data from the file.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// Where the row starts: the CSV line, or the sheet's row number.
    pub line: u64,
    /// One value per file column, padded with NULL to the columns' count.
    pub values: Vec<ImportValue>,
    /// The share of the file read once this row was, from 0 to 1, when it
    /// can be told: the bytes of a CSV, the rows of a sheet that declares
    /// its dimension.
    pub read: Option<f64>,
}

/// What a reader hands its visitor, in order: the columns once, then the
/// rows.
#[derive(Clone, Debug, PartialEq)]
pub enum ReadEvent {
    /// The file's columns: the header's texts, or empty names when the file
    /// has no header row. Sent before the first row, also when there is
    /// none.
    Columns(Vec<String>),
    Row(Record),
}

/// Whether a visitor wants more rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// What a read learned about the file besides its rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReadSummary {
    /// Rows handed to the visitor.
    pub rows: u64,
    /// The file stopped before its end because the visitor asked.
    pub stopped: bool,
    /// An XLSX held a number in the rows read: Excel stores numbers as
    /// floating point, which the dialog says.
    pub numbers: bool,
    /// The encoding the CSV was decoded with: the one chosen, or the one its
    /// byte-order mark names.
    pub encoding: Option<&'static Encoding>,
}

/// Reads the file at `path` with `options`, handing the columns and then
/// each row to `visit` until the file ends or `visit` says
/// [`Flow::Stop`]. Fails, at the line or cell that caused it, on what the
/// file cannot give as written: undecodable bytes, a row with another
/// number of fields, an error cell, a record or a shared strings table over
/// its limit. Checks `cancel` as it goes. Blocks; run it on the blocking
/// pool.
pub fn read(
    path: &Path,
    options: &ReadOptions,
    cancel: &CancellationToken,
    visit: &mut dyn FnMut(ReadEvent) -> DriverResult<Flow>,
) -> DriverResult<ReadSummary> {
    match options {
        ReadOptions::Csv(options) => delimited::read(path, options, cancel, visit),
        ReadOptions::Xlsx(options) => workbook::read(path, options, cancel, visit),
    }
}

/// Rows the dialog's preview reads at most.
pub const PREVIEW_ROWS: usize = 50;

/// The first rows of a file, as the dialog previews them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preview {
    pub columns: Vec<String>,
    pub rows: Vec<Record>,
    /// The file holds more rows than these.
    pub more: bool,
    /// See [`ReadSummary::numbers`].
    pub numbers: bool,
    /// See [`ReadSummary::encoding`].
    pub encoding: Option<&'static Encoding>,
}

/// Reads the columns and up to `limit` rows of the file, as an import would
/// read them, so the preview shows what will be bound.
pub fn preview(
    path: &Path,
    options: &ReadOptions,
    limit: usize,
    cancel: &CancellationToken,
) -> DriverResult<Preview> {
    let mut preview = Preview::default();
    let summary = read(path, options, cancel, &mut |event| {
        match event {
            ReadEvent::Columns(columns) => preview.columns = columns,
            ReadEvent::Row(record) => {
                if preview.rows.len() >= limit {
                    preview.more = true;
                    return Ok(Flow::Stop);
                }
                preview.rows.push(record);
            }
        }
        Ok(Flow::Continue)
    })?;
    preview.numbers = summary.numbers;
    preview.encoding = summary.encoding;
    Ok(preview)
}

/// How often the readers look at the cancellation token, in rows or cells.
const CANCEL_EVERY: u64 = 1024;

fn check_cancel(cancel: &CancellationToken, count: u64) -> DriverResult<()> {
    if count.is_multiple_of(CANCEL_EVERY) && cancel.is_cancelled() {
        return Err(DriverError::cancelled());
    }
    Ok(())
}

/// The error for a file that could not be opened or read, without its
/// contents.
fn file_error(error: &std::io::Error) -> DriverError {
    use std::io::ErrorKind as K;
    match error.kind() {
        K::NotFound => DriverError::new(
            ErrorCode::NotFound,
            tr!("The file no longer exists. Choose it again."),
        ),
        K::PermissionDenied => DriverError::new(
            ErrorCode::PermissionDenied,
            tr!("The file could not be read: no permission, or another program holds it."),
        ),
        _ => DriverError::new(ErrorCode::Internal, tr!("The file could not be read.")),
    }
}
