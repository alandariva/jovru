//! Synthetic data for the performance run: a large SQL text and the lines
//! to jump to, a result with thousands of named columns, an editable
//! table's description, CSV and `.xlsx` files to import, SQLite databases
//! and a long automation history. Everything is generated; nothing comes
//! from the user's files, and nothing here is logged.
use crate::automation::definition::{JobDefinition, canonical_json};
use crate::automation::{Cause, JobKind, RunId, RunOrigin, RunState, UtcMillis};
use crate::drivers::write::{EditableColumn, EditableTable, KeyKind, WriteMode};
use crate::drivers::{
    CancellationToken, ColumnInfo, ConnectionConfig, ConnectionId, Credentials, DriverKind,
    DriverResult, ExecutionOptions, ObjectKind, ObjectPath, QueryRequest, ResolvedConnection, Row,
    RowSink, SinkControl, builtin_registry,
};
use crate::import::mapping::{Mapping, MatchBy, TargetColumn};
use crate::result::{ResultColumn, ResultData};
use jykre_storage::{AutomationRepository, NewRun, RunFinish};
use serde_json::json;
use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

// ----- A9: a large SQL text and where to jump in it -----

/// Numbered one-line `SELECT`s, about 82 bytes a line (the editor's earlier
/// measurements used about 89), until the text holds at least `min_bytes`.
pub fn sql_script(min_bytes: usize) -> String {
    let mut text = String::with_capacity(min_bytes + 128);
    let mut line = 1usize;
    while text.len() < min_bytes {
        text.push_str(&format!(
            "SELECT {line} AS line_number, 'synthetic row {line:07}' AS label FROM sales.event_log;\n"
        ));
        line += 1;
    }
    text
}

/// What is typed into **Go to line**, `count` times: the first line, the
/// last, lines spread over the text (some with a column), and lines past
/// its end, which go to the last one.
pub fn jump_targets(lines: usize, count: usize) -> Vec<String> {
    let lines = lines.max(1);
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        // xorshift64: the same targets on every run.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut targets = vec![
        "1".to_owned(),
        lines.to_string(),
        (lines + 1).to_string(),
        "99999999".to_owned(),
    ];
    while targets.len() < count {
        let pick = next();
        let line = 1 + (pick % lines as u64) as usize;
        targets.push(match targets.len() % 5 {
            0 => format!("{line}:{}", 1 + pick % 40),
            4 => (lines + 1 + (pick % 1000) as usize).to_string(),
            _ => line.to_string(),
        });
    }
    targets.truncate(count);
    targets
}

// ----- A15: thousands of named columns -----

/// Words the column names are made of, some with accents, so a search
/// ignores case and accents as it does in the application.
const WORDS: [&str; 10] = [
    "id",
    "nome",
    "preço",
    "descrição",
    "quantidade",
    "data_de_criação",
    "situação",
    "cliente",
    "região",
    "observação",
];

/// The name of column `index` of [`named_fixture`].
pub fn column_name(index: usize) -> String {
    format!("{}_{index:04}", WORDS[index % WORDS.len()])
}

/// A result `columns` wide whose columns are named by [`column_name`].
pub fn named_fixture(rows: usize, columns: usize) -> ResultData {
    let names: Vec<ResultColumn> = (0..columns)
        .map(|column| ResultColumn::new(column_name(column), "text"))
        .collect();
    let values: Vec<Vec<String>> = (0..rows)
        .map(|row| {
            (0..columns)
                .map(|column| format!("{row}-{column}"))
                .collect()
        })
        .collect();
    ResultData::from_rows(
        names,
        values
            .iter()
            .map(|row| row.iter().map(|value| Some(value.as_str()))),
    )
}

// ----- A10: an editable result -----

/// What a driver would say of a table the result's columns all come from:
/// every column writable and nullable, the first one the primary key.
pub fn editable_target(columns: &[ResultColumn]) -> EditableTable {
    EditableTable {
        table: ObjectPath::default()
            .child(ObjectKind::Schema, "main")
            .child(ObjectKind::Table, "synthetic"),
        writes: WriteMode::Direct,
        columns: columns
            .iter()
            .map(|column| {
                Some(EditableColumn {
                    name: column.name.clone(),
                    declared_type: column.data_type.clone(),
                    nullable: true,
                    writable: true,
                    has_default: false,
                    binary: false,
                    can_key: true,
                })
            })
            .collect(),
        key: vec![0],
        key_kind: KeyKind::PrimaryKey,
        side_effects: Vec::new(),
    }
}

// ----- A11: files to import -----

/// The table both files are imported into.
pub const IMPORT_TABLE: &str = "imported";
pub const IMPORT_TABLE_SQL: &str = "CREATE TABLE imported (id INTEGER, name TEXT NOT NULL, \
amount NUMERIC, active INTEGER, created_at TEXT, region TEXT)";
/// The CSV's columns, in its header.
pub const CSV_COLUMNS: [&str; 5] = ["id", "name", "amount", "active", "created_at"];
/// The workbook's columns, in its header row.
pub const XLSX_COLUMNS: [&str; 5] = ["id", "name", "amount", "active", "region"];
const REGIONS: [&str; 4] = ["north", "south", "east", "west"];

/// The imported table as a structural path.
pub fn import_table() -> ObjectPath {
    ObjectPath::default()
        .child(ObjectKind::Schema, "main")
        .child(ObjectKind::Table, IMPORT_TABLE)
}

/// The table's columns as the metadata cache would list them, matched to
/// `file_columns` by name; the driver checks them again when the import
/// starts.
pub fn import_mapping(file_columns: &[&str]) -> Mapping {
    let targets = ["id", "name", "amount", "active", "created_at", "region"]
        .iter()
        .map(|name| TargetColumn {
            name: (*name).into(),
            data_type: None,
            required: false,
            generated: false,
        })
        .collect();
    let columns: Vec<String> = file_columns.iter().map(|name| (*name).to_owned()).collect();
    Mapping::new(targets, &columns, MatchBy::Name)
}

/// Writes a CSV of `rows` data rows with [`CSV_COLUMNS`]; returns its size.
pub fn write_csv(path: &Path, rows: usize) -> std::io::Result<u64> {
    let mut file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    writeln!(file, "{}", CSV_COLUMNS.join(","))?;
    for n in 1..=rows {
        writeln!(
            file,
            "{n},Customer {n:07} of Sao Paulo,{}.{:02},{},2026-01-{:02}",
            n % 100_000,
            n % 100,
            n % 2,
            1 + n % 28
        )?;
    }
    file.flush()?;
    drop(file);
    Ok(std::fs::metadata(path)?.len())
}

/// Writes an `.xlsx` of `rows` data rows with [`XLSX_COLUMNS`], whose names
/// are all different, so its shared strings table grows with the rows.
/// The writer's own temporary files go into `scratch`. Returns its size.
pub fn write_xlsx(path: &Path, scratch: &Path, rows: usize) -> Result<u64, String> {
    let error = |error: rust_xlsxwriter::XlsxError| error.to_string();
    let mut book = rust_xlsxwriter::Workbook::new();
    book.set_tempdir(scratch).map_err(error)?;
    let sheet = book.add_worksheet_with_low_memory().map_err(error)?;
    for (column, name) in XLSX_COLUMNS.iter().enumerate() {
        sheet
            .write_string(0, u16::try_from(column).unwrap_or(0), *name)
            .map_err(error)?;
    }
    for n in 1..=rows {
        let row = u32::try_from(n).map_err(|_| "Too many rows for a sheet".to_owned())?;
        sheet.write_number(row, 0, n as f64).map_err(error)?;
        sheet
            .write_string(
                row,
                1,
                format!(
                    "Customer {n:07} of São Paulo, account {:06}",
                    (n * 7_919) % 1_000_003
                ),
            )
            .map_err(error)?;
        sheet
            .write_number(row, 2, (n % 100_000) as f64 + 0.25)
            .map_err(error)?;
        sheet.write_boolean(row, 3, n % 2 == 0).map_err(error)?;
        sheet
            .write_string(row, 4, REGIONS[n % REGIONS.len()])
            .map_err(error)?;
    }
    book.save(path).map_err(error)?;
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| error.to_string())
}

// ----- SQLite databases -----

/// A writable connection to the SQLite file at `path`.
pub fn sqlite(path: &Path) -> ResolvedConnection {
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::Sqlite,
            options: json!({ "path": path.to_string_lossy(), "readOnly": false }),
        },
        credentials: Credentials::default(),
    }
}

/// Keeps the first column of the first row, as text.
#[derive(Default)]
struct FirstValue(Option<String>);

impl RowSink for FirstValue {
    fn columns(&mut self, _: Vec<ColumnInfo>) -> SinkControl {
        SinkControl::Continue
    }

    fn row(&mut self, row: Row) -> SinkControl {
        use crate::drivers::Cell;
        if self.0.is_none() {
            self.0 = row.first().and_then(|cell| match cell {
                Cell::Integer(number) => Some(number.to_string()),
                Cell::UnsignedInteger(number) => Some(number.to_string()),
                Cell::Decimal(text) | Cell::Text(text) => Some(text.clone()),
                _ => None,
            });
        }
        SinkControl::Continue
    }
}

/// Runs `statements` in order on the SQLite file at `path` (made empty
/// first when it does not exist: the driver never creates one) and returns
/// the first value the last one read. Must run inside the runtime.
pub async fn sqlite_run(path: &Path, statements: &[&str]) -> DriverResult<Option<String>> {
    if !path.exists() {
        std::fs::write(path, b"").map_err(|_| {
            crate::drivers::DriverError::new(
                crate::drivers::ErrorCode::Internal,
                "Could not create a synthetic database file.",
            )
        })?;
    }
    let driver = builtin_registry()?.get(DriverKind::Sqlite)?;
    let mut session = driver
        .open_session(&sqlite(path), CancellationToken::new())
        .await?;
    let mut last = None;
    for statement in statements {
        let mut sink = FirstValue::default();
        let executed = session
            .execute(
                &QueryRequest::new(*statement, ExecutionOptions::default()),
                &mut sink,
                CancellationToken::new(),
            )
            .await;
        if let Err(error) = executed {
            let _ = session.close().await;
            return Err(error);
        }
        last = sink.0;
    }
    session.close().await?;
    Ok(last)
}

/// The rows of `table` in the SQLite file at `path`.
pub async fn sqlite_count(path: &Path, table: &str) -> DriverResult<u64> {
    let sql = format!("SELECT count(*) FROM {table}");
    let value = sqlite_run(path, &[&sql]).await?;
    Ok(value.and_then(|text| text.parse().ok()).unwrap_or(0))
}

// ----- A6: jobs and a long history -----

/// The table the synthetic jobs read.
pub const JOBS_TABLE_SQL: &str = "CREATE TABLE big (id INTEGER PRIMARY KEY, name TEXT, \
amount NUMERIC, active INTEGER, created_at TEXT)";

/// Fills `big` with `rows` rows in one statement.
pub fn jobs_rows_sql(rows: usize) -> String {
    format!(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {rows}) \
         INSERT INTO big (id, name, amount, active, created_at) \
         SELECT i, 'customer ' || i, i || '.25', i % 2, '2026-01-01' FROM n"
    )
}

/// A SQL Job named `name` reading `sql` on `connection`, keeping up to
/// `max_rows` rows.
pub fn sql_job(name: &str, connection: ConnectionId, sql: &str, max_rows: u32) -> JobDefinition {
    let mut definition = JobDefinition::new_sql(name, Some(connection));
    if let crate::automation::definition::JobBody::Sql(body) = &mut definition.body {
        body.sql = sql.to_owned();
        body.max_rows = max_rows;
    }
    definition
}

/// How the `index`-th synthetic run ended: mostly succeeded, some failed
/// (with a cause, as storage requires) or cancelled.
pub fn synthetic_finish(index: usize, finished_at: UtcMillis) -> RunFinish {
    match index % 10 {
        0 => RunFinish {
            state: RunState::Failed,
            cause: Some(if index.is_multiple_of(20) {
                Cause::Database
            } else {
                Cause::Timeout
            }),
            effect_unknown: !index.is_multiple_of(20),
            failure_code: Some("synthetic".into()),
            finished_at,
            ..RunFinish::default()
        },
        7 => RunFinish {
            state: RunState::Cancelled,
            finished_at,
            ..RunFinish::default()
        },
        _ => RunFinish {
            state: RunState::Succeeded,
            rows_delivered: Some(i64::try_from(index % 1000).unwrap_or(0)),
            rows_discarded: Some(0),
            finished_at,
            ..RunFinish::default()
        },
    }
}

/// Writes `runs` finished runs of `job` on `connection` through the
/// production repository, newest at `now`, a minute apart (well within the
/// 90 days runs are kept).
pub async fn seed_history(
    repository: &AutomationRepository,
    job: &JobDefinition,
    revision: i64,
    connection: ConnectionId,
    runs: usize,
    now: UtcMillis,
) -> DriverResult<()> {
    let snapshot = canonical_json(&json!({
        "kind": "sql",
        "definition": job.to_value(),
        "connection": {"id": connection.to_string(), "revision": 1},
        "effective": {"timeout_ms": 300_000, "max_rows": 1000},
        "reads_only": true,
    }));
    for index in 0..runs {
        let started_at = now.minus(Duration::from_secs(60 * (runs - index) as u64));
        let id = RunId::new();
        repository
            .admit_run(&NewRun {
                id,
                kind: JobKind::Sql,
                job_id: Some(job.id),
                job_revision: Some(revision),
                job_version: None,
                chain_run_id: None,
                step_index: None,
                origin: RunOrigin::Manual,
                reads_only: true,
                snapshot_json: snapshot.clone(),
                connection_id: Some(connection),
                connection_revision: Some(1),
                program_fingerprint: None,
                scheduled: None,
                started_at,
            })
            .await?;
        let finished = started_at.plus(Duration::from_millis(40 + (index % 900) as u64));
        repository
            .finish_run(id, &synthetic_finish(index, finished))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid_edit::{EditSession, EditValue};

    #[test]
    fn the_sql_text_is_as_large_as_asked_in_one_line_statements() {
        let text = sql_script(64 * 1024);
        assert!(text.len() >= 64 * 1024);
        let lines = text.lines().count();
        // About 80–90 bytes a line.
        assert!((700..=850).contains(&lines), "{lines}");
        assert!(
            text.lines()
                .all(|line| line.starts_with("SELECT ") && line.ends_with(';'))
        );
        assert!(text.is_ascii());
    }

    #[test]
    fn jumps_reach_the_ends_and_past_the_text_and_all_parse() {
        let targets = jump_targets(12_000, 60);
        assert_eq!(targets.len(), 60);
        assert_eq!(targets, jump_targets(12_000, 60), "the same on every run");
        let parsed: Vec<_> = targets
            .iter()
            .map(|text| crate::go_to_line::parse(text).expect("a target"))
            .collect();
        let past = parsed
            .iter()
            .filter(|target| crate::go_to_line::place(**target, 12_000).past_end)
            .count();
        assert!(past >= 5, "{past}");
        assert!(parsed.iter().any(|target| target.column.is_some()));
        assert!(parsed.iter().any(|target| target.line == 1));
    }

    #[test]
    fn named_columns_are_found_ignoring_accents() {
        let data = named_fixture(3, 10_000);
        assert_eq!((data.row_count(), data.column_count()), (3, 10_000));
        assert_eq!(data.columns()[7_776].name, "situação_7776");
        let layout = crate::column_layout::ColumnLayout::new(10_000);
        let found = crate::column_search::find(data.columns(), &layout, "situacao_9");
        assert_eq!(found.len(), 100);
        let one = crate::column_search::find(data.columns(), &layout, "descricao_7773");
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn the_editable_target_takes_values_nulls_inserts_and_deletes() {
        let data = crate::performance::fixture(10);
        let mut session = EditSession::new(
            editable_target(data.columns()),
            vec![Some(false); data.column_count()],
        );
        let read = |column: usize| data.value(2, column).map(str::to_owned);
        session
            .set(2, &read, 1, EditValue::Text("x".into()))
            .unwrap();
        session.set(2, &read, 2, EditValue::Null).unwrap();
        let read = |column: usize| data.value(5, column).map(str::to_owned);
        session.toggle_delete(5, &read).unwrap();
        let inserted = session.insert_row();
        session
            .set_inserted(inserted, 1, EditValue::Text("new".into()))
            .unwrap();
        assert_eq!(session.pending(), 3);
    }

    #[test]
    fn the_files_have_their_header_and_rows() {
        let folder = super::super::temp::TempFolder::create().unwrap();
        let csv = folder.path().join("rows.csv");
        let bytes = write_csv(&csv, 25).unwrap();
        let text = std::fs::read_to_string(&csv).unwrap();
        assert_eq!(bytes as usize, text.len());
        assert_eq!(text.lines().count(), 26);
        assert!(text.starts_with("id,name,amount,active,created_at\n1,Customer 0000001"));
        let xlsx = folder.path().join("rows.xlsx");
        assert!(write_xlsx(&xlsx, folder.path(), 25).unwrap() > 0);
        let preview = crate::import::preview(
            &xlsx,
            &crate::import::ReadOptions::Xlsx(crate::import::XlsxOptions::new("Sheet1")),
            100,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(preview.columns, XLSX_COLUMNS);
        assert_eq!(preview.rows.len(), 25);
        let mapping = import_mapping(&CSV_COLUMNS);
        assert!(mapping.problems(CSV_COLUMNS.len()).is_empty());
        folder.remove().unwrap();
    }

    #[test]
    fn synthetic_runs_end_as_storage_requires() {
        for index in 0..40 {
            let finish = synthetic_finish(index, UtcMillis(1));
            assert!(finish.state.is_terminal());
            assert_eq!(finish.state == RunState::Failed, finish.cause.is_some());
        }
        let job = sql_job("Synthetic", ConnectionId::new(), "SELECT 1", 10);
        assert!(crate::automation::definition::problems(&job).is_empty());
        assert!(jobs_rows_sql(5).contains("i < 5"));
    }

    #[tokio::test]
    async fn sqlite_tables_are_made_filled_and_counted() {
        let folder = super::super::temp::TempFolder::create().unwrap();
        let path = folder.path().join("jobs.sqlite");
        sqlite_run(&path, &[JOBS_TABLE_SQL, &jobs_rows_sql(300)])
            .await
            .unwrap();
        assert_eq!(sqlite_count(&path, "big").await.unwrap(), 300);
        folder.remove().unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_seeded_history_reads_back_in_pages() {
        let folder = super::super::temp::TempFolder::create().unwrap();
        let jykre_storage::Opened::Ready(repository) =
            AutomationRepository::open(folder.path()).await.unwrap()
        else {
            unreachable!("a new file");
        };
        let connection = ConnectionId::new();
        let job = sql_job("Synthetic", connection, "SELECT 1", 10);
        let stored = repository
            .insert_job(job.id, JobKind::Sql, &job.to_json(), UtcMillis::now())
            .await
            .unwrap();
        seed_history(
            &repository,
            &job,
            stored.revision,
            connection,
            30,
            UtcMillis::now(),
        )
        .await
        .unwrap();
        let page = repository
            .history(&jykre_storage::HistoryFilter {
                job: Some(job.id),
                limit: 20,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.len(), 20);
        let failed = repository
            .history(&jykre_storage::HistoryFilter {
                state: Some(RunState::Failed),
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(failed.len(), 3);
        drop(repository);
        // The pool closes its files on the runtime, so the folder is removed
        // off it.
        tokio::task::spawn_blocking(move || folder.remove())
            .await
            .unwrap()
            .unwrap();
    }
}
