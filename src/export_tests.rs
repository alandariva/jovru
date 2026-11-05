use super::*;
use crate::cell_format::result_csv;
use crate::copy_as::{Block, CopyFormat, CopyOptions, SqlStyle, copy_as};
use crate::grid::CellSelection;
use std::time::{Duration, Instant};

fn columns() -> Vec<ResultColumn> {
    vec![
        ResultColumn::new("id", "numeric(38,10)"),
        ResultColumn::new("name", "text"),
        ResultColumn::new("active", "bool"),
        ResultColumn::new("id", "int8"),
    ]
}

/// NULL, empty text, whitespace, the text 'NULL', separators, quotes, line
/// breaks and an exact decimal that no float holds.
fn data() -> ResultData {
    ResultData::from_rows(
        columns(),
        [
            [
                Some("12345678901234567890.0123456789"),
                Some("O'Brien; \"Bob\""),
                Some("t"),
                Some("1"),
            ],
            [Some("2"), None, Some("f"), Some("2")],
            [Some("-0.50"), Some(""), None, Some("3")],
            [Some("3"), Some("NULL"), Some("1"), None],
            [Some("4"), Some("a|b\r\nc,d"), Some("x"), Some("5")],
            [Some("5"), Some("   "), Some("0"), Some("6")],
        ],
    )
}

fn options(format: ExportFormat) -> ExportOptions {
    ExportOptions {
        format,
        ..ExportOptions::default()
    }
}

fn sql() -> SqlTarget {
    SqlTarget {
        dialect: Some(SqlDialect::Postgres),
        table: "public.people".into(),
        columns: vec!["id".into(), "name".into(), "active".into(), "\"id\"".into()],
    }
}

fn encode(options: &ExportOptions, data: &ResultData) -> String {
    let mut encoder = Encoder::new(options, data.columns(), &sql()).unwrap();
    let mut out = String::new();
    encoder.begin(&mut out);
    encoder.rows(data, &mut out);
    encoder.end(&mut out);
    out
}

/// The whole result as **Copy as** sees a selection of everything.
fn copied(format: CopyFormat, data: &ResultData) -> String {
    let families: Vec<_> = data
        .columns()
        .iter()
        .map(|column| family(&column.data_type))
        .collect();
    let visible: Vec<usize> = (0..data.row_count()).collect();
    let block = Block::new(
        data,
        &visible,
        CellSelection::all(&visible, data.column_count()),
        &families,
    )
    .unwrap();
    let quote = |name: &str| name.to_owned();
    let style = SqlStyle {
        dialect: Some(SqlDialect::Postgres),
        quote_name: &quote,
        table: "public.people",
    };
    copy_as(&CopyOptions::default().with(format, true), &block, &style)
}

#[test]
fn the_default_csv_is_the_csv_copy_with_a_final_line_break() {
    let data = data();
    assert_eq!(
        encode(&ExportOptions::default(), &data),
        format!("{}\n", result_csv(&data))
    );
}

#[test]
fn csv_quotes_only_what_needs_it_and_keeps_null_apart_from_empty_text() {
    let data = data();
    let text = encode(
        &ExportOptions {
            separator: Separator::Comma,
            quoting: Quoting::Needed,
            ..ExportOptions::default()
        },
        &data,
    );
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(lines[0], "id,name,active,id");
    assert_eq!(
        lines[1],
        "12345678901234567890.0123456789,\"O'Brien; \"\"Bob\"\"\",t,1"
    );
    // NULL is an empty field; empty text is "".
    assert_eq!(lines[2], "2,,f,2");
    assert_eq!(lines[3], "-0.50,\"\",,3");
    assert_eq!(lines[4], "3,NULL,1,");
    // A line break inside a value stays inside its quotes.
    assert_eq!(lines[5], "4,\"a|b\r");
    assert_eq!(lines[6], "c,d\",x,5");
    assert_eq!(lines[7], "5,   ,0,6");
    assert_eq!(lines[8], "");
    assert_eq!(lines.len(), 9);

    let tab = encode(
        &ExportOptions {
            separator: Separator::Tab,
            quoting: Quoting::Needed,
            header: false,
            ..ExportOptions::default()
        },
        &ResultData::from_rows(columns(), [[Some("a\tb"), Some("c;d"), None, Some("")]]),
    );
    assert_eq!(tab, "\"a\tb\"\tc;d\t\t\"\"\n");
}

#[test]
fn json_is_the_json_copy_and_json_lines_one_object_per_line() {
    let data = data();
    assert_eq!(
        encode(&options(ExportFormat::Json), &data),
        format!("{}\n", copied(CopyFormat::Json, &data))
    );
    let empty = ResultData::new(columns());
    assert_eq!(encode(&options(ExportFormat::Json), &empty), "[]\n");
    let lines = encode(&options(ExportFormat::JsonLines), &data);
    let first = lines.lines().next().unwrap();
    // The exact decimal is a JSON number as written; the repeated name gets
    // a suffix.
    assert_eq!(
        first,
        "{\"id\": 12345678901234567890.0123456789, \"name\": \"O'Brien; \\\"Bob\\\"\", \"active\": true, \"id_2\": 1}"
    );
    assert_eq!(lines.lines().count(), data.row_count());
    // Every line is a JSON value on its own.
    for line in lines.lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    assert_eq!(encode(&options(ExportFormat::JsonLines), &empty), "");
}

#[test]
fn inserts_and_markdown_are_what_copy_as_writes() {
    let data = data();
    let inserts = encode(&options(ExportFormat::Insert), &data);
    // The export quotes the repeated column as the caller did.
    assert_eq!(
        inserts,
        format!("{}\n", copied(CopyFormat::Insert, &data))
            .replace("(id, name, active, id)", "(id, name, active, \"id\")")
    );
    assert!(inserts.contains("'a|b\r\nc,d'"));
    assert_eq!(
        encode(&options(ExportFormat::Markdown), &data),
        format!("{}\n", copied(CopyFormat::Markdown, &data))
    );
}

#[test]
fn csv_and_json_lines_copies_are_what_the_export_writes() {
    let data = data();
    // The default CSV copy with column names is the default CSV export.
    assert_eq!(
        format!("{}\n", copied(CopyFormat::Csv, &data)),
        encode(&ExportOptions::default(), &data)
    );
    assert_eq!(
        format!("{}\n", copied(CopyFormat::JsonLines, &data)),
        encode(&options(ExportFormat::JsonLines), &data)
    );
    // And so is one with the export's other options.
    let families: Vec<_> = data
        .columns()
        .iter()
        .map(|column| family(&column.data_type))
        .collect();
    let visible: Vec<usize> = (0..data.row_count()).collect();
    let block = Block::new(
        &data,
        &visible,
        CellSelection::all(&visible, data.column_count()),
        &families,
    )
    .unwrap();
    let quote = |name: &str| name.to_owned();
    let style = SqlStyle {
        dialect: None,
        quote_name: &quote,
        table: "t",
    };
    let copy = CopyOptions {
        format: CopyFormat::Csv,
        separator: Separator::Tab,
        quoting: Quoting::Needed,
        ..CopyOptions::default()
    };
    let export = ExportOptions {
        separator: Separator::Tab,
        quoting: Quoting::Needed,
        header: false,
        ..ExportOptions::default()
    };
    assert_eq!(
        format!("{}\n", copy_as(&copy, &block, &style)),
        encode(&export, &data)
    );
}

#[test]
fn excel_gets_a_number_only_when_no_digit_can_change() {
    assert_eq!(xlsx_number("1.50"), Some(1.5));
    assert_eq!(xlsx_number("-0.000123"), Some(-0.000123));
    assert_eq!(xlsx_number("123456789012345"), Some(123_456_789_012_345.0));
    assert_eq!(xlsx_number("1000000000000000000000"), Some(1e21));
    assert_eq!(xlsx_number("1.5e10"), Some(1.5e10));
    // 16 significant digits: Excel would round it.
    assert_eq!(xlsx_number("9007199254740993"), None);
    assert_eq!(xlsx_number("0.1234567890123456"), None);
    assert_eq!(xlsx_number("12345678901234567890.0123456789"), None);
    for text in [
        "", ".", "1.2.3", "NaN", "Infinity", "1e", "1e400", "0x10", "1,5",
    ] {
        assert_eq!(xlsx_number(text), None, "{text}");
    }
    assert_eq!(boolean("TRUE"), Some(true));
    assert_eq!(boolean("0"), Some(false));
    assert_eq!(boolean("yes"), None);
}

#[test]
fn file_names_never_become_paths_or_reserved_names() {
    use ExportFormat as F;
    assert_eq!(suggested_file_name("orders", F::Csv), "orders.csv");
    assert_eq!(suggested_file_name("a/b:c\\d*?", F::Xlsx), "a_b_c_d__.xlsx");
    assert_eq!(suggested_file_name("../../etc", F::Json), "_.._etc.json");
    assert_eq!(suggested_file_name("CON", F::Csv), "_CON.csv");
    assert_eq!(
        suggested_file_name("com1.backup", F::Csv),
        "_com1.backup.csv"
    );
    assert_eq!(suggested_file_name("console", F::Csv), "console.csv");
    assert_eq!(suggested_file_name(" ...", F::Csv), "result.csv");
    assert_eq!(suggested_file_name("x. ", F::JsonLines), "x.jsonl");
    assert_eq!(
        suggested_file_name("line\nbreak", F::Insert),
        "line_break.sql"
    );
    assert_eq!(
        suggested_file_name("pedidos_ç", F::Markdown),
        "pedidos_ç.md"
    );
    let long = "n".repeat(300);
    assert_eq!(suggested_file_name(&long, F::Csv).chars().count(), 104);
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jykre-export-{name}-{}",
        crate::drivers::ConnectionId::new()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Only these names are in `dir`: no temporary file was left behind.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_file_replaces_the_destination_only_when_it_is_finished() {
    let dir = scratch("finish");
    let target = dir.join("people.csv");
    std::fs::write(&target, "previous export").unwrap();
    let data = data();
    let mut file =
        ExportFile::create(&target, &ExportOptions::default(), data.columns(), &sql()).unwrap();
    file.write(&data).unwrap();
    file.write(&data).unwrap();
    // Until it is finished, the destination keeps what it had.
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "previous export");
    assert_eq!(names(&dir).len(), 2);
    let written = file.finish().unwrap();
    assert_eq!(written.rows, 12);
    let text = std::fs::read_to_string(&target).unwrap();
    assert_eq!(written.bytes, text.len() as u64);
    let csv = result_csv(&data);
    let rows = csv.split_once('\n').unwrap().1;
    assert_eq!(text, format!("{csv}\n{rows}\n"));
    assert_eq!(names(&dir), ["people.csv"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_abandoned_file_leaves_nothing_and_the_destination_untouched() {
    let dir = scratch("abandon");
    let target = dir.join("people.json");
    std::fs::write(&target, "previous export").unwrap();
    let data = data();
    let mut file = ExportFile::create(
        &target,
        &options(ExportFormat::Json),
        data.columns(),
        &sql(),
    )
    .unwrap();
    file.write(&data).unwrap();
    drop(file);
    assert_eq!(names(&dir), ["people.json"]);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "previous export");

    // An Excel file too, with its own temporary files.
    let target = dir.join("people.xlsx");
    let mut file = ExportFile::create(
        &target,
        &options(ExportFormat::Xlsx),
        data.columns(),
        &sql(),
    )
    .unwrap();
    file.write(&data).unwrap();
    drop(file);
    assert_eq!(names(&dir), ["people.json"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_missing_folder_fails_before_anything_is_read() {
    let dir = scratch("missing");
    let target = dir.join("not-there").join("people.csv");
    let error =
        ExportFile::create(&target, &ExportOptions::default(), &columns(), &sql()).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    // The message names no folder of the user's.
    assert!(!error.message.contains("not-there"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_bom_comes_first_only_in_csv() {
    let dir = scratch("bom");
    let data = data();
    for (format, bom) in [(ExportFormat::Csv, true), (ExportFormat::Json, false)] {
        let target = dir.join(format!("people.{}", format.extension()));
        let options = ExportOptions {
            format,
            bom: true,
            ..ExportOptions::default()
        };
        let mut file = ExportFile::create(&target, &options, data.columns(), &sql()).unwrap();
        file.write(&data).unwrap();
        file.finish().unwrap();
        let bytes = std::fs::read(&target).unwrap();
        assert_eq!(bytes.starts_with("\u{feff}".as_bytes()), bom, "{format:?}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_excel_file_is_a_workbook_within_excel_limits() {
    let dir = scratch("xlsx");
    let target = dir.join("people.xlsx");
    let data = data();
    let mut file = ExportFile::create(
        &target,
        &options(ExportFormat::Xlsx),
        data.columns(),
        &sql(),
    )
    .unwrap();
    file.write(&data).unwrap();
    let written = file.finish().unwrap();
    assert_eq!(written.rows, 6);
    let bytes = std::fs::read(&target).unwrap();
    // A zip archive, as every XLSX is.
    assert!(bytes.starts_with(b"PK\x03\x04"));
    assert_eq!(written.bytes, bytes.len() as u64);
    assert_eq!(names(&dir), ["people.xlsx"]);

    // A value longer than a cell holds fails the export instead of being cut.
    let long = "x".repeat(32_768);
    let too_long = ResultData::from_rows(
        vec![ResultColumn::new("note", "text")],
        [[Some(long.as_str())]],
    );
    let mut file = ExportFile::create(
        &dir.join("long.xlsx"),
        &options(ExportFormat::Xlsx),
        too_long.columns(),
        &sql(),
    )
    .unwrap();
    let error = file.write(&too_long).unwrap_err();
    assert_eq!(error.code, ErrorCode::Validation);
    drop(file);

    // Rows past the last one of a sheet fail before any is written.
    let mut sheet = Sheet::new(&dir, data.columns(), true).unwrap();
    sheet.next_row = XLSX_MAX_ROWS - 5;
    assert_eq!(sheet.rows(&data).unwrap_err().code, ErrorCode::Validation);
    drop(sheet);
    assert_eq!(names(&dir), ["people.xlsx"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn preferences_round_trip_and_unknown_values_take_their_default() {
    let preferences = ExportPreferences {
        options: ExportOptions {
            format: ExportFormat::JsonLines,
            separator: Separator::Tab,
            quoting: Quoting::Needed,
            header: false,
            bom: true,
        },
        directory: Some(PathBuf::from("exports")),
    };
    assert_eq!(
        ExportPreferences::from_value(Some(&preferences.to_value())),
        preferences
    );
    assert_eq!(
        ExportPreferences::from_value(Some(&serde_json::json!({
            "format": "parquet",
            "separator": "|",
            "quoting": 3,
            "directory": "",
        }))),
        ExportPreferences::default()
    );
    assert_eq!(
        ExportPreferences::from_value(None),
        ExportPreferences::default()
    );
}

#[tokio::test]
async fn preferences_are_kept_in_the_configuration() {
    let dir = scratch("preferences");
    assert_eq!(
        load_preferences(&dir).await.unwrap(),
        ExportPreferences::default()
    );
    let preferences = ExportPreferences {
        options: options(ExportFormat::Xlsx),
        directory: Some(dir.clone()),
    };
    let older = PREFERENCE_WRITES.ask();
    let newer = PREFERENCE_WRITES.ask();
    // A write superseded by a newer one is skipped.
    save_preferences(&dir, ExportPreferences::default(), older)
        .await
        .unwrap();
    save_preferences(&dir, preferences.clone(), newer)
        .await
        .unwrap();
    assert_eq!(load_preferences(&dir).await.unwrap(), preferences);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_large_result_encodes_within_budget() {
    let columns: Vec<ResultColumn> = (0..10)
        .map(|index| ResultColumn::new(format!("c{index}"), "text"))
        .collect();
    let value = "synthetic value, \"quoted\"";
    let data = ResultData::from_rows(
        columns,
        (0..100_000).map(|_| std::iter::repeat_n(Some(value), 10)),
    );
    let started = Instant::now();
    let text = encode(&ExportOptions::default(), &data);
    // About 90 ms in a debug build; the budget catches complexity
    // regressions, not small slowdowns.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(text.lines().count(), 100_001);
}
