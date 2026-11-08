//! Reading, mapping, drafting and batching on dedicated synthetic files,
//! written by each test into a folder of its own.
use super::delimited::{self, ENCODINGS};
use super::mapping::{Mapping, MatchBy, Problem, TargetColumn};
use super::pipeline::{
    BATCH_ROWS, Batch, BatchWriter, Batcher, CHANNEL_BATCHES, ImportPlan, Outcome, Progress,
};
use super::workbook::{self, cell_address};
use super::*;
use crate::drivers::{Driver, MySqlDriver, ObjectKind, ObjectPath, PostgresDriver, async_trait};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A folder of the test's own, removed when it ends.
struct Folder(PathBuf);

impl Folder {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("jykre-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        // Best effort: a leftover folder in the temporary directory harms
        // nothing.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(value: &str) -> ImportValue {
    ImportValue::Text(value.into())
}

/// Every event of a read, or its error.
fn events(path: &Path, options: &ReadOptions) -> DriverResult<(Vec<String>, Vec<Record>)> {
    let mut columns = None;
    let mut rows = Vec::new();
    read(path, options, &CancellationToken::new(), &mut |event| {
        match event {
            ReadEvent::Columns(names) => {
                assert!(columns.is_none(), "the columns come once");
                assert!(rows.is_empty(), "the columns come first");
                columns = Some(names);
            }
            ReadEvent::Row(record) => rows.push(record),
        }
        Ok(Flow::Continue)
    })?;
    Ok((columns.expect("the columns always come"), rows))
}

fn csv(options: CsvOptions) -> ReadOptions {
    ReadOptions::Csv(options)
}

fn values(rows: &[Record]) -> Vec<Vec<ImportValue>> {
    rows.iter().map(|row| row.values.clone()).collect()
}

#[test]
fn csv_fields_keep_quotes_line_breaks_and_delimiters_as_written() {
    let folder = Folder::new();
    for (delimiter, separator) in [
        (Delimiter::Comma, ","),
        (Delimiter::Semicolon, ";"),
        (Delimiter::Tab, "\t"),
        (Delimiter::Pipe, "|"),
    ] {
        let body = format!(
            "id{s}name{s}note\r\n1{s}\"Ann \"\"the\"\" first\"{s}\"two\nlines\"\r\n2{s} Bob {s}NULL\n",
            s = separator
        );
        let path = folder.file("people.csv", body.as_bytes());
        let (columns, rows) = events(
            &path,
            &csv(CsvOptions {
                delimiter,
                ..CsvOptions::default()
            }),
        )
        .unwrap();
        assert_eq!(columns, ["id", "name", "note"], "{delimiter:?}");
        assert_eq!(
            values(&rows),
            [
                vec![text("1"), text("Ann \"the\" first"), text("two\nlines")],
                // Spaces stay, and the text NULL is text.
                vec![text("2"), text(" Bob "), text("NULL")],
            ],
            "{delimiter:?}"
        );
        // Each row knows the line it starts on, after a field's line break.
        assert_eq!(rows[0].line, 2);
        assert_eq!(rows[1].line, 4);
        assert_eq!(rows[1].read, Some(1.0));
    }
}

#[test]
fn empty_csv_fields_are_null_or_empty_text_as_chosen() {
    let folder = Folder::new();
    let path = folder.file("empty.csv", b"a,b,c\n,\"\",x\n");
    let (_, rows) = events(&path, &csv(CsvOptions::default())).unwrap();
    // `csv` cannot tell `""` from nothing: both are empty (decision I-4).
    assert_eq!(
        values(&rows),
        [vec![ImportValue::Null, ImportValue::Null, text("x")]]
    );
    let (_, rows) = events(
        &path,
        &csv(CsvOptions {
            empty: EmptyFields::EmptyText,
            ..CsvOptions::default()
        }),
    )
    .unwrap();
    assert_eq!(values(&rows), [vec![text(""), text(""), text("x")]]);
}

#[test]
fn the_header_row_can_be_lower_or_absent_and_blank_lines_are_skipped() {
    let folder = Folder::new();
    let path = folder.file("report.csv", b"Monthly report\n\nid,total\n1,10\n\n2,20\n");
    let (columns, rows) = events(
        &path,
        &csv(CsvOptions {
            header_row: Some(2),
            ..CsvOptions::default()
        }),
    )
    .unwrap();
    assert_eq!(columns, ["id", "total"]);
    assert_eq!(
        values(&rows),
        [vec![text("1"), text("10")], vec![text("2"), text("20")]]
    );
    let path = folder.file("plain.csv", b"1,10\n2,20\n");
    let (columns, rows) = events(
        &path,
        &csv(CsvOptions {
            header_row: None,
            ..CsvOptions::default()
        }),
    )
    .unwrap();
    assert_eq!(columns, ["", ""]);
    assert_eq!(rows.len(), 2);
    // A header beyond the file's end is said, not guessed.
    let error = events(
        &path,
        &csv(CsvOptions {
            header_row: Some(5),
            ..CsvOptions::default()
        }),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Validation);
    assert!(error.message.contains("row 5"), "{}", error.message);
    // An empty file has no columns and no rows.
    let path = folder.file("nothing.csv", b"");
    assert_eq!(
        events(&path, &csv(CsvOptions::default())).unwrap(),
        (Vec::new(), Vec::new())
    );
}

#[test]
fn rows_know_their_line_past_blank_lines_and_either_line_ending() {
    let folder = Folder::new();
    let path = folder.file("lines.csv", b"a,b\n\n1,2\r\n\r\n3,\"x\r\ny\"\n\n\n5,6");
    let (_, rows) = events(&path, &csv(CsvOptions::default())).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.line).collect::<Vec<_>>(),
        [3, 5, 9]
    );
    assert_eq!(rows[1].values, [text("3"), text("x\r\ny")]);
    let path = folder.file("late.csv", b"a,b\r\n1,2\r\n\r\n3\r\n");
    let error = events(&path, &csv(CsvOptions::default())).unwrap_err();
    assert!(
        error.message.contains("Line 4 has 1 fields"),
        "{}",
        error.message
    );
}

#[test]
fn a_ragged_row_is_refused_at_its_line() {
    let folder = Folder::new();
    let path = folder.file("ragged.csv", b"a,b\n1,2\n3\n4,5\n");
    let error = events(&path, &csv(CsvOptions::default())).unwrap_err();
    assert_eq!(error.code, ErrorCode::Validation);
    assert!(
        error.message.contains("Line 3 has 1 fields"),
        "{}",
        error.message
    );
}

#[test]
fn encodings_decode_and_a_byte_order_mark_overrides_the_choice() {
    let folder = Folder::new();
    // A UTF-8 BOM is removed.
    let path = folder.file("bom.csv", b"\xEF\xBB\xBFname\nJos\xC3\xA9\n");
    let (columns, rows) = events(&path, &csv(CsvOptions::default())).unwrap();
    assert_eq!(columns, ["name"]);
    assert_eq!(values(&rows), [vec![text("José")]]);
    // Windows-1252, chosen.
    let path = folder.file("latin.csv", b"name;city\nJos\xE9;S\xE3o Paulo\n");
    let options = csv(CsvOptions {
        delimiter: Delimiter::Semicolon,
        encoding: encoding_rs::WINDOWS_1252,
        ..CsvOptions::default()
    });
    let (_, rows) = events(&path, &options).unwrap();
    assert_eq!(values(&rows), [vec![text("José"), text("São Paulo")]]);
    // UTF-16LE with a BOM, although UTF-8 was chosen.
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in "id,nome\n1,Ação\n".encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let path = folder.file("wide.csv", &utf16);
    let preview = preview(
        &path,
        &csv(CsvOptions::default()),
        10,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(preview.columns, ["id", "nome"]);
    assert_eq!(preview.rows[0].values, [text("1"), text("Ação")]);
    assert_eq!(preview.encoding, Some(encoding_rs::UTF_16LE));
    // UTF-16LE without a BOM reads only when chosen.
    let path = folder.file("bare.csv", &utf16[2..]);
    let options = csv(CsvOptions {
        encoding: encoding_rs::UTF_16LE,
        ..CsvOptions::default()
    });
    let (_, rows) = events(&path, &options).unwrap();
    assert_eq!(values(&rows), [vec![text("1"), text("Ação")]]);
    // UTF-8 is offered first, and the list holds no `replacement`.
    assert_eq!(ENCODINGS.first(), Some(&encoding_rs::UTF_8));
    assert!(!ENCODINGS.contains(&encoding_rs::REPLACEMENT));
}

#[test]
fn malformed_input_is_refused_at_its_offset_never_replaced() {
    let folder = Folder::new();
    // 0xE9 alone is not UTF-8: it sits at byte 9.
    let path = folder.file("bad.csv", b"name\nJos\xE9\nAna\n");
    let mut seen = Vec::new();
    let error = read(
        &path,
        &csv(CsvOptions::default()),
        &CancellationToken::new(),
        &mut |event| {
            if let ReadEvent::Row(record) = event {
                seen.push(record.values);
            }
            Ok(Flow::Continue)
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Validation);
    assert!(
        error.message.contains("not valid UTF-8 at byte 8"),
        "{}",
        error.message
    );
    assert!(
        seen.iter().flatten().all(|value| match value {
            ImportValue::Text(text) => !text.contains('\u{FFFD}'),
            _ => true,
        }),
        "nothing is replaced"
    );
    // A lone UTF-16 surrogate is malformed too.
    let mut utf16 = vec![0xFF, 0xFE];
    for unit in [u16::from(b'a'), u16::from(b'\n'), 0xD800, u16::from(b'\n')] {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    let path = folder.file("surrogate.csv", &utf16);
    let error = events(&path, &csv(CsvOptions::default())).unwrap_err();
    assert!(
        error.message.contains("not valid UTF-16LE"),
        "{}",
        error.message
    );
}

#[test]
fn a_record_over_the_limit_is_refused_instead_of_growing() {
    let folder = Folder::new();
    // A quote that never closes would swallow the rest of the file.
    let mut body = b"a,b\n1,\"open\n".to_vec();
    body.extend(std::iter::repeat_n(b'x', 256 * 1024));
    let path = folder.file("unbalanced.csv", &body);
    let error = events(
        &path,
        &csv(CsvOptions {
            max_record_bytes: 64 * 1024,
            ..CsvOptions::default()
        }),
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Validation);
    assert!(error.message.contains("longer than"), "{}", error.message);
    // The same file within the limit reads to its end.
    let (_, rows) = events(&path, &csv(CsvOptions::default())).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(delimited::MAX_RECORD_BYTES, 64 * 1024 * 1024);
}

#[test]
fn reading_stops_when_cancelled_or_when_the_visitor_has_enough() {
    let folder = Folder::new();
    let mut body = String::from("n\n");
    for index in 0..3000 {
        body.push_str(&format!("{index}\n"));
    }
    let path = folder.file("many.csv", body.as_bytes());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = read(&path, &csv(CsvOptions::default()), &cancel, &mut |_| {
        Ok(Flow::Continue)
    })
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Cancelled);
    let preview = preview(
        &path,
        &csv(CsvOptions::default()),
        5,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(preview.rows.len(), 5);
    assert!(preview.more);
    assert_eq!(preview.rows[4].values, [text("4")]);
}

#[test]
fn inspecting_a_file_names_its_kind_delimiter_and_byte_order_mark() {
    let folder = Folder::new();
    let path = folder.file("a.csv", b"\xFF\xFEi\0d\0;\0n\0\n\0");
    let info = inspect(&path).unwrap();
    assert_eq!(info.kind, FileKind::Csv);
    assert_eq!(info.delimiter, Delimiter::Semicolon);
    assert_eq!(info.bom, Some(encoding_rs::UTF_16LE));
    let path = folder.file("b.txt", b"a\tb\n");
    let info = inspect(&path).unwrap();
    assert_eq!((info.delimiter, info.bom), (Delimiter::Tab, None));
    let rows = r#"<row r="1"><c r="A1"><v>1</v></c></row>"#;
    let path = handmade_xlsx(&folder, "c.xlsx", false, None, "A1", rows);
    assert_eq!(inspect(&path).unwrap().sheets, ["Data", "Other"]);
    let error = inspect(&folder.0.join("missing.csv")).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
}

#[test]
fn delimiters_are_guessed_from_the_first_line_outside_quotes() {
    assert_eq!(Delimiter::guess("a;b;c\n1,2;3"), Delimiter::Semicolon);
    assert_eq!(Delimiter::guess("a\tb\tc"), Delimiter::Tab);
    assert_eq!(Delimiter::guess("\"x;y;z\",b"), Delimiter::Comma);
    assert_eq!(Delimiter::guess("a|b"), Delimiter::Pipe);
    assert_eq!(Delimiter::guess("single"), Delimiter::Comma);
}

#[test]
fn file_kinds_follow_the_extension_and_other_workbooks_are_refused() {
    assert_eq!(file_kind(Path::new("a.XLSX")).unwrap(), FileKind::Xlsx);
    assert_eq!(file_kind(Path::new("a.csv")).unwrap(), FileKind::Csv);
    assert_eq!(file_kind(Path::new("a.tsv")).unwrap(), FileKind::Csv);
    assert_eq!(file_kind(Path::new("noext")).unwrap(), FileKind::Csv);
    for refused in ["a.xls", "a.xlsb", "a.ods", "a.xlsm"] {
        let error = file_kind(Path::new(refused)).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation, "{refused}");
    }
}

/// An `.xlsx` written part by part: `shared` strings, a 1904 epoch, and the
/// sheet's `<sheetData>` content. Style 1 is a date, 2 a time, 3 a
/// duration.
fn handmade_xlsx(
    folder: &Folder,
    name: &str,
    epoch_1904: bool,
    shared: Option<&str>,
    dimension: &str,
    rows: &str,
) -> PathBuf {
    let path = folder.0.join(name);
    let file = std::fs::File::create(&path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut part = |name: &str, body: &str| {
        zip.start_file(name, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    };
    part(
        "[Content_Types].xml",
        r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#,
    );
    part(
        "_rels/.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
    );
    part(
        "xl/workbook.xml",
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><workbookPr date1904="{}"/><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Other" sheetId="2" r:id="rId2"/></sheets></workbook>"#,
            u8::from(epoch_1904)
        ),
    );
    part(
        "xl/_rels/workbook.xml.rels",
        r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#,
    );
    part(
        "xl/styles.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cellXfs count="4"><xf numFmtId="0"/><xf numFmtId="14"/><xf numFmtId="21"/><xf numFmtId="46"/></cellXfs></styleSheet>"#,
    );
    if let Some(shared) = shared {
        part("xl/sharedStrings.xml", shared);
    }
    part(
        "xl/worksheets/sheet1.xml",
        &format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="{dimension}"/><sheetData>{rows}</sheetData></worksheet>"#
        ),
    );
    part(
        "xl/worksheets/sheet2.xml",
        r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData/></worksheet>"#,
    );
    zip.finish().unwrap();
    path
}

const SHARED: &str = r#"<?xml version="1.0" encoding="UTF-8"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="3" uniqueCount="3"><si><t>id</t></si><si><t>name</t></si><si><t>Ann</t></si></sst>"#;

fn xlsx(sheet: &str) -> ReadOptions {
    ReadOptions::Xlsx(XlsxOptions::new(sheet))
}

#[test]
fn xlsx_cells_become_the_values_adr_0040_names() {
    let folder = Folder::new();
    let rows = concat!(
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="inlineStr"><is><t>amount</t></is></c>"#,
        r#"<c r="D1" t="inlineStr"><is><t>ok</t></is></c><c r="E1" t="inlineStr"><is><t>when</t></is></c>"#,
        r#"<c r="F1" t="inlineStr"><is><t>sum</t></is></c><c r="G1" t="inlineStr"><is><t>lazy</t></is></c></row>"#,
        r#"<row r="2"><c r="A2"><v>1</v></c><c r="B2" t="s"><v>2</v></c><c r="C2"><v>0.1</v></c>"#,
        r#"<c r="D2" t="b"><v>1</v></c><c r="E2" s="1"><v>45943.541</v></c><c r="F2"><f>A2+1</f><v>2</v></c><c r="G2"><f>A2+1</f></c></row>"#,
        // A row without values is left out, as a blank CSV line is.
        r#"<row r="3"><c r="A3" s="1"/></row>"#,
        r#"<row r="4"><c r="A4"><v>123456789012345678</v></c><c r="C4"><v>1E-7</v></c><c r="D4" t="b"><v>0</v></c>"#,
        r#"<c r="E4" s="2"><v>0.5</v></c><c r="F4" s="3"><v>1.5</v></c><c r="G4" t="d"><v>2024-02-29T10:00:00</v></c></row>"#,
    );
    let path = handmade_xlsx(&folder, "cells.xlsx", false, Some(SHARED), "A1:G4", rows);
    assert_eq!(workbook::sheets(&path).unwrap(), ["Data", "Other"]);
    let (columns, records) = events(&path, &xlsx("Data")).unwrap();
    assert_eq!(
        columns,
        ["id", "name", "amount", "ok", "when", "sum", "lazy"]
    );
    assert_eq!(
        values(&records),
        [
            vec![
                text("1"),
                text("Ann"),
                text("0.1"),
                ImportValue::Boolean(true),
                text("2025-10-13 12:59:02.400"),
                // A formula's cached value; without one, NULL.
                text("2"),
                ImportValue::Null,
            ],
            vec![
                // Already rounded in the file: the double's shortest text.
                text("123456789012345680"),
                ImportValue::Null,
                text("0.0000001"),
                ImportValue::Boolean(false),
                text("12:00:00"),
                text("36:00:00"),
                text("2024-02-29T10:00:00"),
            ],
        ]
    );
    assert_eq!(records[1].line, 4);
    assert_eq!(records[1].read, Some(1.0));
    let preview = preview(&path, &xlsx("Data"), 10, &CancellationToken::new()).unwrap();
    assert!(preview.numbers, "the floating point note applies");
    // An empty sheet has no columns.
    assert_eq!(
        events(&path, &xlsx("Other")).unwrap(),
        (Vec::new(), Vec::new())
    );
    let missing = events(&path, &xlsx("Gone")).unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

#[test]
fn the_1904_epoch_shifts_dates_and_xlsxwriter_files_read_back() {
    let folder = Folder::new();
    let rows = r#"<row r="1"><c r="A1" t="inlineStr"><is><t>day</t></is></c></row><row r="2"><c r="A2" s="1"><v>1</v></c></row>"#;
    let path = handmade_xlsx(&folder, "mac.xlsx", true, None, "A1:A2", rows);
    let (_, records) = events(&path, &xlsx("Data")).unwrap();
    assert_eq!(values(&records), [vec![text("1904-01-02 00:00:00")]]);
    let path = handmade_xlsx(&folder, "pc.xlsx", false, None, "A1:A2", rows);
    let (_, records) = events(&path, &xlsx("Data")).unwrap();
    assert_eq!(values(&records), [vec![text("1900-01-01 00:00:00")]]);

    // A workbook as rust_xlsxwriter (and so `jykre::export`) writes it.
    let path = folder.0.join("written.xlsx");
    let mut book = rust_xlsxwriter::Workbook::new();
    let sheet = book.add_worksheet();
    sheet.set_name("Sales").unwrap();
    sheet.write_string(0, 0, "code").unwrap();
    sheet.write_string(0, 1, "price").unwrap();
    sheet.write_string(0, 2, "paid").unwrap();
    sheet.write_string(0, 3, "due").unwrap();
    sheet.write_string(0, 4, "total").unwrap();
    sheet.write_string(1, 0, "007").unwrap();
    sheet.write_number(1, 1, 19.99).unwrap();
    sheet.write_boolean(1, 2, true).unwrap();
    let date = rust_xlsxwriter::Format::new().set_num_format("yyyy-mm-dd");
    sheet
        .write_number_with_format(1, 3, 45658.0, &date)
        .unwrap();
    sheet
        .write_formula(
            1,
            4,
            rust_xlsxwriter::Formula::new("=B2*2").set_result("39.98"),
        )
        .unwrap();
    book.save(&path).unwrap();
    let (columns, records) = events(&path, &xlsx("Sales")).unwrap();
    assert_eq!(columns, ["code", "price", "paid", "due", "total"]);
    assert_eq!(
        values(&records),
        [vec![
            text("007"),
            text("19.99"),
            ImportValue::Boolean(true),
            text("2025-01-01 00:00:00"),
            text("39.98"),
        ]]
    );
}

#[test]
fn xlsx_refusals_name_the_cell() {
    let folder = Folder::new();
    let header = r#"<row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c><c r="B1" t="inlineStr"><is><t>b</t></is></c></row>"#;
    let cases = [
        (
            r#"<row r="2"><c r="A2"><v>1</v></c><c r="B2" t="e"><v>#N/A</v></c></row>"#,
            "Cell B2 holds the error #N/A",
        ),
        (
            r#"<row r="3"><c r="A3"><v>1</v></c></row><row r="2"><c r="A2"><v>1</v></c></row>"#,
            "Cell A2 comes after row 3",
        ),
        (
            r#"<row r="2"><c r="B2"><v>1</v></c><c r="A2"><v>1</v></c></row>"#,
            "Cell A2 comes after column B2",
        ),
        (
            r#"<row r="2"><c r="A2"><v>1</v></c><c r="D2"><v>1</v></c></row>"#,
            "Cell D2 holds a value right of the sheet's 2 columns",
        ),
        (
            r#"<row r="2"><c r="A2" s="1"><v>-1</v></c></row>"#,
            "Cell A2 holds a date outside",
        ),
    ];
    for (index, (rows, expected)) in cases.into_iter().enumerate() {
        let body = format!("{header}{rows}");
        let path = handmade_xlsx(
            &folder,
            &format!("bad{index}.xlsx"),
            false,
            None,
            "A1:D3",
            &body,
        );
        let error = events(&path, &xlsx("Data")).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation, "{expected}");
        assert!(error.message.contains(expected), "{}", error.message);
    }
    // An error cell in a row above the header is skipped with its row.
    let lower = header
        .replace("r=\"1\"", "r=\"2\"")
        .replace("A1", "A2")
        .replace("B1", "B2");
    let body = format!(r#"<row r="1"><c r="A1" t="e"><v>#REF!</v></c></row>{lower}"#);
    let path = handmade_xlsx(&folder, "above.xlsx", false, None, "A1:B2", &body);
    let options = ReadOptions::Xlsx(XlsxOptions {
        header_row: Some(2),
        ..XlsxOptions::new("Data")
    });
    assert_eq!(events(&path, &options).unwrap().0, ["a", "b"]);
    // A header row without any value is said.
    let row_three = r#"<row r="3"><c r="A3"><v>1</v></c></row>"#;
    let path = handmade_xlsx(
        &folder,
        "headless.xlsx",
        false,
        None,
        "A1:B3",
        &format!("{lower}{row_three}"),
    );
    let options = ReadOptions::Xlsx(XlsxOptions {
        header_row: Some(1),
        ..XlsxOptions::new("Data")
    });
    let error = events(&path, &options).unwrap_err();
    assert!(
        error.message.contains("Row 1, the header row"),
        "{}",
        error.message
    );
}

/// The read of `rows` as the sheet `Data`, with `shared` strings.
fn read_handmade(
    folder: &Folder,
    name: &str,
    shared: Option<&str>,
    rows: &str,
) -> DriverResult<(Vec<String>, Vec<Record>)> {
    let path = handmade_xlsx(folder, name, false, shared, "A1:B3", rows);
    events(&path, &xlsx("Data"))
}

#[test]
fn malformed_workbooks_are_refused_with_a_message_never_a_panic() {
    let folder = Folder::new();
    let header = r#"<row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row>"#;
    let refused = [
        r#"<row r="2"><c r="2A"><v>1</v></c></row>"#,
        r#"<row r="2"><c r="A0"><v>1</v></c></row>"#,
        // Numbers that are not numbers, or no double can hold.
        r#"<row r="2"><c r="A2" t="n"><v>abc</v></c></row>"#,
        r#"<row r="2"><c r="A2"><v>1E999</v></c></row>"#,
        r#"<row r="2"><c r="A2"><v>-1E999</v></c></row>"#,
        r#"<row r="2"><c r="A2" t="b"><v>maybe</v></c></row>"#,
        r#"<row r="2"><c r="A2" t="s"><v>99999999</v></c></row>"#,
        r#"<row r="2"><c r="A2" t="nonsense"><v>1</v></c></row>"#,
        r#"<row r="x"><c r="A2"><v>1</v></c></row>"#,
        // A sheet cut short.
        r#"<row r="2"><c r="A2"><v>1</v></c></row><row r="3"><c r="A3"><v>"#,
        r#"<row r="2"><c r="A2"><v>1"#,
    ];
    let mut accepted = Vec::new();
    for (index, rows) in refused.into_iter().enumerate() {
        let shared = (rows.contains("t=\"s\"")).then_some(SHARED);
        let result = read_handmade(
            &folder,
            &format!("malformed{index}.xlsx"),
            shared,
            &format!("{header}{rows}"),
        );
        match result {
            Err(error) if error.code == ErrorCode::Validation && !error.message.is_empty() => {}
            other => accepted.push(format!("{rows} => {other:?}")),
        }
    }
    assert!(accepted.is_empty(), "{accepted:#?}");
    // Empty values and styles past the table read as what they are.
    let (_, records) = read_handmade(
        &folder,
        "empty.xlsx",
        None,
        &format!(
            r#"{header}<row r="2"><c r="A2"><v></v></c></row><row r="3"><c r="A3" s="999"><v>99999999999999999999999</v></c></row>"#
        ),
    )
    .unwrap();
    assert_eq!(
        values(&records),
        [vec![text("100000000000000000000000")]],
        "an empty <v> is an empty cell; a huge finite number keeps its double"
    );
    // A value without a type that is not a number is its text, as written.
    let (_, records) = read_handmade(
        &folder,
        "untyped.xlsx",
        None,
        &format!(r#"{header}<row r="2"><c r="A2"><v>abc</v></c></row>"#),
    )
    .unwrap();
    assert_eq!(values(&records), [vec![text("abc")]]);
    // A shared strings table cut short is refused when the workbook opens.
    let cut = &SHARED[..SHARED.len() - 30];
    let rows = r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#;
    let error = read_handmade(&folder, "cut-shared.xlsx", Some(cut), rows).unwrap_err();
    assert_eq!(error.code, ErrorCode::Validation, "{}", error.message);
    let path = folder.0.join("cut-shared.xlsx");
    assert!(workbook::sheets(&path).is_err());
}

/// `calamine` 0.38 adds up a cell reference's row and column in `u32`
/// without checking (`get_row_and_optional_column`): an absurd reference
/// overflows, which panics with overflow checks (debug builds) and wraps
/// without them. Read the way the dialog and the import read, on the
/// blocking pool, such a file is a failed read, never a crash.
#[tokio::test]
async fn absurd_cell_references_fail_the_read() {
    let folder = Folder::new();
    let header = r#"<row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c></row>"#;
    for (index, rows) in [
        r#"<row r="2"><c r="A99999999999999999999"><v>1</v></c></row>"#,
        r#"<row r="2"><c r="ZZZZZZZZ2"><v>1</v></c></row>"#,
        r#"<row r="99999999999999999999"><c><v>1</v></c></row>"#,
        // Within u32, past what a sheet holds.
        r#"<row r="2"><c r="A4000000000"><v>1</v></c></row>"#,
        r#"<row r="2"><c r="ZZZZ2"><v>1</v></c></row>"#,
    ]
    .into_iter()
    .enumerate()
    {
        let path = handmade_xlsx(
            &folder,
            &format!("absurd{index}.xlsx"),
            false,
            None,
            "A1:B3",
            &format!("{header}{rows}"),
        );
        let result = off_thread(move || {
            read(&path, &xlsx("Data"), &CancellationToken::new(), &mut |_| {
                Ok(Flow::Continue)
            })
        })
        .await;
        let error = result.expect_err(rows);
        assert!(!error.message.is_empty(), "{rows}");
    }
}

#[tokio::test]
async fn a_panic_while_reading_is_a_failed_read_not_a_crash() {
    let result: DriverResult<()> = off_thread(|| {
        unreachable!("a reader that panics on a malformed file");
    })
    .await;
    let error = result.unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
    assert_eq!(error.message, "Reading the file stopped unexpectedly.");
}

#[test]
fn without_a_header_the_declared_dimension_sets_the_columns() {
    let folder = Folder::new();
    let rows = r#"<row r="1"><c r="A1"><v>1</v></c></row><row r="2"><c r="A2"><v>2</v></c><c r="C2"><v>3</v></c></row>"#;
    let path = handmade_xlsx(&folder, "bare.xlsx", false, None, "A1:C2", rows);
    let options = ReadOptions::Xlsx(XlsxOptions {
        header_row: None,
        ..XlsxOptions::new("Data")
    });
    let (columns, records) = events(&path, &options).unwrap();
    assert_eq!(columns, ["", "", ""]);
    assert_eq!(
        values(&records),
        [
            vec![text("1"), ImportValue::Null, ImportValue::Null],
            vec![text("2"), ImportValue::Null, text("3")],
        ]
    );
}

#[test]
fn a_shared_strings_table_over_the_limit_is_refused_before_opening() {
    let folder = Folder::new();
    let rows = r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#;
    let path = handmade_xlsx(&folder, "big.xlsx", false, Some(SHARED), "A1", rows);
    let options = ReadOptions::Xlsx(XlsxOptions {
        max_shared_strings: 100,
        ..XlsxOptions::new("Data")
    });
    let error = events(&path, &options).unwrap_err();
    assert_eq!(error.code, ErrorCode::Validation);
    assert!(error.message.contains("CSV"), "{}", error.message);
    // The check needs only the zip's directory, never `calamine`.
    let file = std::fs::File::open(&path).unwrap();
    assert!(workbook::check_shared_strings(file, 100).is_err());
    let file = std::fs::File::open(&path).unwrap();
    assert!(workbook::check_shared_strings(file, 1024 * 1024).is_ok());
    // A table that declares far more strings than it holds would make
    // `calamine` reserve room for them all.
    let lying = SHARED.replace("uniqueCount=\"3\"", "uniqueCount=\"900000000000\"");
    let path = handmade_xlsx(&folder, "lying.xlsx", false, Some(&lying), "A1", rows);
    let error = events(&path, &xlsx("Data")).unwrap_err();
    assert!(error.message.contains("declares more"), "{}", error.message);
    // Not a workbook at all.
    let path = folder.file("fake.xlsx", b"id,name\n");
    let error = workbook::sheets(&path).unwrap_err();
    assert!(error.message.contains("not an .xlsx"), "{}", error.message);
}

#[test]
fn cell_addresses_and_dates_follow_excel() {
    assert_eq!(cell_address(0, 0), "A1");
    assert_eq!(cell_address(9, 25), "Z10");
    assert_eq!(cell_address(0, 26), "AA1");
    assert_eq!(cell_address(1_048_575, 16_383), "XFD1048576");
    use calamine::{ExcelDateTime, ExcelDateTimeType as T};
    let date =
        |serial, kind, epoch| workbook::datetime_text(&ExcelDateTime::new(serial, kind, epoch));
    assert_eq!(
        date(45943.541, T::DateTime, false).as_deref(),
        Some("2025-10-13 12:59:02.400")
    );
    assert_eq!(date(0.25, T::DateTime, false).as_deref(), Some("06:00:00"));
    assert_eq!(
        date(-0.25, T::TimeDelta, false).as_deref(),
        Some("-06:00:00")
    );
    assert_eq!(date(0.0, T::DateTime, true).as_deref(), Some("00:00:00"));
    assert_eq!(
        date(1.0, T::DateTime, true).as_deref(),
        Some("1904-01-02 00:00:00")
    );
    assert_eq!(date(2_958_466.0, T::DateTime, false), None);
    assert_eq!(date(f64::NAN, T::DateTime, false), None);
}

fn column(name: &str) -> TargetColumn {
    TargetColumn {
        name: name.into(),
        data_type: None,
        required: false,
        generated: false,
    }
}

#[test]
fn columns_match_by_name_ignoring_case_or_by_position() {
    let file: Vec<String> = ["Name", "ID", "extra", "id"].map(String::from).into();
    let targets = vec![
        column("id"),
        column("name"),
        TargetColumn {
            generated: true,
            ..column("total")
        },
        column("ID"),
        column("missing"),
    ];
    let mapping = Mapping::new(targets.clone(), &file, MatchBy::Name);
    // Exact names first: `id` takes `id`, `ID` takes `ID`.
    assert_eq!(mapping.source(0), Some(3));
    assert_eq!(mapping.source(1), Some(0));
    assert_eq!(
        mapping.source(2),
        None,
        "a generated column is never matched"
    );
    assert_eq!(mapping.source(3), Some(1));
    assert_eq!(mapping.source(4), None);
    assert!(mapping.problems(file.len()).is_empty());
    assert_eq!(
        mapping
            .columns()
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "name", "ID"]
    );
    let by_position = Mapping::new(targets, &file, MatchBy::Position);
    assert_eq!(
        (0..5)
            .map(|index| by_position.source(index))
            .collect::<Vec<_>>(),
        [Some(0), Some(1), None, Some(2), Some(3)]
    );
    assert_eq!(MatchBy::default_for(true), MatchBy::Name);
    assert_eq!(MatchBy::default_for(false), MatchBy::Position);
}

#[test]
fn mapping_refusals_come_before_anything_runs() {
    let targets = vec![
        TargetColumn {
            required: true,
            ..column("id")
        },
        TargetColumn {
            generated: true,
            ..column("total")
        },
        column("note"),
    ];
    let mut mapping = Mapping::new(targets, &[], MatchBy::Name);
    assert_eq!(
        mapping.problems(0),
        [
            Problem::NothingMapped,
            Problem::RequiredLeftOut("id".into())
        ]
    );
    mapping.set(0, Some(0));
    mapping.set(1, Some(1));
    mapping.set(2, Some(5));
    assert_eq!(
        mapping.problems(2),
        [
            Problem::GeneratedFilled("total".into()),
            Problem::MissingSource("note".into())
        ]
    );
    for problem in mapping.problems(2) {
        assert!(!problem.message().is_empty());
    }
    // One file column may fill two table columns.
    mapping.set(1, None);
    mapping.set(2, Some(0));
    assert_eq!(
        mapping.project(vec![text("7"), text("x")]),
        [text("7"), text("7")]
    );
}

#[test]
fn cached_columns_are_required_only_when_nothing_else_fills_them() {
    use crate::drivers::MetadataObject;
    let table = ObjectPath::default().child(ObjectKind::Table, "t");
    let mut cached = MetadataObject::new(ObjectKind::Column, &table, "code");
    cached.nullable = Some(false);
    assert!(TargetColumn::from_cached(&cached).required);
    cached.default_value = Some("0".into());
    assert!(!TargetColumn::from_cached(&cached).required);
    cached.default_value = None;
    cached.primary_key = Some(1);
    assert!(
        !TargetColumn::from_cached(&cached).required,
        "a key may be generated"
    );
}

#[test]
fn a_draft_table_quotes_by_the_driver_and_types_every_column_as_text() {
    let path = ObjectPath::default()
        .child(ObjectKind::Database, "shop")
        .child(ObjectKind::Schema, "sales");
    let columns: Vec<String> = ["id", "", "Name", "name", "say \"hi\""]
        .map(String::from)
        .into();
    let sql = draft::create_table(&PostgresDriver, &path, "orders 2024", &columns);
    assert!(sql.starts_with("-- "));
    assert!(
        sql.ends_with(concat!(
            "CREATE TABLE \"sales\".\"orders 2024\" (\n",
            "    \"id\" TEXT,\n",
            "    \"column_2\" TEXT,\n",
            "    \"Name\" TEXT,\n",
            "    \"name_2\" TEXT,\n",
            "    \"say \"\"hi\"\"\" TEXT\n",
            ");\n"
        )),
        "{sql}"
    );
    let mysql = ObjectPath::default().child(ObjectKind::Database, "shop");
    let sql = draft::create_table(&MySqlDriver, &mysql, "t`x", &["a".into()]);
    assert!(
        sql.contains("CREATE TABLE `shop`.`t``x` (\n    `a` TEXT\n);"),
        "{sql}"
    );
    assert_eq!(draft::table_name("  "), "imported");
    assert_eq!(draft::table_name("orders"), "orders");
    assert_eq!(draft::text_type(MySqlDriver.descriptor().dialect), "TEXT");
}

#[test]
fn batches_close_at_their_rows_or_their_bytes() {
    let mut batcher = Batcher::new(3, usize::MAX);
    assert!(batcher.push(1, vec![text("a")]).is_none());
    assert!(batcher.push(2, vec![text("b")]).is_none());
    let batch = batcher.push(3, vec![text("c")]).unwrap();
    assert_eq!(batch.rows.len(), 3);
    assert_eq!(batch.lines, [1, 2, 3]);
    assert!(batcher.push(4, vec![text("d")]).is_none());
    assert_eq!(batcher.finish().unwrap().lines, [4]);
    let mut batcher = Batcher::new(1000, 64);
    let big = vec![text(&"x".repeat(100))];
    assert_eq!(batcher.push(1, big).unwrap().rows.len(), 1);
    assert!(Batcher::default().finish().is_none());
}

fn numbers_csv(folder: &Folder, rows: usize) -> PathBuf {
    let mut body = String::from("n,label\n");
    for index in 0..rows {
        body.push_str(&format!("{index},row {index}\n"));
    }
    folder.file("numbers.csv", body.as_bytes())
}

fn plan(path: PathBuf) -> ImportPlan {
    let columns: Vec<String> = vec!["n".into(), "label".into()];
    ImportPlan {
        path,
        options: csv(CsvOptions::default()),
        mapping: Mapping::new(vec![column("n"), column("label")], &columns, MatchBy::Name),
    }
}

#[test]
fn the_reader_waits_for_the_writer_so_memory_stays_within_two_batches() {
    let folder = Folder::new();
    let total = BATCH_ROWS * 10;
    let plan = plan(numbers_csv(&folder, total));
    let (sender, mut receiver) = tokio::sync::mpsc::channel(CHANNEL_BATCHES);
    let (progress, watching) = tokio::sync::watch::channel(Progress::default());
    let reader = std::thread::spawn(move || {
        pipeline::read_batches(&plan, &sender, &progress, &CancellationToken::new())
    });
    // Until the channel is full, the reader runs ahead; then it waits.
    let mut waited = 0;
    while receiver.len() < CHANNEL_BATCHES && waited < 1000 {
        std::thread::sleep(Duration::from_millis(10));
        waited += 1;
    }
    let bound = ((CHANNEL_BATCHES + 1) * BATCH_ROWS) as u64;
    assert!(watching.borrow().rows_read <= bound);
    let mut received = 0;
    while let Some(batch) = receiver.blocking_recv() {
        received += batch.rows.len();
        // Never more than the channel, the batch waiting to be sent and the
        // one being filled, beyond what the writer took.
        assert!(watching.borrow().rows_read <= received as u64 + bound);
    }
    assert_eq!(reader.join().unwrap().unwrap(), total as u64);
    assert_eq!(received, total);
}

/// A writer that keeps what it is asked, and fails or stalls on request.
#[derive(Clone, Default)]
struct FakeWriter {
    calls: Arc<Mutex<Vec<String>>>,
    rows: Arc<Mutex<Vec<Vec<ImportValue>>>>,
    fail_write_at: Option<usize>,
    /// Waits for its token in every write.
    stall: bool,
    /// The rows it claims each write wrote, instead of the rows sent.
    claim: Option<u64>,
    commit_error: Option<ErrorCode>,
}

#[async_trait]
impl BatchWriter for FakeWriter {
    async fn begin(&mut self, _cancel: CancellationToken) -> DriverResult<()> {
        self.calls.lock().unwrap().push("begin".into());
        Ok(())
    }

    async fn write(&mut self, batch: &Batch, cancel: CancellationToken) -> DriverResult<u64> {
        let count = {
            let mut calls = self.calls.lock().unwrap();
            calls.push("write".into());
            calls.iter().filter(|call| *call == "write").count()
        };
        if self.stall {
            cancel.cancelled().await;
            return Err(DriverError::cancelled());
        }
        if self.fail_write_at == Some(count) {
            return Err(DriverError::new(
                ErrorCode::Database,
                format!("line {}: invalid input", batch.lines[0]),
            ));
        }
        self.rows.lock().unwrap().extend(batch.rows.iter().cloned());
        Ok(self.claim.unwrap_or(batch.rows.len() as u64))
    }

    async fn end(&mut self, commit: bool, _cancel: CancellationToken) -> DriverResult<()> {
        self.calls
            .lock()
            .unwrap()
            .push(if commit { "commit" } else { "rollback" }.into());
        match (commit, self.commit_error) {
            (true, Some(code)) => Err(DriverError::new(code, "commit failed")),
            _ => Ok(()),
        }
    }
}

impl FakeWriter {
    fn last_call(&self) -> String {
        self.calls
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

async fn run_with(writer: &mut FakeWriter, path: PathBuf, limit: Option<Duration>) -> Outcome {
    let (progress, _) = tokio::sync::watch::channel(Progress::default());
    pipeline::run(
        plan(path),
        writer,
        limit,
        CancellationToken::new(),
        progress,
    )
    .await
}

#[tokio::test]
async fn an_import_commits_once_every_row_is_written() {
    let folder = Folder::new();
    let path = numbers_csv(&folder, BATCH_ROWS * 2 + 5);
    let mut writer = FakeWriter::default();
    let outcome = run_with(&mut writer, path, None).await;
    assert_eq!(
        outcome,
        Outcome::Imported {
            rows: (BATCH_ROWS * 2 + 5) as u64
        }
    );
    assert_eq!(
        outcome.message("sales.t"),
        "Imported 2005 rows into sales.t."
    );
    let calls = writer.calls.lock().unwrap().clone();
    assert_eq!(calls, ["begin", "write", "write", "write", "commit"]);
    assert_eq!(writer.rows.lock().unwrap()[1], [text("1"), text("row 1")]);
}

#[tokio::test]
async fn a_failure_anywhere_rolls_everything_back() {
    let folder = Folder::new();
    // The database rejects a value in the second batch.
    let path = numbers_csv(&folder, BATCH_ROWS * 3);
    let mut writer = FakeWriter {
        fail_write_at: Some(2),
        ..FakeWriter::default()
    };
    let outcome = run_with(&mut writer, path.clone(), None).await;
    let Outcome::NothingImported { reason } = &outcome else {
        unreachable!("{outcome:?}");
    };
    assert_eq!(reason.code, ErrorCode::Database);
    assert!(reason.message.contains("line 1002"), "{}", reason.message);
    assert_eq!(writer.last_call(), "rollback");
    assert_eq!(
        outcome.message("t"),
        "Nothing was imported: t is as it was."
    );
    // The file is ragged after rows were written.
    let mut body = std::fs::read_to_string(&path).unwrap();
    body.push_str("1,2,3\n");
    let path = folder.file("ragged.csv", body.as_bytes());
    let mut writer = FakeWriter::default();
    let outcome = run_with(&mut writer, path.clone(), None).await;
    assert!(
        matches!(outcome, Outcome::NothingImported { .. }),
        "{outcome:?}"
    );
    assert_eq!(writer.last_call(), "rollback");
    // The driver writes fewer rows than it was sent.
    let mut writer = FakeWriter {
        claim: Some(1),
        ..FakeWriter::default()
    };
    let outcome = run_with(&mut writer, numbers_csv(&folder, 10), None).await;
    let Outcome::NothingImported { reason } = outcome else {
        unreachable!("rows written must be rows sent");
    };
    assert_eq!(reason.code, ErrorCode::Internal);
    assert_eq!(writer.last_call(), "rollback");
}

#[tokio::test]
async fn cancelling_or_the_time_limit_rolls_back() {
    let folder = Folder::new();
    let path = numbers_csv(&folder, 10);
    let mut writer = FakeWriter {
        stall: true,
        ..FakeWriter::default()
    };
    let outcome = run_with(&mut writer, path.clone(), Some(Duration::from_millis(50))).await;
    let Outcome::NothingImported { reason } = outcome else {
        unreachable!("the time limit stops it");
    };
    assert_eq!(reason.code, ErrorCode::Timeout);
    assert_eq!(writer.last_call(), "rollback");

    let mut writer = FakeWriter {
        stall: true,
        ..FakeWriter::default()
    };
    let cancel = CancellationToken::new();
    let (progress, _) = tokio::sync::watch::channel(Progress::default());
    let stop = cancel.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        stop.cancel();
    });
    let outcome = pipeline::run(plan(path), &mut writer, None, cancel, progress).await;
    canceller.await.unwrap();
    let Outcome::NothingImported { reason } = outcome else {
        unreachable!("Cancel stops it");
    };
    assert_eq!(reason.code, ErrorCode::Cancelled);
    assert_eq!(writer.last_call(), "rollback");
}

#[tokio::test]
async fn a_commit_whose_answer_is_lost_is_neither_success_nor_rollback() {
    let folder = Folder::new();
    let mut writer = FakeWriter {
        commit_error: Some(ErrorCode::Connection),
        ..FakeWriter::default()
    };
    let outcome = run_with(&mut writer, numbers_csv(&folder, 3), None).await;
    assert!(matches!(outcome, Outcome::Unknown { .. }), "{outcome:?}");
    assert!(outcome.message("t").contains("may or may not"));
    // A commit the database refuses rolled the transaction back.
    let mut writer = FakeWriter {
        commit_error: Some(ErrorCode::Database),
        ..FakeWriter::default()
    };
    let outcome = run_with(&mut writer, numbers_csv(&folder, 3), None).await;
    assert!(
        matches!(outcome, Outcome::NothingImported { .. }),
        "{outcome:?}"
    );
}
