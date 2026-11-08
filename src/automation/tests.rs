//! The proof of concept's experiments that need no server (plan, section 4):
//! a scripted driver for admission and races, synthetic SQLite files for
//! statements. PostgreSQL and MySQL run in `tests/live.rs`.
use super::*;
use crate::drivers::SqliteDriver;
use crate::parameters::{ParameterError, ParameterProblem};
use crate::sql::SqlDialect;
use jykre_core::{
    CancellationToken, Cell, ColumnInfo, ConnectionConfig, Credentials, Driver, DriverCapabilities,
    DriverDescriptor, DriverKind, DriverOutcome, DriverResult, DriverStatus, IntrospectionReport,
    IntrospectionSink, ObjectKind, ParameterKind, QueryRequest, ResolvedConnection, RowSink,
    ServerInfo, Session, SinkControl, async_trait,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// A scripted driver.

#[derive(Debug, Clone)]
enum Script {
    /// Runs until cancelled.
    Blocks,
    /// Runs until `release` is notified.
    WaitsFor(Arc<Notify>),
    /// Sends these columns and rows, honouring the row limit and Stop.
    Rows(Vec<ColumnInfo>, Vec<Vec<Cell>>),
}

#[derive(Debug, Clone)]
struct Scripted {
    script: Script,
    opened: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
}

impl Scripted {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            opened: Arc::default(),
            closed: Arc::default(),
        })
    }
}

#[derive(Debug)]
struct ScriptedSession {
    script: Script,
    closed: Arc<AtomicUsize>,
}

#[async_trait]
impl Session for ScriptedSession {
    async fn execute(
        &mut self,
        request: &QueryRequest,
        sink: &mut dyn RowSink,
        cancel: CancellationToken,
    ) -> DriverResult<DriverOutcome> {
        match &self.script {
            Script::Blocks => {
                cancel.cancelled().await;
                Err(DriverError::cancelled())
            }
            Script::WaitsFor(release) => {
                tokio::select! {
                    () = release.notified() => Ok(DriverOutcome::default()),
                    () = cancel.cancelled() => Err(DriverError::cancelled()),
                }
            }
            Script::Rows(columns, rows) => {
                if sink.columns(columns.clone()) == SinkControl::Stop {
                    return Ok(DriverOutcome::default());
                }
                for (index, row) in rows.iter().enumerate() {
                    if index == request.options.max_rows as usize {
                        return Ok(DriverOutcome {
                            has_more: false,
                            truncated: true,
                            ..DriverOutcome::default()
                        });
                    }
                    if sink.row(row.clone()) == SinkControl::Stop {
                        return Ok(DriverOutcome::default());
                    }
                }
                Ok(DriverOutcome::default())
            }
        }
    }

    async fn close(self: Box<Self>) -> DriverResult<()> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl Driver for Scripted {
    fn descriptor(&self) -> DriverDescriptor {
        DriverDescriptor {
            kind: DriverKind::Postgres,
            display_name: "Scripted".into(),
            dialect: SqlDialect::Postgres,
            status: DriverStatus::Ready,
            capabilities: DriverCapabilities {
                sql: true,
                metadata: false,
                transactions: false,
                hierarchy: vec![ObjectKind::Table],
                default_port: None,
                identifier_quote: '"',
                named_parameters: true,
            },
        }
    }

    fn validate_config(&self, _options: &serde_json::Value) -> DriverResult<()> {
        Ok(())
    }

    async fn test(
        &self,
        _connection: &ResolvedConnection,
        _cancel: CancellationToken,
    ) -> DriverResult<ServerInfo> {
        Err(DriverError::validation("not used"))
    }

    async fn open_session(
        &self,
        _connection: &ResolvedConnection,
        _cancel: CancellationToken,
    ) -> DriverResult<Box<dyn Session>> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ScriptedSession {
            script: self.script.clone(),
            closed: self.closed.clone(),
        }))
    }

    async fn introspect(
        &self,
        _connection: &ResolvedConnection,
        _sink: &mut dyn IntrospectionSink,
        _cancel: CancellationToken,
    ) -> DriverResult<IntrospectionReport> {
        Err(DriverError::validation("not used"))
    }
}

fn scripted_input() -> ResolvedConnection {
    ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::Postgres,
            options: serde_json::json!({}),
        },
        credentials: Credentials::default(),
    }
}

fn scripted_run(driver: &Arc<Scripted>, job: SqlJob) -> RunRequest {
    RunRequest {
        job,
        driver: driver.clone(),
        connection: scripted_input(),
        values: HashMap::new(),
    }
}

/// Waits, within `limit`, until `done` holds.
async fn eventually(limit: Duration, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < until, "condition not reached in {limit:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Synthetic SQLite files.

struct SqliteFile {
    path: PathBuf,
    input: ResolvedConnection,
}

impl SqliteFile {
    async fn new(setup: &[&str]) -> Self {
        let path = std::env::temp_dir().join(format!("jykre-automation-{}.sqlite", RunId::new()));
        std::fs::write(&path, b"").unwrap();
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Sqlite,
                options: serde_json::json!({"path": path.to_string_lossy()}),
            },
            credentials: Credentials::default(),
        };
        let file = Self { path, input };
        let mut session = file.session().await;
        for sql in setup {
            exec(&mut *session, sql).await;
        }
        session.close().await.unwrap();
        file
    }

    async fn session(&self) -> Box<dyn Session> {
        SqliteDriver
            .open_session(&self.input, CancellationToken::new())
            .await
            .unwrap()
    }

    fn run(&self, job: SqlJob, values: &[(&str, &str)]) -> RunRequest {
        RunRequest {
            job,
            driver: Arc::new(SqliteDriver),
            connection: self.input.clone(),
            values: values
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    /// The first cell of a query, read through a job of its own.
    async fn scalar(&self, engine: &Engine, sql: &str) -> Option<String> {
        let mut handle = engine
            .submit(self.run(SqlJob::new(ConnectionId::new(), sql), &[]))
            .unwrap();
        let outcome = handle.finished().await.unwrap();
        assert_eq!(outcome.state, RunState::Succeeded, "{:?}", outcome.error);
        outcome.data.value(0, 0).map(str::to_owned)
    }
}

impl Drop for SqliteFile {
    fn drop(&mut self) {
        // Only this test's synthetic file; a leftover in the temporary
        // folder is harmless.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Runs one statement on a session the test holds, as a tab would.
async fn exec(session: &mut dyn Session, sql: &str) -> crate::result::ResultData {
    let mut builder = crate::result::ResultBuilder::new(1_000, 1 << 20);
    session
        .execute(
            &QueryRequest::new(sql, ExecutionOptions::default()),
            &mut builder,
            CancellationToken::new(),
        )
        .await
        .map_err(|error| format!("{sql}: {error}"))
        .unwrap();
    builder.finish().0
}

const NUMBERS: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)";
const TEN_THOUSAND: &str = "INSERT INTO t WITH RECURSIVE n(i) AS \
     (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000) SELECT i, 0 FROM n";

fn engine() -> Engine {
    Engine::new(Handle::current())
}

fn job(sql: &str) -> SqlJob {
    SqlJob::new(ConnectionId::new(), sql)
}

fn spec(name: &str, kind: ParameterKind) -> ParameterSpec {
    let mut spec = ParameterSpec::new(name);
    spec.kind = kind;
    spec
}

// ---------------------------------------------------------------------------
// E1: binding and one statement.

#[test]
fn markers_bind_as_parameters_and_literals_stay_untouched() {
    let sql =
        "SELECT :id::text, ':skip' /* :c */, $$ :also $$, \"col:x\" -- :d\nFROM t WHERE id = :id";
    let mut job = job(sql);
    job.parameters = vec![spec("id", ParameterKind::Number)];
    let values = HashMap::from([("id".to_owned(), " 42 ".to_owned())]);
    let prepared = prepare(&job, SqlDialect::Postgres, &values).unwrap();
    let request = prepared.request();
    assert_eq!(request.sql, sql, "the SQL is sent as written");
    assert_eq!(request.parameters.len(), 1);
    let id = &request.parameters[0];
    assert_eq!(
        (id.name.as_str(), id.kind, id.value.as_deref()),
        ("id", ParameterKind::Number, Some("42"))
    );
    let markers: Vec<&str> = id.markers.iter().map(|range| &sql[range.clone()]).collect();
    assert_eq!(markers, [":id", ":id"]);
    assert!(prepared.reads_only());
}

#[test]
fn anything_but_one_statement_or_a_valid_value_is_refused() {
    let none = HashMap::new();
    for (sql, found) in [("SELECT 1; SELECT 2", 2), ("  -- nothing\n", 0), (";", 0)] {
        assert_eq!(
            prepare(&job(sql), SqlDialect::Postgres, &none).unwrap_err(),
            PrepareError::Statements { found },
            "{sql}"
        );
    }
    // One statement whose body holds semicolons is one statement.
    let body = "CREATE FUNCTION f() RETURNS int BEGIN ATOMIC SELECT 1; END";
    assert!(prepare(&job(body), SqlDialect::Postgres, &none).is_ok());
    // A marker without a specification, and a value of the wrong kind.
    let mut typed = job("SELECT :a, :b");
    typed.parameters = vec![spec("a", ParameterKind::Date)];
    let values = HashMap::from([("a".to_owned(), "tomorrow".to_owned())]);
    assert_eq!(
        prepare(&typed, SqlDialect::Postgres, &values).unwrap_err(),
        PrepareError::Parameters(vec![
            ParameterError {
                name: "a".into(),
                problem: ParameterProblem::Invalid
            },
            ParameterError {
                name: "b".into(),
                problem: ParameterProblem::Missing
            },
        ])
    );
    let mut zero = job("SELECT 1");
    zero.timeout = Duration::ZERO;
    assert!(matches!(
        prepare(&zero, SqlDialect::Postgres, &none),
        Err(PrepareError::Options(_))
    ));
}

#[tokio::test]
async fn two_statements_are_refused_before_a_session_opens() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine();
    let refused = engine
        .submit(scripted_run(
            &driver,
            job("UPDATE t SET v = 1; DELETE FROM t"),
        ))
        .unwrap_err();
    assert_eq!(
        refused,
        Refusal::Invalid(PrepareError::Statements { found: 2 })
    );
    assert_eq!(driver.opened.load(Ordering::SeqCst), 0);
    assert!(engine.runs().is_empty());
}

#[tokio::test]
async fn a_bound_value_reaches_sqlite_as_a_value() {
    let file = SqliteFile::new(&[]).await;
    let engine = engine();
    let mut job = job("SELECT :name AS v, ':name' AS literal /* :name */");
    job.parameters = vec![ParameterSpec::new("name")];
    let hostile = "x'); DROP TABLE t; --";
    let mut handle = engine.submit(file.run(job, &[("name", hostile)])).unwrap();
    let outcome = handle.finished().await.unwrap();
    assert_eq!(outcome.state, RunState::Succeeded, "{:?}", outcome.error);
    assert_eq!(outcome.data.value(0, 0), Some(hostile));
    assert_eq!(outcome.data.value(0, 1), Some(":name"));
}

// ---------------------------------------------------------------------------
// E2–E4: the completion rule.

#[tokio::test]
async fn a_write_beyond_the_row_limit_is_read_to_its_end() {
    let file = SqliteFile::new(&[NUMBERS, TEN_THOUSAND]).await;
    let engine = engine();
    let mut handle = engine
        .submit(file.run(job("UPDATE t SET v = v + 1 RETURNING id"), &[]))
        .unwrap();
    let outcome = handle.finished().await.unwrap();
    assert_eq!(outcome.state, RunState::Succeeded, "{:?}", outcome.error);
    assert!(!outcome.effect_unknown);
    assert!(outcome.truncated);
    assert_eq!(outcome.data.row_count(), 1_000);
    assert_eq!(outcome.rows_discarded, 9_000);
    // Drivers report no affected rows for a statement that returns rows.
    assert_eq!(outcome.rows_affected, None);
    assert!(outcome.completion.is_some());
    assert_eq!(
        file.scalar(&engine, "SELECT count(*) FROM t WHERE v = 1")
            .await
            .as_deref(),
        Some("10000")
    );
    let summary = &engine.runs()[1];
    assert_eq!(
        (summary.state, summary.rows_delivered, summary.truncated),
        (RunState::Succeeded, 1_000, true)
    );
}

#[tokio::test]
async fn a_write_without_rows_reports_what_it_changed() {
    let file = SqliteFile::new(&[NUMBERS, TEN_THOUSAND]).await;
    let engine = engine();
    let mut handle = engine
        .submit(file.run(job("UPDATE t SET v = 7 WHERE id <= 25"), &[]))
        .unwrap();
    let outcome = handle.finished().await.unwrap();
    assert_eq!(outcome.state, RunState::Succeeded);
    assert_eq!(outcome.rows_affected, Some(25));
    assert!(!outcome.truncated && outcome.completion.is_none());
}

#[tokio::test]
async fn a_write_past_its_deadline_fails_with_its_effect_unknown() {
    let file = SqliteFile::new(&[NUMBERS, TEN_THOUSAND]).await;
    let engine = engine();
    // Each returned row scans the table again: seconds of work.
    let mut slow = job(
        "UPDATE t SET v = v + 1 RETURNING (SELECT count(*) FROM t AS a, t AS b WHERE a.id = b.id AND a.v >= t.v - 1)",
    );
    slow.timeout = Duration::from_millis(300);
    let started = Instant::now();
    let mut handle = engine.submit(file.run(slow, &[])).unwrap();
    let outcome = handle.finished().await.unwrap();
    assert_eq!(outcome.state, RunState::Failed);
    assert_eq!(outcome.cause, Some(Cause::Timeout));
    assert!(outcome.effect_unknown);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    // Never applied twice, whatever happened: every row moved together.
    let values = file
        .scalar(&engine, "SELECT group_concat(DISTINCT v) FROM t")
        .await;
    assert!(matches!(values.as_deref(), Some("0" | "1")), "{values:?}");
}

#[tokio::test]
async fn a_query_above_the_limit_is_truncated_without_reading_the_rest() {
    let file = SqliteFile::new(&[NUMBERS, TEN_THOUSAND]).await;
    let engine = engine();
    let mut handle = engine
        .submit(file.run(job("SELECT id, v FROM t ORDER BY id"), &[]))
        .unwrap();
    let outcome = handle.finished().await.unwrap();
    assert_eq!(outcome.state, RunState::Succeeded);
    assert!(outcome.truncated && !outcome.effect_unknown);
    assert_eq!(outcome.data.row_count(), 1_000);
    assert_eq!(outcome.rows_discarded, 0);
    assert!(outcome.completion.is_none());
}

#[tokio::test]
async fn a_transaction_left_open_is_rolled_back_and_said() {
    let file = SqliteFile::new(&[NUMBERS]).await;
    let engine = engine();
    let mut handle = engine.submit(file.run(job("BEGIN"), &[])).unwrap();
    let outcome = handle.finished().await.unwrap();
    assert_eq!(outcome.state, RunState::Succeeded);
    assert!(outcome.transaction_rolled_back);
}

// ---------------------------------------------------------------------------
// E5: a tab's open transaction.

#[tokio::test]
async fn a_run_never_touches_a_transaction_open_in_another_session() {
    let file =
        SqliteFile::new(&["CREATE TABLE a (id INTEGER)", "CREATE TABLE b (id INTEGER)"]).await;
    let engine = engine();
    let mut tab = file.session().await;
    exec(&mut *tab, "BEGIN").await;
    exec(&mut *tab, "INSERT INTO a VALUES (1)").await;
    // The job's own session does not see the uncommitted row.
    assert_eq!(
        file.scalar(&engine, "SELECT count(*) FROM a")
            .await
            .as_deref(),
        Some("0")
    );
    // SQLite has one writer: the job waits for the tab's lock, then fails
    // on its own, without committing or rolling back the tab's work.
    let mut handle = engine
        .submit(file.run(job("INSERT INTO b VALUES (1)"), &[]))
        .unwrap();
    let outcome = handle.finished().await.unwrap();
    assert_eq!(outcome.state, RunState::Failed);
    assert_eq!(outcome.cause, Some(Cause::Database));
    assert!(!outcome.effect_unknown);
    assert!(matches!(
        tab.transaction(CancellationToken::new()).await.unwrap(),
        Some(jykre_core::TransactionStatus::Open { .. })
    ));
    let seen = exec(&mut *tab, "SELECT count(*) FROM a").await;
    assert_eq!(seen.value(0, 0), Some("1"));
    exec(&mut *tab, "COMMIT").await;
    tab.close().await.unwrap();
    assert_eq!(
        file.scalar(
            &engine,
            "SELECT (SELECT count(*) FROM a) || '/' || (SELECT count(*) FROM b)"
        )
        .await
        .as_deref(),
        Some("1/0")
    );
}

// ---------------------------------------------------------------------------
// E6–E8: admission, arbitration and ownership.

#[tokio::test]
async fn admission_refuses_beyond_the_limits_at_once() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine();
    let (first, second) = (ConnectionId::new(), ConnectionId::new());
    let on = |connection| {
        let mut job = job("SELECT 1");
        job.connection = connection;
        engine.submit(scripted_run(&driver, job))
    };
    let one = on(first).unwrap();
    assert_eq!(
        on(first).unwrap_err(),
        Refusal::ConnectionBusy { connection: first }
    );
    let two = on(second).unwrap();
    assert_eq!(
        on(ConnectionId::new()).unwrap_err(),
        Refusal::GlobalLimit { limit: 2 }
    );
    assert_eq!(engine.active(), 2);
    for handle in [one, two] {
        assert!(engine.stop(handle.id));
        let mut handle = handle;
        assert_eq!(handle.finished().await.unwrap().state, RunState::Cancelled);
    }
    // A slot freed is a slot available.
    assert!(on(first).is_ok());
    engine.shutdown().await;
}

#[tokio::test]
async fn stop_ends_a_run_at_once_and_closes_its_session() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine();
    let mut write = engine
        .submit(scripted_run(&driver, job("DELETE FROM t WHERE id = 1")))
        .unwrap();
    eventually(Duration::from_secs(5), || {
        matches!(
            write.status(),
            RunStatus::Active {
                phase: Phase::Executing,
                ..
            }
        )
    })
    .await;
    let stopped = Instant::now();
    assert!(engine.stop(write.id));
    let outcome = write.finished().await.unwrap();
    assert!(stopped.elapsed() < Duration::from_secs(1));
    assert_eq!((outcome.state, outcome.cause), (RunState::Cancelled, None));
    assert!(outcome.effect_unknown, "a write that was sent");
    assert_eq!(driver.closed.load(Ordering::SeqCst), 1);
    assert!(!engine.stop(write.id), "no longer active");

    let mut read = engine
        .submit(scripted_run(&driver, job("SELECT 1")))
        .unwrap();
    assert!(engine.stop(read.id));
    let outcome = read.finished().await.unwrap();
    assert_eq!(outcome.state, RunState::Cancelled);
    assert!(!outcome.effect_unknown, "a read changes nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_and_the_deadline_together_leave_one_outcome() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine();
    let (mut cancelled, mut timed_out) = (0, 0);
    for _ in 0..200 {
        let mut job = job("UPDATE t SET v = 1");
        job.timeout = Duration::from_millis(2);
        let mut handle = engine.submit(scripted_run(&driver, job)).unwrap();
        let stopper = engine.clone();
        let id = handle.id;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2)).await;
            stopper.stop(id);
        });
        let outcome = handle.finished().await.unwrap();
        match (outcome.state, outcome.cause) {
            (RunState::Cancelled, None) => cancelled += 1,
            (RunState::Failed, Some(Cause::Timeout)) => timed_out += 1,
            other => unreachable!("unexpected outcome {other:?}"),
        }
        // Nothing follows the outcome, and the history agrees with it.
        assert!(!handle.changed().await);
        let runs: Vec<RunSummary> = engine
            .runs()
            .into_iter()
            .filter(|run| run.id == id)
            .collect();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            (runs[0].state, runs[0].cause),
            (outcome.state, outcome.cause)
        );
    }
    assert_eq!(cancelled + timed_out, 200);
    assert_eq!(engine.active(), 0);
    assert_eq!(
        driver.opened.load(Ordering::SeqCst),
        driver.closed.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn dropping_every_handle_leaves_the_run_going() {
    let release = Arc::new(Notify::new());
    let driver = Scripted::new(Script::WaitsFor(release.clone()));
    let engine = engine();
    let handle = engine
        .submit(scripted_run(&driver, job("UPDATE t SET v = 1")))
        .unwrap();
    let id = handle.id;
    eventually(Duration::from_secs(5), || {
        matches!(
            handle.status(),
            RunStatus::Active {
                phase: Phase::Executing,
                ..
            }
        )
    })
    .await;
    drop(handle);
    release.notify_one();
    eventually(Duration::from_secs(5), || engine.active() == 0).await;
    let run = engine.runs().into_iter().find(|run| run.id == id).unwrap();
    assert_eq!(run.state, RunState::Succeeded);
    assert_eq!(driver.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn shutdown_stops_every_run_and_refuses_new_ones() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine();
    let first = engine
        .submit(scripted_run(&driver, job("SELECT 1")))
        .unwrap();
    let second = engine
        .submit(scripted_run(&driver, job("SELECT 2")))
        .unwrap();
    engine.shutdown().await;
    assert_eq!(engine.active(), 0);
    for handle in [first, second] {
        assert!(matches!(
            handle.status(),
            RunStatus::Finished(outcome) if outcome.state == RunState::Cancelled
        ));
    }
    assert_eq!(driver.closed.load(Ordering::SeqCst), 2);
    assert_eq!(
        engine
            .submit(scripted_run(&driver, job("SELECT 3")))
            .unwrap_err(),
        Refusal::ShuttingDown
    );
}

// ---------------------------------------------------------------------------
// E9: the in-memory output.

#[tokio::test]
async fn the_result_keeps_order_duplicates_null_and_exact_text() {
    let file = SqliteFile::new(&["CREATE TABLE x (id INTEGER)", "INSERT INTO x VALUES (1)"]).await;
    let engine = engine();
    let mut handle = engine
        .submit(file.run(
            job("SELECT a.id, b.id, NULL AS n, 'NULL' AS t, '0.100000000000000000' AS d FROM x AS a, x AS b"),
            &[],
        ))
        .unwrap();
    let outcome = handle.finished().await.unwrap();
    let names: Vec<&str> = outcome
        .data
        .columns()
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, ["id", "id", "n", "t", "d"]);
    assert!(outcome.data.is_null(0, 2));
    assert_eq!(outcome.data.value(0, 3), Some("NULL"));
    assert!(!outcome.data.is_null(0, 3));
    assert_eq!(outcome.data.value(0, 4), Some("0.100000000000000000"));

    // A driver's exact decimal is kept as its text, never as a float.
    let exact = "12345678901234567890.123456789012345678";
    let driver = Scripted::new(Script::Rows(
        vec![ColumnInfo {
            name: "d".into(),
            data_type: "NUMERIC".into(),
            nullable: None,
        }],
        vec![vec![Cell::Decimal(exact.into())]],
    ));
    let mut handle = engine
        .submit(scripted_run(&driver, job("SELECT d FROM t")))
        .unwrap();
    let outcome = handle.finished().await.unwrap();
    assert_eq!(outcome.data.value(0, 0), Some(exact));
}

#[tokio::test]
async fn the_history_records_no_value_or_message() {
    let file = SqliteFile::new(&[]).await;
    let engine = engine();
    let mut failing = job("SELECT * FROM missing_table WHERE secret = :secret");
    failing.parameters = vec![ParameterSpec::new("secret")];
    let mut handle = engine
        .submit(file.run(failing, &[("secret", "s3cr3t-value")]))
        .unwrap();
    let outcome = handle.finished().await.unwrap();
    assert_eq!(
        (outcome.state, outcome.cause),
        (RunState::Failed, Some(Cause::Database))
    );
    assert!(
        outcome.error.is_some(),
        "the user still reads the server's message"
    );
    let recorded = format!("{:?}", engine.runs());
    assert!(!recorded.contains("s3cr3t-value"));
    assert!(!recorded.contains("missing_table"));
}
