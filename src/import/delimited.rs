//! CSV: `csv`'s reader over a decoder of the chosen encoding
//! ([proposal](../../docs/plans/csv-and-xlsx-reading-dependencies.md#encodings)).
//!
//! The decoder sits between the file and the CSV reader with fixed buffers
//! and hands it UTF-8. A byte-order mark overrides the chosen encoding. Bytes
//! that are not valid in the encoding stop the read at their offset: nothing
//! is replaced, since text from a CSV reaches the table as written. One
//! record is held at a time; a record longer than [`MAX_RECORD_BYTES`]
//! stops the read rather than grow, since an unbalanced quote would
//! otherwise swallow the rest of the file.
use super::{
    EmptyFields, Flow, ImportValue, ReadEvent, ReadSummary, Record, check_cancel, file_error,
};
use crate::drivers::{CancellationToken, DriverError, DriverResult, ErrorCode};
use crate::tr;
use encoding_rs::{Decoder, DecoderResult, Encoding};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The longest record a CSV import reads, in bytes of decoded text.
pub const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// What separates the fields of a CSV.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Delimiter {
    #[default]
    Comma,
    Semicolon,
    Tab,
    Pipe,
}

impl Delimiter {
    pub const ALL: [Self; 4] = [Self::Comma, Self::Semicolon, Self::Tab, Self::Pipe];

    pub fn byte(self) -> u8 {
        match self {
            Self::Comma => b',',
            Self::Semicolon => b';',
            Self::Tab => b'\t',
            Self::Pipe => b'|',
        }
    }

    /// The delimiter that appears most often outside quotes in `sample` (the
    /// file's first line), or a comma when none does: a starting point the
    /// preview confirms, never a decision.
    pub fn guess(sample: &str) -> Self {
        let mut counts = [0usize; 4];
        let mut quoted = false;
        for ch in sample.chars() {
            match ch {
                '"' => quoted = !quoted,
                '\n' | '\r' if !quoted => break,
                _ if !quoted => {
                    if let Some(index) = Self::ALL
                        .iter()
                        .position(|delimiter| char::from(delimiter.byte()) == ch)
                        && let Some(count) = counts.get_mut(index)
                    {
                        *count += 1;
                    }
                }
                _ => {}
            }
        }
        let mut best = Self::Comma;
        let mut best_count = 0;
        for (delimiter, count) in Self::ALL.into_iter().zip(counts) {
            if count > best_count {
                best = delimiter;
                best_count = count;
            }
        }
        best
    }
}

/// The encodings a CSV can be read in: `encoding_rs`'s, which are the
/// WHATWG Encoding Standard's, without its `replacement` encoding. UTF-8
/// first, the default.
pub const ENCODINGS: &[&Encoding] = &[
    encoding_rs::UTF_8,
    encoding_rs::UTF_16LE,
    encoding_rs::UTF_16BE,
    encoding_rs::WINDOWS_1252,
    encoding_rs::ISO_8859_15,
    encoding_rs::WINDOWS_1250,
    encoding_rs::WINDOWS_1251,
    encoding_rs::WINDOWS_1253,
    encoding_rs::WINDOWS_1254,
    encoding_rs::WINDOWS_1255,
    encoding_rs::WINDOWS_1256,
    encoding_rs::WINDOWS_1257,
    encoding_rs::WINDOWS_1258,
    encoding_rs::WINDOWS_874,
    encoding_rs::ISO_8859_2,
    encoding_rs::ISO_8859_3,
    encoding_rs::ISO_8859_4,
    encoding_rs::ISO_8859_5,
    encoding_rs::ISO_8859_6,
    encoding_rs::ISO_8859_7,
    encoding_rs::ISO_8859_8,
    encoding_rs::ISO_8859_8_I,
    encoding_rs::ISO_8859_10,
    encoding_rs::ISO_8859_13,
    encoding_rs::ISO_8859_14,
    encoding_rs::ISO_8859_16,
    encoding_rs::KOI8_R,
    encoding_rs::KOI8_U,
    encoding_rs::IBM866,
    encoding_rs::MACINTOSH,
    encoding_rs::X_MAC_CYRILLIC,
    encoding_rs::SHIFT_JIS,
    encoding_rs::EUC_JP,
    encoding_rs::ISO_2022_JP,
    encoding_rs::GBK,
    encoding_rs::GB18030,
    encoding_rs::BIG5,
    encoding_rs::EUC_KR,
    encoding_rs::X_USER_DEFINED,
];

/// How a CSV is read.
#[derive(Clone, Debug, PartialEq)]
pub struct CsvOptions {
    pub delimiter: Delimiter,
    /// The encoding chosen; a byte-order mark in the file overrides it.
    pub encoding: &'static Encoding,
    /// The record that names the columns, one-based; records before it are
    /// skipped. `None`: every record is data.
    pub header_row: Option<u32>,
    pub empty: EmptyFields,
    /// The longest record read, [`MAX_RECORD_BYTES`] unless a test says
    /// otherwise.
    pub max_record_bytes: u64,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: Delimiter::Comma,
            encoding: encoding_rs::UTF_8,
            header_row: Some(1),
            empty: EmptyFields::Null,
            max_record_bytes: MAX_RECORD_BYTES,
        }
    }
}

/// Why the decoder stopped, carried inside an `io::Error` through the CSV
/// reader.
#[derive(Debug)]
enum Stop {
    /// Bytes not valid in `encoding`, starting at this offset of the file.
    Malformed {
        offset: u64,
        encoding: &'static Encoding,
    },
    /// The record being read passed the limit.
    RecordTooLong,
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed { offset, .. } => write!(f, "malformed input at byte {offset}"),
            Self::RecordTooLong => f.write_str("record too long"),
        }
    }
}

impl std::error::Error for Stop {}

/// Bytes of decoded text the adapter keeps after handing them out, so the
/// reading loop can look at the byte that ended a record. The CSV reader
/// buffers 8 KiB ahead of what it parsed, so twice that is enough.
const TAIL: usize = 32 * 1024;

const BUFFER: usize = 8 * 1024;

/// A `Read` of UTF-8 decoded from `inner` in `encoding`, which never
/// replaces malformed input: it fails at it instead. `encoding_rs_io` would
/// replace it with U+FFFD, so this adapter is ours.
struct Decoding<R> {
    inner: R,
    decoder: Decoder,
    input: Box<[u8]>,
    start: usize,
    end: usize,
    eof: bool,
    output: Box<[u8]>,
    out_start: usize,
    out_end: usize,
    /// An error found after the output still to hand out.
    pending: Option<Stop>,
    finished: bool,
    /// Bytes of the file consumed.
    raw: u64,
    /// Bytes of UTF-8 handed out, the last [`TAIL`] of them kept in `tail`.
    handed: u64,
    tail: Box<[u8]>,
    max_record: u64,
    /// Where, in decoded bytes, the record being read starts; the reading
    /// loop moves it after each record.
    record_start: Arc<AtomicU64>,
}

impl<R: Read> Decoding<R> {
    fn new(
        inner: R,
        encoding: &'static Encoding,
        max_record: u64,
        record_start: Arc<AtomicU64>,
    ) -> Self {
        Self {
            inner,
            // Sniffs a byte-order mark, which then names the encoding, and
            // removes it.
            decoder: encoding.new_decoder(),
            input: vec![0; BUFFER].into_boxed_slice(),
            start: 0,
            end: 0,
            eof: false,
            output: vec![0; BUFFER].into_boxed_slice(),
            out_start: 0,
            out_end: 0,
            pending: None,
            finished: false,
            raw: 0,
            handed: 0,
            tail: vec![0; TAIL].into_boxed_slice(),
            max_record,
            record_start,
        }
    }

    /// The decoded byte at `offset`, while it is among the last [`TAIL`]
    /// handed out.
    fn byte_at(&self, offset: u64) -> Option<u8> {
        if offset >= self.handed || self.handed - offset > TAIL as u64 {
            return None;
        }
        self.tail.get((offset % TAIL as u64) as usize).copied()
    }

    /// Decodes the next part of the file into `output`.
    fn fill(&mut self) -> io::Result<()> {
        self.out_start = 0;
        self.out_end = 0;
        while self.out_end == 0 && self.pending.is_none() && !self.finished {
            if self.start == self.end && !self.eof {
                let read = loop {
                    match self.inner.read(&mut self.input) {
                        Ok(read) => break read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => return Err(error),
                    }
                };
                self.start = 0;
                self.end = read;
                self.eof = read == 0;
            }
            let last = self.eof;
            let input = self.input.get(self.start..self.end).unwrap_or_default();
            let (result, read, written) =
                self.decoder
                    .decode_to_utf8_without_replacement(input, &mut self.output, last);
            self.start += read;
            self.out_end = written;
            self.raw += read as u64;
            match result {
                DecoderResult::InputEmpty => {
                    if last {
                        self.finished = true;
                    }
                }
                DecoderResult::OutputFull => {}
                DecoderResult::Malformed(bad, after) => {
                    let offset = self.raw.saturating_sub(u64::from(after) + u64::from(bad));
                    self.pending = Some(Stop::Malformed {
                        offset,
                        encoding: self.decoder.encoding(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for Decoding<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.out_start == self.out_end {
            self.fill()?;
            if self.out_start == self.out_end {
                return match self.pending.take() {
                    Some(stop) => Err(io::Error::new(io::ErrorKind::InvalidData, stop)),
                    None => Ok(0),
                };
            }
        }
        let record_start = self.record_start.load(Ordering::Relaxed);
        if self.handed.saturating_sub(record_start) > self.max_record {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                Stop::RecordTooLong,
            ));
        }
        let available = self
            .output
            .get(self.out_start..self.out_end)
            .unwrap_or_default();
        let count = available.len().min(buf.len());
        let (Some(target), Some(source)) = (buf.get_mut(..count), available.get(..count)) else {
            return Ok(0);
        };
        target.copy_from_slice(source);
        for (index, byte) in source.iter().enumerate() {
            let at = (self.handed + index as u64) % TAIL as u64;
            if let Some(slot) = self.tail.get_mut(at as usize) {
                *slot = *byte;
            }
        }
        self.out_start += count;
        self.handed += count as u64;
        Ok(count)
    }
}

/// Reads the CSV at `path`; see [`super::read`].
pub(super) fn read(
    path: &Path,
    options: &CsvOptions,
    cancel: &CancellationToken,
    visit: &mut dyn FnMut(ReadEvent) -> DriverResult<Flow>,
) -> DriverResult<ReadSummary> {
    let file = File::open(path).map_err(|error| file_error(&error))?;
    let total = file.metadata().map(|metadata| metadata.len()).ok();
    read_from(file, total, options, cancel, visit)
}

/// Reads CSV from `source`, `total` bytes long when known.
pub(super) fn read_from(
    source: impl Read,
    total: Option<u64>,
    options: &CsvOptions,
    cancel: &CancellationToken,
    visit: &mut dyn FnMut(ReadEvent) -> DriverResult<Flow>,
) -> DriverResult<ReadSummary> {
    let record_start = Arc::new(AtomicU64::new(0));
    let decoding = Decoding::new(
        source,
        options.encoding,
        options.max_record_bytes,
        record_start.clone(),
    );
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(options.delimiter.byte())
        .has_headers(false)
        // Rows before the header may hold fewer fields; data rows are
        // checked against the columns here, with their line.
        .flexible(true)
        .from_reader(decoding);
    let mut record = csv::StringRecord::new();
    let mut summary = ReadSummary {
        encoding: Some(options.encoding),
        ..ReadSummary::default()
    };
    let mut index: u64 = 0;
    let mut width: Option<usize> = None;
    loop {
        check_cancel(cancel, index)?;
        let more = reader
            .read_record(&mut record)
            .map_err(|error| csv_error(error, options.max_record_bytes))?;
        let consumed = reader.position().byte();
        record_start.store(consumed, Ordering::Relaxed);
        let line = record_line(&record, reader.position().line(), || {
            consumed
                .checked_sub(1)
                .and_then(|last| reader.get_ref().byte_at(last))
        });
        if index == 0 {
            summary.encoding = Some(reader.get_ref().decoder.encoding());
        }
        if !more {
            break;
        }
        index += 1;
        match options.header_row.map(u64::from) {
            Some(header) if index < header => continue,
            Some(header) if index == header => {
                let names: Vec<String> = record.iter().map(str::to_owned).collect();
                width = Some(names.len());
                if visit(ReadEvent::Columns(names))? == Flow::Stop {
                    summary.stopped = true;
                    return Ok(summary);
                }
                continue;
            }
            _ => {}
        }
        let expected = match width {
            Some(width) => width,
            None => {
                // Only a file without a header row gets here: its first
                // record sets the columns.
                let names = vec![String::new(); record.len()];
                width = Some(names.len());
                if visit(ReadEvent::Columns(names))? == Flow::Stop {
                    summary.stopped = true;
                    return Ok(summary);
                }
                record.len()
            }
        };
        if record.len() != expected {
            return Err(DriverError::validation(tr!(
                "Line {0} has a different number of fields ({1}) than the file has columns ({2}). Check the delimiter, or fix that line.",
                line,
                record.len(),
                expected
            )));
        }
        let values = record
            .iter()
            .map(|field| match (field.is_empty(), options.empty) {
                (true, EmptyFields::Null) => ImportValue::Null,
                _ => ImportValue::Text(field.to_owned()),
            })
            .collect();
        let read = total
            .filter(|total| *total > 0)
            .map(|total| (reader.get_ref().raw as f64 / total as f64).min(1.0));
        summary.rows += 1;
        if visit(ReadEvent::Row(Record { line, values, read }))? == Flow::Stop {
            summary.stopped = true;
            return Ok(summary);
        }
    }
    if width.is_none() {
        // A file without data rows still says its columns: none, or a
        // header row beyond its end.
        if let Some(header) = options.header_row
            && u64::from(header) > index
            && index > 0
        {
            return Err(DriverError::validation(tr!(
                "The file ends at row {0}, so row {1} cannot be its header.",
                index,
                header
            )));
        }
        visit(ReadEvent::Columns(Vec::new()))?;
    }
    Ok(summary)
}

/// The line `record` starts on. The CSV reader counts the line breaks it
/// consumed (`consumed_lines` is one more than them), but its record
/// positions are taken before it skips blank lines and the `\n` of a
/// `\r\n` that ended the previous record, so they can be early. Counted
/// back from the end instead: the breaks inside the record's fields, and
/// its own `\n` when the reader consumed it (`last_byte`).
fn record_line(
    record: &csv::StringRecord,
    consumed_lines: u64,
    last_byte: impl FnOnce() -> Option<u8>,
) -> u64 {
    let inside: u64 = record
        .iter()
        .map(|field| field.bytes().filter(|byte| *byte == b'\n').count() as u64)
        .sum();
    let terminated = u64::from(last_byte() == Some(b'\n'));
    consumed_lines.saturating_sub(inside + terminated).max(1)
}

/// The error for a CSV that could not be read, at its line when known.
fn csv_error(error: csv::Error, limit: u64) -> DriverError {
    let line = error.position().map(|position| position.line());
    match error.into_kind() {
        csv::ErrorKind::Io(error) => {
            match error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<Stop>())
            {
                Some(Stop::Malformed { offset, encoding }) => DriverError::validation(tr!(
                    "The file is not valid {0} at byte {1}. Choose the encoding it was saved in.",
                    encoding.name(),
                    offset
                )),
                Some(Stop::RecordTooLong) => DriverError::validation(tr!(
                    "A record is longer than {0} MiB, probably because a quote is never closed. Check the file near the last line the preview shows.",
                    limit / (1024 * 1024)
                )),
                None => file_error(&error),
            }
        }
        csv::ErrorKind::Utf8 { pos, .. } => DriverError::new(
            ErrorCode::Internal,
            tr!(
                "Line {0} could not be read as text.",
                pos.map(|position| position.line()).or(line).unwrap_or(0)
            ),
        ),
        _ => DriverError::new(
            ErrorCode::Internal,
            tr!("Line {0} could not be read.", line.unwrap_or(0)),
        ),
    }
}
