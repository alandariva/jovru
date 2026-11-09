//! Whole imports into SQLite files, through the driver's own session: the
//! write path ADR-0040 adds, end to end, on dedicated synthetic files.
use super::mapping::{Mapping, MatchBy, TargetColumn};
use super::pipeline::{ImportPlan, Outcome, Progress};
use super::session::{ImportJob, run, target_columns};
use super::tests::Folder;
use super::*;
use crate::drivers::{
    ConnectionConfig, Credentials, DriverKind, ExecutionOptions, ObjectKind, ObjectPath,
    QueryRequest, ResolvedConnection, Row, RowSink, Session, SinkControl, builtin_registry,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn sqlite(path: &Path) -> ResolvedConnection {
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::Sqlite,
            options: serde_json::json!({ "path": path.to_string_lossy(), "readOnly": false }),
        },
        credentials: Credentials::default(),
    }
}

#[derive(Default)]
struct Rows(Vec<Row>);

impl RowSink for Rows {
    fn columns(&mut self, _: Vec<crate::drivers::ColumnInfo>) -> SinkControl {
        SinkControl::Continue
    }
    fn row(&mut self, row: Row) -> SinkControl {
        self.0.push(row);
        SinkControl::Continue
    }
}

async fn session(path: &Path) -> Box<dyn Session> {
    let driver = builtin_registry().unwrap().get(DriverKind::Sqlite).unwrap();
    driver
        .open_session(&sqlite(path), CancellationToken::new())
        .await
        .unwrap()
}

/// Runs `sql` on the file and returns its rows as text.
async fn query(path: &Path, sql: &str) -> Vec<Vec<Option<String>>> {
    let mut session = session(path).await;
    let mut rows = Rows::default();
    session
        .execute(
            &QueryRequest::new(sql, ExecutionOptions::default()),
            &mut rows,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    session.close().await.unwrap();
    rows.0
        .iter()
        .map(|row| row.iter().map(text).collect())
        .collect()
}

fn text(cell: &crate::drivers::Cell) -> Option<String> {
    use crate::drivers::Cell;
    match cell {
        Cell::Null => None,
        Cell::Boolean(flag) => Some(flag.to_string()),
        Cell::Integer(number) => Some(number.to_string()),
        Cell::UnsignedInteger(number) => Some(number.to_string()),
        Cell::Float(number) => Some(number.to_string()),
        Cell::Decimal(text) | Cell::Text(text) | Cell::Temporal(text) => Some(text.clone()),
        Cell::Binary(bytes) => Some(format!("{bytes:?}")),
    }
}

/// A database file with the table `people`: an integer key the database
/// fills, a required name, a number, a flag and a generated column.
async fn database(folder: &Folder) -> PathBuf {
    // SQLite reads an empty file as an empty database; the driver never
    // creates one.
    let path = folder.file("people.sqlite", b"");
    let mut session = session(&path).await;
    session
        .execute(
            &QueryRequest::new(
                "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, \
                 score NUMERIC, active INTEGER, upper_name TEXT GENERATED ALWAYS AS (upper(name)))",
                ExecutionOptions::default(),
            ),
            &mut Rows::default(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    session.close().await.unwrap();
    path
}

fn table() -> ObjectPath {
    ObjectPath::default()
        .child(ObjectKind::Schema, "main")
        .child(ObjectKind::Table, "people")
}

fn cached(names: &[&str]) -> Vec<TargetColumn> {
    names
        .iter()
        .map(|name| TargetColumn {
            name: (*name).into(),
            data_type: None,
            required: false,
            generated: false,
        })
        .collect()
}

fn job(path: &Path, file: PathBuf, columns: &[String]) -> ImportJob {
    let targets = cached(&["id", "name", "score", "active", "upper_name"]);
    let mut mapping = Mapping::new(targets, columns, MatchBy::Name);
    mapping.set(4, None);
    ImportJob {
        connection: sqlite(path),
        table: table(),
        plan: ImportPlan {
            path: file,
            options: ReadOptions::Csv(CsvOptions::default()),
            mapping,
        },
        limit: Some(Duration::from_secs(60)),
    }
}

fn header(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

async fn import(job: ImportJob) -> Outcome {
    let (progress, _) = tokio::sync::watch::channel(Progress::default());
    run(job, CancellationToken::new(), progress).await
}

#[tokio::test]
async fn a_csv_is_imported_into_a_sqlite_table_all_at_once() {
    let folder = Folder::new();
    let path = database(&folder).await;
    let mut body = String::from("name,score,active\n");
    for index in 0..2500 {
        body.push_str(&format!("person {index},{index}.5,\n"));
    }
    body.push_str("NULL,0.10,1\n");
    let file = folder.file("people.csv", body.as_bytes());
    let outcome = import(job(&path, file, &header(&["name", "score", "active"]))).await;
    assert_eq!(outcome, Outcome::Imported { rows: 2501 });
    let rows = query(
        &path,
        "SELECT count(*), sum(active IS NULL), max(id) FROM people",
    )
    .await;
    assert_eq!(
        rows,
        [vec![
            Some("2501".into()),
            Some("2500".into()),
            Some("2501".into())
        ]]
    );
    // The text NULL is text, and the generated column computed itself.
    let rows = query(
        &path,
        "SELECT name, score, active, upper_name FROM people WHERE id = 2501",
    )
    .await;
    assert_eq!(
        rows,
        [vec![
            Some("NULL".into()),
            Some("0.1".into()),
            Some("1".into()),
            Some("NULL".into())
        ]]
    );
}

#[tokio::test]
async fn a_rejected_row_leaves_the_table_as_it_was_and_names_its_line() {
    let folder = Folder::new();
    let path = database(&folder).await;
    let mut body = String::from("name,score,active\n");
    for index in 0..1500 {
        body.push_str(&format!("person {index},1,1\n"));
    }
    // An empty name is NULL by default, which NOT NULL rejects.
    body.push_str(",2,1\n");
    let file = folder.file("people.csv", body.as_bytes());
    let outcome = import(job(&path, file, &header(&["name", "score", "active"]))).await;
    let Outcome::NothingImported { reason } = &outcome else {
        unreachable!("{outcome:?}");
    };
    assert!(
        reason.message.contains("Line 1502 of the file"),
        "{}",
        reason.message
    );
    assert_eq!(
        query(&path, "SELECT count(*) FROM people").await,
        [vec![Some("0".into())]]
    );
    assert_eq!(
        outcome.message("main.people"),
        "Nothing was imported: main.people is as it was."
    );
}

#[tokio::test]
async fn the_driver_decides_what_is_required_and_generated() {
    let folder = Folder::new();
    let path = database(&folder).await;
    let file = folder.file("people.csv", b"score,upper_name\n1,X\n");
    // The cache said nothing; the driver says `name` must be filled and
    // `upper_name` is computed.
    let targets = cached(&["id", "name", "score", "active", "upper_name"]);
    let mapping = Mapping::new(targets, &header(&["score", "upper_name"]), MatchBy::Name);
    let outcome = import(ImportJob {
        connection: sqlite(&path),
        table: table(),
        plan: ImportPlan {
            path: file,
            options: ReadOptions::Csv(CsvOptions::default()),
            mapping,
        },
        limit: None,
    })
    .await;
    let Outcome::NothingImported { reason } = outcome else {
        unreachable!("the mapping is refused");
    };
    assert!(reason.message.contains("name"), "{}", reason.message);
    let mut driver_session = session(&path).await;
    let target = driver_session
        .table_target(&table(), CancellationToken::new())
        .await
        .unwrap()
        .editable()
        .unwrap();
    driver_session.close().await.unwrap();
    assert!(target.key.is_empty(), "an import's target has no key");
    let columns = target_columns(&target);
    assert_eq!(
        columns
            .iter()
            .map(|column| (column.name.as_str(), column.required, column.generated))
            .collect::<Vec<_>>(),
        [
            ("id", false, false),
            ("name", true, false),
            ("score", false, false),
            ("active", false, false),
            ("upper_name", false, true),
        ]
    );
    // A table that is not there is said in words.
    let mut missing = job(
        &path,
        folder.file("x.csv", b"name\nA\n"),
        &header(&["name"]),
    );
    missing.table = ObjectPath::default()
        .child(ObjectKind::Schema, "main")
        .child(ObjectKind::Table, "nobody");
    let Outcome::NothingImported { reason } = import(missing).await else {
        unreachable!("no such table");
    };
    assert!(reason.message.contains("nobody"), "{}", reason.message);
}

#[tokio::test]
async fn xlsx_booleans_and_dates_reach_sqlite_as_its_values() {
    let folder = Folder::new();
    let path = database(&folder).await;
    let file = folder.0.join("people.xlsx");
    let mut book = rust_xlsxwriter::Workbook::new();
    let sheet = book.add_worksheet();
    sheet.write_string(0, 0, "name").unwrap();
    sheet.write_string(0, 1, "active").unwrap();
    sheet.write_string(0, 2, "score").unwrap();
    sheet.write_string(1, 0, "Ana").unwrap();
    sheet.write_boolean(1, 1, true).unwrap();
    let date = rust_xlsxwriter::Format::new().set_num_format("yyyy-mm-dd");
    sheet
        .write_number_with_format(1, 2, 45658.0, &date)
        .unwrap();
    sheet.write_string(2, 0, "Bo").unwrap();
    sheet.write_boolean(2, 1, false).unwrap();
    sheet.write_number(2, 2, 0.1).unwrap();
    book.save(&file).unwrap();
    let mut job = job(&path, file, &header(&["name", "active", "score"]));
    job.plan.options = ReadOptions::Xlsx(XlsxOptions::new("Sheet1"));
    assert_eq!(import(job).await, Outcome::Imported { rows: 2 });
    assert_eq!(
        query(&path, "SELECT name, active, score FROM people ORDER BY id").await,
        [
            vec![
                Some("Ana".into()),
                Some("1".into()),
                Some("2025-01-01 00:00:00".into())
            ],
            vec![Some("Bo".into()), Some("0".into()), Some("0.1".into())],
        ]
    );
}

#[tokio::test]
async fn cancelling_rolls_the_import_back() {
    let folder = Folder::new();
    let path = database(&folder).await;
    let mut body = String::from("name\n");
    for index in 0..20_000 {
        body.push_str(&format!("person {index}\n"));
    }
    let file = folder.file("many.csv", body.as_bytes());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (progress, _) = tokio::sync::watch::channel(Progress::default());
    let outcome = run(job(&path, file, &header(&["name"])), cancel, progress).await;
    let Outcome::NothingImported { reason } = outcome else {
        unreachable!("cancelled");
    };
    assert_eq!(reason.code, crate::drivers::ErrorCode::Cancelled);
    assert_eq!(
        query(&path, "SELECT count(*) FROM people").await,
        [vec![Some("0".into())]]
    );
}
