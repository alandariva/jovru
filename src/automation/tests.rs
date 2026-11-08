//! The engine's tests that need no server: a scripted driver for admission,
//! the queue and races, synthetic SQLite files for statements, history and
//! chains. PostgreSQL and MySQL run in `tests/live.rs`.
use super::definition::{ChainStep, JobBody, JobDefinition, RowRule, Source, SqlOutput};
use super::queue::{Blocked, Limits, QueueFull};
use super::*;
use crate::drivers::SqliteDriver;
use crate::parameters::{ParameterError, ParameterProblem};
use crate::sql::SqlDialect;
use jykre_core::{
    CancellationToken, Cell, ColumnInfo, ConnectionConfig, Credentials, Driver, DriverCapabilities,
    DriverDescriptor, DriverKind, DriverOutcome, DriverResult, DriverStatus, ExecutionOptions,
    IntrospectionReport, IntrospectionSink, ObjectKind, ParameterKind, QueryRequest,
    ResolvedConnection, RowSink, ServerInfo, Session, SinkControl, async_trait,
};
use jykre_storage::{AutomationRepository, HistoryEntry, HistoryFilter, NewRun};
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

// ---------------------------------------------------------------------------
// Engines and jobs.

fn data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("jykre-automation-engine-{}", RunId::new()))
}

async fn engine_in(dir: PathBuf, limits: Limits) -> Engine {
    let engine = Engine::with_limits(
        Handle::current(),
        dir,
        limits,
        crate::result::DEFAULT_MEMORY_CEILING,
        process::Timings::default(),
    );
    assert_eq!(engine.started().await, Availability::Ready);
    engine
}

async fn engine() -> Engine {
    engine_in(data_dir(), Limits::default()).await
}

/// A SQL Job definition on `connection`.
fn sql_job(connection: ConnectionId, sql: &str) -> JobDefinition {
    let mut job = JobDefinition::new_sql("job", Some(connection));
    if let JobBody::Sql(body) = &mut job.body {
        body.sql = sql.into();
    }
    job
}

fn body(job: &mut JobDefinition) -> &mut definition::SqlBody {
    match &mut job.body {
        JobBody::Sql(body) => body,
        _ => unreachable!("a SQL job"),
    }
}

fn spec(name: &str, kind: ParameterKind) -> ParameterSpec {
    let mut spec = ParameterSpec::new(name);
    spec.kind = kind;
    spec
}

fn scripted_input(driver: &Arc<Scripted>, id: ConnectionId) -> ConnectionInput {
    ConnectionInput {
        id,
        driver: driver.clone(),
        resolved: ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({}),
            },
            credentials: Credentials::default(),
        },
        revision: 1,
    }
}

/// A draft run of `job` on the scripted driver.
fn scripted_run(driver: &Arc<Scripted>, job: JobDefinition) -> JobRequest {
    let connection = match &job.body {
        JobBody::Sql(body) => body.connection.unwrap_or_default(),
        _ => ConnectionId::new(),
    };
    JobRequest {
        definition: job,
        stored: None,
        values: HashMap::new(),
        connection: Some(scripted_input(driver, connection)),
    }
}

fn outcome(status: RunStatus) -> Arc<RunOutcome> {
    match status {
        RunStatus::Finished(outcome) => outcome,
        other => unreachable!("not finished: {other:?}"),
    }
}

fn sql(outcome: &RunOutcome) -> &SqlOutcome {
    outcome.sql.as_ref().unwrap()
}

async fn finished(handle: &mut RunHandle) -> Arc<RunOutcome> {
    outcome(handle.finished().await)
}

async fn record(engine: &Engine, id: RunId) -> RunRecord {
    engine.repository().unwrap().run(id).await.unwrap().unwrap()
}

/// Waits, within `limit`, until `done` holds.
async fn eventually(limit: Duration, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < until, "condition not reached in {limit:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn executing(handle: &RunHandle) -> bool {
    matches!(
        handle.status(),
        RunStatus::Active(Progress {
            phase: Phase::Executing,
            ..
        })
    )
}

// ---------------------------------------------------------------------------
// Synthetic SQLite files.

struct SqliteFile {
    path: PathBuf,
    id: ConnectionId,
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
        let file = Self {
            path,
            id: ConnectionId::new(),
            input,
        };
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

    fn connection(&self) -> ConnectionInput {
        ConnectionInput {
            id: self.id,
            driver: Arc::new(SqliteDriver),
            resolved: self.input.clone(),
            revision: 1,
        }
    }

    fn job(&self, sql: &str) -> JobDefinition {
        sql_job(self.id, sql)
    }

    fn run(&self, job: JobDefinition, values: &[(&str, &str)]) -> JobRequest {
        JobRequest {
            definition: job,
            stored: None,
            values: values
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            connection: Some(self.connection()),
        }
    }

    /// The first cell of a query, read through a job of its own.
    async fn scalar(&self, engine: &Engine, sql: &str) -> Option<String> {
        let mut handle = engine.submit(self.run(self.job(sql), &[])).unwrap();
        let outcome = finished(&mut handle).await;
        assert_eq!(outcome.state, RunState::Succeeded, "{:?}", outcome.error);
        self::sql(&outcome).data.value(0, 0).map(str::to_owned)
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

fn prepared_job(sql: &str) -> SqlJob {
    SqlJob::new(ConnectionId::new(), sql)
}

// ---------------------------------------------------------------------------
// Binding and one statement (section 4.2, 4.3).

#[test]
fn markers_bind_as_parameters_and_literals_stay_untouched() {
    let sql =
        "SELECT :id::text, ':skip' /* :c */, $$ :also $$, \"col:x\" -- :d\nFROM t WHERE id = :id";
    let mut job = prepared_job(sql);
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
            prepare(&prepared_job(sql), SqlDialect::Postgres, &none).unwrap_err(),
            PrepareError::Statements { found },
            "{sql}"
        );
    }
    // One statement whose body holds semicolons is one statement.
    let body = "CREATE FUNCTION f() RETURNS int BEGIN ATOMIC SELECT 1; END";
    assert!(prepare(&prepared_job(body), SqlDialect::Postgres, &none).is_ok());
    // A marker without a specification, and a value of the wrong kind.
    let mut typed = prepared_job("SELECT :a, :b");
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
    let mut zero = prepared_job("SELECT 1");
    zero.timeout = Duration::ZERO;
    assert!(matches!(
        prepare(&zero, SqlDialect::Postgres, &none),
        Err(PrepareError::Options(_))
    ));
}

#[tokio::test]
async fn a_refused_run_opens_no_session_and_leaves_no_history() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine().await;
    let connection = ConnectionId::new();
    let refused = engine
        .submit(scripted_run(
            &driver,
            sql_job(connection, "UPDATE t SET v = 1; DELETE FROM t"),
        ))
        .unwrap_err();
    assert_eq!(
        refused,
        Refusal::Invalid(PrepareError::Statements { found: 2 })
    );
    let mut missing = sql_job(connection, "SELECT :id");
    body(&mut missing).parameters = vec![spec("id", ParameterKind::Number)];
    assert!(matches!(
        engine.submit(scripted_run(&driver, missing)),
        Err(Refusal::Invalid(PrepareError::Parameters(_)))
    ));
    // A run whose job names another connection than the one given.
    let mut other = scripted_run(&driver, sql_job(connection, "SELECT 1"));
    other.connection = Some(scripted_input(&driver, ConnectionId::new()));
    assert_eq!(engine.submit(other).unwrap_err(), Refusal::NoConnection);
    assert_eq!(driver.opened.load(Ordering::SeqCst), 0);
    let history = engine
        .repository()
        .unwrap()
        .history(&HistoryFilter::default())
        .await
        .unwrap();
    assert!(history.is_empty());
}

#[tokio::test]
async fn a_bound_value_reaches_sqlite_as_a_value() {
    let file = SqliteFile::new(&[]).await;
    let engine = engine().await;
    let mut job = file.job("SELECT :name AS v, ':name' AS literal /* :name */");
    body(&mut job).parameters = vec![ParameterSpec::new("name")];
    let hostile = "x'); DROP TABLE t; --";
    let mut handle = engine.submit(file.run(job, &[("name", hostile)])).unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded, "{:?}", outcome.error);
    assert_eq!(sql(&outcome).data.value(0, 0), Some(hostile));
    assert_eq!(sql(&outcome).data.value(0, 1), Some(":name"));
}

// ---------------------------------------------------------------------------
// The completion rule (section 4.6, CA-14).

#[tokio::test]
async fn a_write_beyond_the_row_limit_is_read_to_its_end() {
    let file = SqliteFile::new(&[NUMBERS, TEN_THOUSAND]).await;
    let engine = engine().await;
    let mut handle = engine
        .submit(file.run(file.job("UPDATE t SET v = v + 1 RETURNING id"), &[]))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded, "{:?}", outcome.error);
    assert!(!outcome.effect_unknown);
    let result = sql(&outcome);
    assert!(result.truncated);
    assert_eq!(result.data.row_count(), 1_000);
    assert_eq!(result.rows_discarded, 9_000);
    // Drivers report no affected rows for a statement that returns rows.
    assert_eq!(result.rows_affected, None);
    assert!(result.completion.is_some());
    assert_eq!(
        file.scalar(&engine, "SELECT count(*) FROM t WHERE v = 1")
            .await
            .as_deref(),
        Some("10000")
    );
    let recorded = record(&engine, handle.id).await;
    assert_eq!(
        (
            recorded.state,
            recorded.rows_delivered,
            recorded.rows_discarded,
            recorded.truncated,
            recorded.reads_only
        ),
        (RunState::Succeeded, Some(1_000), Some(9_000), true, false)
    );
}

#[tokio::test]
async fn a_write_without_rows_reports_what_it_changed() {
    let file = SqliteFile::new(&[NUMBERS, TEN_THOUSAND]).await;
    let engine = engine().await;
    let mut handle = engine
        .submit(file.run(file.job("UPDATE t SET v = 7 WHERE id <= 25"), &[]))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded);
    assert_eq!(sql(&outcome).rows_affected, Some(25));
    assert!(!sql(&outcome).truncated && sql(&outcome).completion.is_none());
    assert_eq!(record(&engine, handle.id).await.rows_affected, Some(25));
}

#[tokio::test]
async fn a_write_past_its_deadline_fails_with_its_effect_unknown_and_is_not_replayed() {
    let file = SqliteFile::new(&[NUMBERS, TEN_THOUSAND]).await;
    let engine = engine().await;
    // Each returned row scans the table again: seconds of work.
    let mut slow = file.job(
        "UPDATE t SET v = v + 1 RETURNING (SELECT count(*) FROM t AS a, t AS b WHERE a.id = b.id AND a.v >= t.v - 1)",
    );
    body(&mut slow).timeout_ms = 300;
    let started = Instant::now();
    let mut handle = engine.submit(file.run(slow, &[])).unwrap();
    let outcome = finished(&mut handle).await;
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
    let recorded = record(&engine, handle.id).await;
    assert_eq!(
        (recorded.state, recorded.cause, recorded.effect_unknown),
        (RunState::Failed, Some(Cause::Timeout), true)
    );
}

#[tokio::test]
async fn a_query_above_the_limit_is_truncated_without_reading_the_rest() {
    let file = SqliteFile::new(&[NUMBERS, TEN_THOUSAND]).await;
    let engine = engine().await;
    let mut handle = engine
        .submit(file.run(file.job("SELECT id, v FROM t ORDER BY id"), &[]))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded);
    assert!(sql(&outcome).truncated && !outcome.effect_unknown);
    assert_eq!(sql(&outcome).data.row_count(), 1_000);
    assert_eq!(sql(&outcome).rows_discarded, 0);
    assert!(sql(&outcome).completion.is_none());
}

#[tokio::test]
async fn a_transaction_left_open_is_rolled_back_and_said() {
    let file = SqliteFile::new(&[NUMBERS]).await;
    let engine = engine().await;
    let mut handle = engine.submit(file.run(file.job("BEGIN"), &[])).unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded);
    assert!(sql(&outcome).transaction_rolled_back);
}

// ---------------------------------------------------------------------------
// A tab's open transaction (CA-02).

#[tokio::test]
async fn a_run_never_touches_a_transaction_open_in_another_session() {
    let file =
        SqliteFile::new(&["CREATE TABLE a (id INTEGER)", "CREATE TABLE b (id INTEGER)"]).await;
    let engine = engine().await;
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
        .submit(file.run(file.job("INSERT INTO b VALUES (1)"), &[]))
        .unwrap();
    let outcome = finished(&mut handle).await;
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
// Admission, the queue, arbitration and ownership (sections 5.3–5.5, 9.3.6).

#[tokio::test]
async fn runs_beyond_the_limits_wait_in_the_queue_and_then_run() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine().await;
    let (first, second) = (ConnectionId::new(), ConnectionId::new());
    let on = |connection| engine.submit(scripted_run(&driver, sql_job(connection, "SELECT 1")));
    let one = on(first).unwrap();
    let mut waiting = on(first).unwrap();
    assert!(matches!(
        waiting.status(),
        RunStatus::Queued {
            position: 1,
            blocked: Blocked::Connection { connection, .. },
            ..
        } if connection == first
    ));
    if let RunStatus::Queued { blocked, .. } = waiting.status() {
        assert_eq!(engine.holders(&blocked), vec!["job".to_owned()]);
    }
    let two = on(second).unwrap();
    assert!(matches!(
        on(ConnectionId::new()).unwrap().status(),
        RunStatus::Queued {
            position: 2,
            blocked: Blocked::Global { limit: 2, .. },
            ..
        }
    ));
    // Stop on the first frees `first`: the queued run starts.
    assert!(engine.stop_run(one.id));
    eventually(Duration::from_secs(5), || executing(&waiting)).await;
    // Stop on a queued run takes it out of the queue: it never ran.
    let queued: Vec<RunId> = engine
        .overview()
        .iter()
        .filter(|entry| matches!(entry.status, Some(RunStatus::Queued { .. })))
        .filter_map(|entry| entry.run)
        .collect();
    assert_eq!(queued.len(), 1);
    assert!(engine.stop_run(queued[0]));
    let mut withdrawn = engine
        .subscribe(queued[0])
        .unwrap_or_else(|| waiting.clone());
    if withdrawn.id == queued[0] {
        assert!(matches!(
            withdrawn.finished().await,
            RunStatus::Refused(Refusal::Withdrawn)
        ));
    }
    for id in [waiting.id, two.id] {
        engine.stop_run(id);
    }
    assert_eq!(finished(&mut waiting).await.state, RunState::Cancelled);
    engine.shutdown().await;
}

#[tokio::test]
async fn the_queue_refuses_beyond_its_size_and_after_its_wait() {
    let driver = Scripted::new(Script::Blocks);
    let limits = Limits {
        global: 1,
        queue_global: 2,
        queue_wait: Duration::from_millis(200),
        ..Limits::default()
    };
    let engine = engine_in(data_dir(), limits).await;
    let run = || {
        engine.submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "SELECT 1"),
        ))
    };
    let active = run().unwrap();
    let mut late = run().unwrap();
    let _also = run().unwrap();
    assert_eq!(
        run().unwrap_err(),
        Refusal::QueueFull(QueueFull::Global { limit: 2 })
    );
    // Past its wait, a queued run leaves the queue, refused, with no history.
    assert!(matches!(
        late.finished().await,
        RunStatus::Refused(Refusal::QueueTimeout)
    ));
    let history = engine
        .repository()
        .unwrap()
        .history(&HistoryFilter::default())
        .await
        .unwrap();
    assert_eq!(history.len(), 1, "only the active run has a row");
    engine.stop_run(active.id);
    engine.shutdown().await;
}

#[tokio::test]
async fn a_run_is_recorded_running_before_it_is_sent() {
    let release = Arc::new(Notify::new());
    let driver = Scripted::new(Script::WaitsFor(release.clone()));
    let engine = engine().await;
    let handle = engine
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "UPDATE t SET v = 1"),
        ))
        .unwrap();
    eventually(Duration::from_secs(5), || executing(&handle)).await;
    let running = record(&engine, handle.id).await;
    assert_eq!(running.state, RunState::Running);
    assert!(running.finished_at.is_none());
    assert!(running.fingerprint.starts_with("sha256:"));
    // Dropping every handle leaves the run going to its end.
    let id = handle.id;
    drop(handle);
    release.notify_one();
    eventually(Duration::from_secs(5), || engine.active() == 0).await;
    let done = record(&engine, id).await;
    assert_eq!(done.state, RunState::Succeeded);
    assert_eq!(driver.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stop_ends_a_run_at_once_and_closes_its_session() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine().await;
    let mut write = engine
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "DELETE FROM t WHERE id = 1"),
        ))
        .unwrap();
    eventually(Duration::from_secs(5), || executing(&write)).await;
    let stopped = Instant::now();
    assert!(engine.stop_run(write.id));
    let outcome = finished(&mut write).await;
    assert!(stopped.elapsed() < Duration::from_secs(1));
    assert_eq!((outcome.state, outcome.cause), (RunState::Cancelled, None));
    assert!(outcome.effect_unknown, "a write that was sent");
    assert_eq!(driver.closed.load(Ordering::SeqCst), 1);
    assert!(!engine.stop_run(write.id), "no longer active");

    let mut read = engine
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "SELECT 1"),
        ))
        .unwrap();
    eventually(Duration::from_secs(5), || executing(&read)).await;
    assert!(engine.stop_run(read.id));
    let outcome = finished(&mut read).await;
    assert_eq!(outcome.state, RunState::Cancelled);
    assert!(!outcome.effect_unknown, "a read changes nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_and_the_deadline_together_leave_one_outcome() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine().await;
    let (mut cancelled, mut timed_out) = (0, 0);
    for _ in 0..100 {
        let mut job = sql_job(ConnectionId::new(), "UPDATE t SET v = 1");
        body(&mut job).timeout_ms = 2;
        let mut handle = engine.submit(scripted_run(&driver, job)).unwrap();
        let stopper = engine.clone();
        let id = handle.id;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2)).await;
            stopper.stop_run(id);
        });
        let outcome = finished(&mut handle).await;
        match (outcome.state, outcome.cause) {
            (RunState::Cancelled, None) => cancelled += 1,
            (RunState::Failed, Some(Cause::Timeout)) => timed_out += 1,
            other => unreachable!("unexpected outcome {other:?}"),
        }
        // Nothing follows the outcome, and the history agrees with it.
        assert!(!handle.changed().await);
        let recorded = record(&engine, id).await;
        assert_eq!(
            (recorded.state, recorded.cause),
            (outcome.state, outcome.cause)
        );
    }
    assert_eq!(cancelled + timed_out, 100);
    assert_eq!(engine.active(), 0);
    assert_eq!(
        driver.opened.load(Ordering::SeqCst),
        driver.closed.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn shutdown_empties_the_queue_stops_every_run_and_refuses_new_ones() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine().await;
    let run = || {
        engine.submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "SELECT 1"),
        ))
    };
    let first = run().unwrap();
    let second = run().unwrap();
    let mut queued = run().unwrap();
    eventually(Duration::from_secs(5), || {
        executing(&first) && executing(&second)
    })
    .await;
    engine.shutdown().await;
    assert_eq!(engine.active(), 0);
    for handle in [first, second] {
        assert!(matches!(
            handle.status(),
            RunStatus::Finished(outcome) if outcome.state == RunState::Cancelled
        ));
    }
    assert!(matches!(
        queued.finished().await,
        RunStatus::Refused(Refusal::ShuttingDown)
    ));
    assert_eq!(driver.closed.load(Ordering::SeqCst), 2);
    assert_eq!(run().unwrap_err(), Refusal::ShuttingDown);
}

// ---------------------------------------------------------------------------
// Recovery and the lock (section 6.6, CA-11).

#[tokio::test]
async fn runs_left_running_are_interrupted_at_start_and_never_replayed() {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let repository = match AutomationRepository::open(&dir).await.unwrap() {
        jykre_storage::Opened::Ready(repository) => repository,
        _ => unreachable!("a new file"),
    };
    let left = |reads_only: bool| NewRun {
        id: RunId::new(),
        kind: JobKind::Sql,
        job_id: None,
        job_revision: None,
        job_version: None,
        chain_run_id: None,
        step_index: None,
        origin: RunOrigin::Manual,
        reads_only,
        snapshot_json: "{}".into(),
        connection_id: Some(ConnectionId::new()),
        connection_revision: Some(1),
        program_fingerprint: None,
        started_at: UtcMillis::now(),
    };
    let (write, read) = (left(false), left(true));
    repository.admit_run(&write).await.unwrap();
    repository.admit_run(&read).await.unwrap();
    drop(repository);

    let engine = engine_in(dir, Limits::default()).await;
    let recovery = engine.take_recovery().unwrap();
    assert_eq!(recovery.interrupted, 2);
    assert!(engine.take_recovery().is_none(), "told once");
    let write = record(&engine, write.id).await;
    let read = record(&engine, read.id).await;
    assert_eq!(
        (write.state, write.effect_unknown),
        (RunState::Interrupted, true)
    );
    assert_eq!(
        (read.state, read.effect_unknown),
        (RunState::Interrupted, false)
    );
    assert_eq!(engine.active(), 0, "nothing runs again");
}

#[tokio::test]
async fn a_second_engine_on_the_same_folder_is_read_only_until_the_first_ends() {
    let dir = data_dir();
    let first = engine_in(dir.clone(), Limits::default()).await;
    let second = Engine::start(Handle::current(), dir);
    assert_eq!(second.started().await, Availability::ReadOnly);
    assert!(second.repository().is_some(), "history stays readable");
    let driver = Scripted::new(Script::Blocks);
    assert_eq!(
        second
            .submit(scripted_run(
                &driver,
                sql_job(ConnectionId::new(), "SELECT 1")
            ))
            .unwrap_err(),
        Refusal::Unavailable
    );
    assert_eq!(
        second
            .save_job(&sql_job(ConnectionId::new(), "SELECT 1"), None)
            .await
            .unwrap_err(),
        SaveError::Unavailable
    );
    assert_eq!(second.retry().await, Availability::ReadOnly);
    first.shutdown().await;
    drop(first);
    // The first engine's tasks hold it a moment longer.
    let until = Instant::now() + Duration::from_secs(5);
    while second.retry().await != Availability::Ready {
        assert!(Instant::now() < until, "the lock was not released");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// The in-memory result and values (sections 4.7, 6.4, CA-13).

#[tokio::test]
async fn the_result_keeps_order_duplicates_null_and_exact_text() {
    let file = SqliteFile::new(&["CREATE TABLE x (id INTEGER)", "INSERT INTO x VALUES (1)"]).await;
    let engine = engine().await;
    let mut handle = engine
        .submit(file.run(
            file.job("SELECT a.id, b.id, NULL AS n, 'NULL' AS t, '0.100000000000000000' AS d FROM x AS a, x AS b"),
            &[],
        ))
        .unwrap();
    let outcome = finished(&mut handle).await;
    let data = &sql(&outcome).data;
    let names: Vec<&str> = data
        .columns()
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, ["id", "id", "n", "t", "d"]);
    assert!(data.is_null(0, 2));
    assert_eq!(data.value(0, 3), Some("NULL"));
    assert!(!data.is_null(0, 3));
    assert_eq!(data.value(0, 4), Some("0.100000000000000000"));

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
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "SELECT d FROM t"),
        ))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(sql(&outcome).data.value(0, 0), Some(exact));
}

/// Every byte of `automation.db` and its journal, after a checkpoint.
async fn stored_bytes(engine: &Engine) -> Vec<u8> {
    let dir = engine.data_dir().to_path_buf();
    let mut bytes = Vec::new();
    for name in ["automation.db", "automation.db-wal"] {
        if let Ok(read) = std::fs::read(dir.join(name)) {
            bytes.extend(read);
        }
    }
    bytes
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[tokio::test]
async fn no_value_cell_or_message_is_stored_or_logged() {
    let file = SqliteFile::new(&[
        "CREATE TABLE s (v TEXT)",
        "INSERT INTO s VALUES ('cell-7c1f')",
    ])
    .await;
    let engine = engine().await;
    let mut failing = file.job("SELECT * FROM missing_table_9d2 WHERE secret = :secret");
    body(&mut failing).parameters = vec![ParameterSpec::new("secret")];
    let mut handle = engine
        .submit(file.run(failing, &[("secret", "input-4b8e")]))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(
        (outcome.state, outcome.cause),
        (RunState::Failed, Some(Cause::Database))
    );
    assert!(
        outcome.error.is_some(),
        "the user still reads the server's message"
    );
    let mut reading = file.job("SELECT v, count(*) FROM s WHERE v <> :other");
    body(&mut reading).parameters = vec![ParameterSpec::new("other")];
    let mut handle = engine
        .submit(file.run(reading, &[("other", "input-5a1c")]))
        .unwrap();
    assert_eq!(finished(&mut handle).await.state, RunState::Succeeded);
    let bytes = stored_bytes(&engine).await;
    for sentinel in [
        "input-4b8e",
        "input-5a1c",
        "cell-7c1f",
        "missing_table_9d2: ",
    ] {
        assert!(!contains(&bytes, sentinel), "{sentinel} was stored");
    }
    let recorded = format!("{:?}", record(&engine, handle.id).await);
    assert!(!recorded.contains("input-5a1c") && !recorded.contains("cell-7c1f"));
}

// ---------------------------------------------------------------------------
// Declared outputs (section 9.3.2, ADR-0035).

fn with_outputs(job: &mut JobDefinition, row: RowRule) {
    let body = body(job);
    body.outputs = vec![SqlOutput {
        name: "address".into(),
        kind: ParameterKind::Text,
        column: 2,
    }];
    body.row = row;
}

#[tokio::test]
async fn outputs_follow_the_row_rule_and_are_stored_only_on_opt_in() {
    let file = SqliteFile::new(&[
        "CREATE TABLE d (k TEXT, address TEXT)",
        "INSERT INTO d VALUES ('a', 'addr-0e3f'), ('b', NULL), ('b', 'x')",
    ])
    .await;
    let engine = engine().await;
    let lookup = |key: &str, row: RowRule, persist: bool| {
        let mut job = file.job("SELECT k, address FROM d WHERE k = :key ORDER BY address");
        body(&mut job).parameters = vec![ParameterSpec::new("key")];
        with_outputs(&mut job, row);
        body(&mut job).persist_outputs = persist;
        file.run(job, &[("key", key)])
    };
    // One row: its value, in memory only without opt-in.
    let mut handle = engine
        .submit(lookup("a", RowRule::ExactlyOne, false))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded);
    let values = outcome.output_values().unwrap();
    assert_eq!(values[0].value.as_deref(), Some("addr-0e3f"));
    assert_eq!(outcome.outputs_stored, None);
    assert!(
        engine
            .repository()
            .unwrap()
            .outputs(handle.id)
            .await
            .unwrap()
            .is_empty()
    );
    // Several rows under `exactly_one`: cause Output, effect known.
    let mut handle = engine
        .submit(lookup("b", RowRule::ExactlyOne, false))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(
        (outcome.state, outcome.cause, outcome.effect_unknown),
        (RunState::Failed, Some(Cause::Output), false)
    );
    // `first` takes the first row in the result's order: NULL stays NULL.
    let mut handle = engine.submit(lookup("b", RowRule::First, true)).unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.output_values().unwrap()[0].value, None);
    assert_eq!(outcome.outputs_stored, Some(true));
    // Zero rows under `at_most_one`: NULL, stored.
    let mut handle = engine
        .submit(lookup("z", RowRule::AtMostOne, true))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded);
    let stored = engine
        .repository()
        .unwrap()
        .outputs(handle.id)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert!(stored[0].is_null);
    // Opt-in with a value: stored in run_outputs, nowhere else.
    let mut handle = engine.submit(lookup("a", RowRule::First, true)).unwrap();
    assert_eq!(finished(&mut handle).await.outputs_stored, Some(true));
    let snapshot = engine
        .repository()
        .unwrap()
        .run_snapshot(handle.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!snapshot.contains("addr-0e3f"));
    assert_eq!(
        engine
            .repository()
            .unwrap()
            .outputs(handle.id)
            .await
            .unwrap()[0]
            .value
            .as_deref(),
        Some("addr-0e3f")
    );
}

// ---------------------------------------------------------------------------
// Jobs, versions and chains (sections 9.3.1–9.3.5, CA-09).

async fn saved(engine: &Engine, job: &JobDefinition) -> (JobId, i64) {
    let stored = engine.save_job(job, None).await.unwrap();
    let version = engine.publish(stored.id).await.unwrap();
    (stored.id, version.version)
}

fn chain_of(steps: Vec<ChainStep>, inputs: Vec<ParameterSpec>) -> JobDefinition {
    let mut chain = JobDefinition::new_chain("chain");
    if let JobBody::Chain(body) = &mut chain.body {
        body.steps = steps;
        body.inputs = inputs;
    }
    chain
}

/// A chain SQL → SQL on one SQLite file: a lookup whose output feeds an
/// insert.
async fn lookup_then_insert(engine: &Engine, file: &SqliteFile) -> JobDefinition {
    let mut lookup = file.job("SELECT k, address FROM d WHERE k = :key");
    lookup.name = "lookup".into();
    body(&mut lookup).parameters = vec![ParameterSpec::new("key")];
    with_outputs(&mut lookup, RowRule::ExactlyOne);
    let (lookup_id, lookup_version) = saved(engine, &lookup).await;
    let mut insert = file.job("INSERT INTO done (address, note) VALUES (:address, :note)");
    insert.name = "insert".into();
    let mut note = ParameterSpec::new("note");
    note.required = false;
    body(&mut insert).parameters = vec![ParameterSpec::new("address"), note];
    let (insert_id, insert_version) = saved(engine, &insert).await;
    let mut first = ChainStep::new("lookup", lookup_id, lookup_version);
    first
        .inputs
        .push(("key".into(), Source::Input("key".into())));
    let mut second = ChainStep::new("insert", insert_id, insert_version);
    second.inputs.push((
        "address".into(),
        Source::Step {
            step: "lookup".into(),
            output: "address".into(),
        },
    ));
    second
        .inputs
        .push(("note".into(), Source::Value("from chain".into())));
    chain_of(vec![first, second], vec![ParameterSpec::new("key")])
}

async fn chain_run(
    engine: &Engine,
    file: &SqliteFile,
    chain: &JobDefinition,
    key: &str,
) -> Arc<ChainOutcome> {
    let plan = engine.plan(chain.clone(), None).await.unwrap();
    assert_eq!(plan.problems, Vec::new());
    assert_eq!(plan.connections(), vec![file.id]);
    let mut handle = engine
        .submit_chain(ChainRequest {
            plan,
            values: HashMap::from([("key".to_owned(), key.to_owned())]),
            connections: HashMap::from([(file.id, file.connection())]),
        })
        .unwrap();
    match handle.finished().await {
        ChainStatus::Finished(outcome) => outcome,
        other => unreachable!("chain not finished: {other:?}"),
    }
}

fn chain_file_setup() -> [&'static str; 3] {
    [
        "CREATE TABLE d (k TEXT, address TEXT)",
        "INSERT INTO d VALUES ('a', 'addr-1'), ('b', 'x'), ('b', 'y')",
        "CREATE TABLE done (address TEXT, note TEXT)",
    ]
}

#[tokio::test]
async fn a_sql_output_feeds_a_later_sql_step_even_with_a_global_limit_of_one() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let limits = Limits {
        global: 1,
        ..Limits::default()
    };
    let engine = engine_in(data_dir(), limits).await;
    engine.set_connection_revisions(HashMap::from([(file.id, 1)]));
    let chain = lookup_then_insert(&engine, &file).await;
    let outcome = chain_run(&engine, &file, &chain, "a").await;
    assert_eq!(outcome.state, RunState::Succeeded, "{outcome:?}");
    assert_eq!(outcome.steps.len(), 2);
    assert_eq!(
        file.scalar(&engine, "SELECT address || '|' || note FROM done")
            .await
            .as_deref(),
        Some("addr-1|from chain")
    );
    let repository = engine.repository().unwrap();
    let recorded = repository.chain_run(outcome.id).await.unwrap().unwrap();
    assert_eq!(recorded.state, RunState::Succeeded);
    let steps = repository.chain_steps(outcome.id).await.unwrap();
    assert_eq!(
        steps
            .iter()
            .map(|step| (step.step_index, step.job_version))
            .collect::<Vec<_>>(),
        vec![(Some(0), Some(1)), (Some(1), Some(1))]
    );
}

#[tokio::test]
async fn a_failed_step_stops_the_chain_and_later_steps_do_not_run() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let engine = engine().await;
    engine.set_connection_revisions(HashMap::from([(file.id, 1)]));
    let chain = lookup_then_insert(&engine, &file).await;
    // Two rows for `b`: the lookup fails with cause Output.
    let outcome = chain_run(&engine, &file, &chain, "b").await;
    assert_eq!(
        (outcome.state, outcome.cause, outcome.failed_step.as_deref()),
        (RunState::Failed, Some(Cause::Output), Some("lookup"))
    );
    assert_eq!(outcome.steps.len(), 1);
    let steps = engine
        .repository()
        .unwrap()
        .chain_steps(outcome.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 1, "the insert never started and has no row");
    assert_eq!(
        file.scalar(&engine, "SELECT count(*) FROM done")
            .await
            .as_deref(),
        Some("0")
    );
    // No row for `z`: the lookup fails too; a NULL never reaches the insert.
    let outcome = chain_run(&engine, &file, &chain, "z").await;
    assert_eq!(outcome.cause, Some(Cause::Output));
}

#[tokio::test]
async fn a_connection_changed_after_the_plan_stops_its_step() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let engine = engine().await;
    engine.set_connection_revisions(HashMap::from([(file.id, 2)]));
    let chain = lookup_then_insert(&engine, &file).await;
    let outcome = chain_run(&engine, &file, &chain, "a").await;
    assert_eq!(
        (outcome.state, outcome.cause, outcome.failed_step.as_deref()),
        (RunState::Failed, Some(Cause::PlanChanged), Some("lookup"))
    );
    assert!(outcome.steps.is_empty());
}

#[tokio::test]
async fn a_mapped_value_that_fails_its_check_stops_the_chain_with_input() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let engine = engine().await;
    engine.set_connection_revisions(HashMap::from([(file.id, 1)]));
    let mut lookup = file.job("SELECT k, 'not a number' FROM d WHERE k = :key");
    body(&mut lookup).parameters = vec![ParameterSpec::new("key")];
    with_outputs(&mut lookup, RowRule::ExactlyOne);
    let (lookup_id, lookup_version) = saved(&engine, &lookup).await;
    let mut count = file.job("SELECT :n + 1");
    body(&mut count).parameters = vec![spec("n", ParameterKind::Number)];
    let (count_id, count_version) = saved(&engine, &count).await;
    let mut first = ChainStep::new("lookup", lookup_id, lookup_version);
    first
        .inputs
        .push(("key".into(), Source::Input("key".into())));
    let mut second = ChainStep::new("count", count_id, count_version);
    second.inputs.push((
        "n".into(),
        Source::Step {
            step: "lookup".into(),
            output: "address".into(),
        },
    ));
    let chain = chain_of(vec![first, second], vec![ParameterSpec::new("key")]);
    // A text output into a number input is refused when saving.
    assert!(matches!(
        engine.save_job(&chain, None).await,
        Err(SaveError::Chain(problems)) if matches!(problems[0], ChainProblem::KindMismatch { .. })
    ));
    let plan = engine.plan(chain, None).await.unwrap();
    assert!(!plan.problems.is_empty());
    assert!(matches!(
        engine.submit_chain(ChainRequest {
            plan,
            values: HashMap::from([("key".to_owned(), "a".to_owned())]),
            connections: HashMap::from([(file.id, file.connection())]),
        }),
        Err(Refusal::Chain(_))
    ));
}

#[tokio::test]
async fn stop_on_a_queued_step_cancels_the_chain() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let limits = Limits {
        global: 1,
        ..Limits::default()
    };
    let engine = engine_in(data_dir(), limits).await;
    engine.set_connection_revisions(HashMap::from([(file.id, 1)]));
    let chain = lookup_then_insert(&engine, &file).await;
    // A run holds the only slot.
    let driver = Scripted::new(Script::Blocks);
    let blocker = engine
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "SELECT 1"),
        ))
        .unwrap();
    let plan = engine.plan(chain, None).await.unwrap();
    let mut handle = engine
        .submit_chain(ChainRequest {
            plan,
            values: HashMap::from([("key".to_owned(), "a".to_owned())]),
            connections: HashMap::from([(file.id, file.connection())]),
        })
        .unwrap();
    eventually(Duration::from_secs(5), || {
        engine.overview().iter().any(|entry| {
            entry.step == Some(0) && matches!(entry.status, Some(RunStatus::Queued { .. }))
        })
    })
    .await;
    assert!(engine.stop_chain(handle.id));
    let ChainStatus::Finished(outcome) = handle.finished().await else {
        unreachable!("the chain ends");
    };
    assert_eq!(outcome.state, RunState::Cancelled);
    assert!(outcome.steps.is_empty(), "the queued step never ran");
    engine.stop_run(blocker.id);
    engine.shutdown().await;
}

#[tokio::test]
async fn published_versions_are_frozen_and_protected_while_a_chain_names_them() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let engine = engine().await;
    let chain = lookup_then_insert(&engine, &file).await;
    let stored_chain = engine.save_job(&chain, None).await.unwrap();
    let JobBody::Chain(body_of_chain) = &chain.body else {
        unreachable!("a chain");
    };
    let lookup = body_of_chain.steps[0].job;
    // Editing the job changes its draft only.
    let repository = engine.repository().unwrap();
    let stored = repository.job(lookup).await.unwrap().unwrap();
    let mut edited = JobDefinition::from_json(&stored.definition_json).unwrap();
    body(&mut edited).sql = "SELECT k, address FROM d WHERE k = :key LIMIT 1".into();
    engine
        .save_job(&edited, Some(stored.revision))
        .await
        .unwrap();
    let plan = engine.plan(chain.clone(), None).await.unwrap();
    assert!(plan.steps[0].changed_since);
    let JobBody::Sql(frozen) = &plan.steps[0].definition.body else {
        unreachable!("a SQL step");
    };
    assert!(!frozen.sql.contains("LIMIT"));
    // The version and the job a chain names cannot be deleted.
    assert!(matches!(
        engine.delete_version(lookup, 1).await,
        Err(SaveError::InUse(chains)) if chains[0].id == stored_chain.id
    ));
    assert!(matches!(
        engine.delete_job(lookup).await,
        Err(SaveError::InUse(_))
    ));
    engine.delete_job(stored_chain.id).await.unwrap();
    engine.delete_job(lookup).await.unwrap();
}

#[tokio::test]
async fn history_lists_runs_and_chain_runs_without_their_steps() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let engine = engine().await;
    engine.set_connection_revisions(HashMap::from([(file.id, 1)]));
    let chain = lookup_then_insert(&engine, &file).await;
    chain_run(&engine, &file, &chain, "a").await;
    file.scalar(&engine, "SELECT 1").await;
    let history = engine
        .repository()
        .unwrap()
        .history(&HistoryFilter::default())
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    assert!(matches!(history[0], HistoryEntry::Run(_)));
    assert!(matches!(history[1], HistoryEntry::Chain(_)));
    let _ = ExecutionOptions::default();
}

// ---------------------------------------------------------------------------
// History that cannot be written (section 6.3, ADR-0034 decision 3).

/// A session that holds `automation.db`'s write lock, as a failing disk
/// would refuse every write, until it is closed.
async fn hold_history(engine: &Engine) -> Box<dyn Session> {
    let input = ResolvedConnection {
        config: ConnectionConfig {
            driver: DriverKind::Sqlite,
            options: serde_json::json!({
                "path": engine.data_dir().join("automation.db").to_string_lossy()
            }),
        },
        credentials: Credentials::default(),
    };
    let mut session = SqliteDriver
        .open_session(&input, CancellationToken::new())
        .await
        .unwrap();
    exec(&mut *session, "BEGIN EXCLUSIVE").await;
    session
}

#[tokio::test]
async fn a_run_whose_history_cannot_be_written_is_refused_before_anything_is_sent() {
    let driver = Scripted::new(Script::Blocks);
    let engine = engine().await;
    let mut held = hold_history(&engine).await;
    let mut handle = engine
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "UPDATE t SET v = 1"),
        ))
        .unwrap();
    assert!(matches!(
        handle.finished().await,
        RunStatus::Refused(Refusal::History)
    ));
    assert_eq!(driver.opened.load(Ordering::SeqCst), 0, "nothing was sent");
    exec(&mut *held, "ROLLBACK").await;
    held.close().await.unwrap();
}

#[tokio::test]
async fn a_terminal_state_that_cannot_be_written_is_never_replayed() {
    let release = Arc::new(Notify::new());
    let driver = Scripted::new(Script::WaitsFor(release.clone()));
    let dir = data_dir();
    let engine = engine_in(dir.clone(), Limits::default()).await;
    let mut handle = engine
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "UPDATE t SET v = 1"),
        ))
        .unwrap();
    eventually(Duration::from_secs(5), || executing(&handle)).await;
    let mut held = hold_history(&engine).await;
    release.notify_one();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded);
    assert!(outcome.history_failed, "the user is told");
    exec(&mut *held, "ROLLBACK").await;
    held.close().await.unwrap();
    assert_eq!(record(&engine, handle.id).await.state, RunState::Running);
    engine.shutdown().await;
    drop(engine);

    // The next start marks it interrupted and never sends it again.
    let engine = engine_in(dir, Limits::default()).await;
    let recovered = record(&engine, handle.id).await;
    assert_eq!(
        (recovered.state, recovered.effect_unknown),
        (RunState::Interrupted, true)
    );
    assert_eq!(driver.opened.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// External programs through the engine (sections 9.1, 9.2, CA-07, CA-09).

use super::definition::{OutputSpec, Protocol};
use super::process::test_child;

/// A program job that runs this test binary in `mode`.
/// Its entrypoint is a small file: hashing the test binary itself would
/// take most of a test's time.
fn program_job(mode: &str, protocol: Protocol, set: &[(&str, &str)]) -> JobDefinition {
    let mut job = JobDefinition::new_process("program");
    let mut body = test_child::body(mode, protocol, set);
    body.entrypoint = Some(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("Cargo.toml")
            .to_string_lossy()
            .into_owned(),
    );
    job.body = JobBody::Process(body);
    job
}

fn program_body(job: &mut JobDefinition) -> &mut definition::ProcessBody {
    match &mut job.body {
        JobBody::Process(body) => body,
        _ => unreachable!("a program"),
    }
}

fn program_run(job: JobDefinition, values: &[(&str, &str)]) -> JobRequest {
    JobRequest {
        definition: job,
        stored: None,
        values: values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        connection: None,
    }
}

fn fast_engine_timings() -> process::Timings {
    process::Timings {
        grace: Duration::from_millis(500),
        drain: Duration::from_millis(500),
        reap: Duration::from_secs(2),
    }
}

async fn program_engine() -> Engine {
    let engine = Engine::with_limits(
        Handle::current(),
        data_dir(),
        Limits::default(),
        crate::result::DEFAULT_MEMORY_CEILING,
        fast_engine_timings(),
    );
    assert_eq!(engine.started().await, Availability::Ready);
    engine
}

#[tokio::test]
async fn a_program_run_is_recorded_with_its_exit_and_logs() {
    let engine = program_engine().await;
    let mut handle = engine
        .submit(program_run(
            program_job("print:hello-out", Protocol::None, &[]),
            &[],
        ))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded, "{outcome:?}");
    assert!(!outcome.effect_unknown);
    let program = outcome.program.as_ref().unwrap();
    assert_eq!(program.exit_code, Some(0));
    assert!(program.program_fingerprint.is_some());
    let recorded = record(&engine, handle.id).await;
    assert_eq!(
        (recorded.kind, recorded.state, recorded.exit_code),
        (JobKind::Process, RunState::Succeeded, Some(0))
    );
    assert!(recorded.log_bytes_stdout.unwrap_or(0) >= "hello-out".len() as i64);
    #[cfg(unix)]
    assert!(recorded.process_group.is_some(), "recorded for recovery");
    let log = logs::read_log(engine.data_dir(), handle.id, logs::Stream::Stdout)
        .await
        .unwrap()
        .unwrap();
    assert!(log.text.contains("hello-out"));
    // Delete logs removes the folder and the sizes.
    engine.delete_logs(handle.id).await;
    assert!(
        logs::read_log(engine.data_dir(), handle.id, logs::Stream::Stdout)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(record(&engine, handle.id).await.log_bytes_stdout, None);
}

#[tokio::test]
async fn a_nonzero_exit_fails_with_exit_and_an_unknown_effect() {
    let engine = program_engine().await;
    let mut handle = engine
        .submit(program_run(program_job("exit:3", Protocol::None, &[]), &[]))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(
        (outcome.state, outcome.cause, outcome.effect_unknown),
        (RunState::Failed, Some(Cause::Exit), true)
    );
    let recorded = record(&engine, handle.id).await;
    assert_eq!(recorded.exit_code, Some(3));
}

#[tokio::test]
async fn a_program_that_cannot_start_fails_without_an_unknown_effect() {
    let engine = program_engine().await;
    let mut job = program_job("exit:0", Protocol::None, &[]);
    program_body(&mut job).program = "jykre-no-such-program-4f1".into();
    let mut handle = engine.submit(program_run(job, &[])).unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(
        (outcome.state, outcome.cause, outcome.effect_unknown),
        (RunState::Failed, Some(Cause::NotFound), false)
    );
}

#[tokio::test]
async fn stop_on_a_program_cancels_it_with_an_unknown_effect() {
    let engine = program_engine().await;
    let mut handle = engine
        .submit(program_run(program_job("sleep", Protocol::None, &[]), &[]))
        .unwrap();
    eventually(Duration::from_secs(10), || {
        matches!(
            handle.status(),
            RunStatus::Active(Progress {
                phase: Phase::Running,
                ..
            })
        )
    })
    .await;
    assert!(engine.stop_run(handle.id));
    let outcome = finished(&mut handle).await;
    assert_eq!(
        (outcome.state, outcome.effect_unknown),
        (RunState::Cancelled, true)
    );
    assert!(outcome.program.as_ref().unwrap().stop.is_some());
}

fn echo_job(persist: bool) -> JobDefinition {
    let mut job = program_job("jjp-echo", Protocol::Jjp1, &[]);
    let body = program_body(&mut job);
    body.inputs = vec![ParameterSpec::new("address")];
    body.outputs = vec![OutputSpec {
        name: "address".into(),
        kind: ParameterKind::Text,
    }];
    body.persist_outputs = persist;
    job
}

#[tokio::test]
async fn a_jjp_program_gets_its_inputs_and_returns_its_outputs() {
    let engine = program_engine().await;
    let mut handle = engine
        .submit(program_run(echo_job(true), &[("address", "out-9b2c")]))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert_eq!(outcome.state, RunState::Succeeded, "{outcome:?}");
    assert_eq!(
        outcome.output_values().unwrap()[0].value.as_deref(),
        Some("out-9b2c")
    );
    assert_eq!(outcome.outputs_stored, Some(true));
    // A required input left empty is refused before anything starts.
    assert!(matches!(
        engine.submit(program_run(echo_job(false), &[])),
        Err(Refusal::Inputs(_))
    ));
}

#[tokio::test]
async fn a_sql_output_feeds_an_external_program_in_a_chain() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let engine = program_engine().await;
    engine.set_connection_revisions(HashMap::from([(file.id, 1)]));
    let mut lookup = file.job("SELECT k, address FROM d WHERE k = :key");
    lookup.name = "lookup".into();
    body(&mut lookup).parameters = vec![ParameterSpec::new("key")];
    with_outputs(&mut lookup, RowRule::ExactlyOne);
    let (lookup_id, lookup_version) = saved(&engine, &lookup).await;
    let (echo_id, echo_version) = saved(&engine, &echo_job(false)).await;
    let mut first = ChainStep::new("lookup", lookup_id, lookup_version);
    first
        .inputs
        .push(("key".into(), Source::Input("key".into())));
    let mut second = ChainStep::new("export", echo_id, echo_version);
    second.inputs.push((
        "address".into(),
        Source::Step {
            step: "lookup".into(),
            output: "address".into(),
        },
    ));
    let chain = chain_of(vec![first, second], vec![ParameterSpec::new("key")]);
    let outcome = chain_run(&engine, &file, &chain, "a").await;
    assert_eq!(outcome.state, RunState::Succeeded, "{outcome:?}");
    assert_eq!(
        outcome.outputs().unwrap()[0].value.as_deref(),
        Some("addr-1"),
        "the program received the SQL output"
    );
    let steps = engine
        .repository()
        .unwrap()
        .chain_steps(outcome.id)
        .await
        .unwrap();
    assert_eq!(
        steps.iter().map(|step| step.kind).collect::<Vec<_>>(),
        vec![JobKind::Sql, JobKind::Process]
    );
}

#[tokio::test]
async fn a_changed_program_file_stops_its_chain_step_until_published_again() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let engine = program_engine().await;
    engine.set_connection_revisions(HashMap::from([(file.id, 1)]));
    let entrypoint = std::env::temp_dir().join(format!("jykre-entry-{}.txt", RunId::new()));
    std::fs::write(&entrypoint, b"version one").unwrap();
    let mut program = program_job("exit:0", Protocol::None, &[]);
    program_body(&mut program).entrypoint = Some(entrypoint.to_string_lossy().into_owned());
    let (program_id, program_version) = saved(&engine, &program).await;
    let versions = engine
        .repository()
        .unwrap()
        .versions(program_id)
        .await
        .unwrap();
    let pinned = JobDefinition::from_json(&versions[0].definition_json).unwrap();
    let JobBody::Process(pinned) = pinned.body else {
        unreachable!("a program");
    };
    assert!(pinned.pinned.unwrap().sha256.starts_with("sha256:"));
    let mut lookup = file.job("SELECT k, address FROM d WHERE k = :key");
    body(&mut lookup).parameters = vec![ParameterSpec::new("key")];
    let (lookup_id, lookup_version) = saved(&engine, &lookup).await;
    let mut first = ChainStep::new("lookup", lookup_id, lookup_version);
    first
        .inputs
        .push(("key".into(), Source::Input("key".into())));
    let chain = chain_of(
        vec![first, ChainStep::new("run", program_id, program_version)],
        vec![ParameterSpec::new("key")],
    );
    assert_eq!(
        chain_run(&engine, &file, &chain, "a").await.state,
        RunState::Succeeded
    );
    std::fs::write(&entrypoint, b"version two").unwrap();
    let outcome = chain_run(&engine, &file, &chain, "a").await;
    assert_eq!(
        (outcome.state, outcome.cause, outcome.failed_step.as_deref()),
        (RunState::Failed, Some(Cause::PlanChanged), Some("run"))
    );
    // The job on its own runs its current definition.
    let mut handle = engine.submit(program_run(program, &[])).unwrap();
    assert_eq!(finished(&mut handle).await.state, RunState::Succeeded);
    let _ = std::fs::remove_file(entrypoint);
}

#[tokio::test]
async fn stop_before_a_program_starts_leaves_nothing_running() {
    let engine = program_engine().await;
    let mut handle = engine
        .submit(program_run(program_job("sleep", Protocol::None, &[]), &[]))
        .unwrap();
    // Stop at once: the run never starts its program, whichever step it
    // reached, and claims nothing about its effect.
    assert!(engine.stop_run(handle.id));
    match handle.finished().await {
        RunStatus::Refused(Refusal::Withdrawn) => {}
        RunStatus::Finished(outcome) => {
            assert_eq!(outcome.state, RunState::Cancelled);
            assert!(!outcome.effect_unknown, "{outcome:?}");
            assert!(outcome.program.as_ref().unwrap().exit_code.is_none());
        }
        other => unreachable!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn quitting_while_a_chain_step_waits_cancels_the_chain() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let limits = Limits {
        global: 1,
        ..Limits::default()
    };
    let engine = engine_in(data_dir(), limits).await;
    engine.set_connection_revisions(HashMap::from([(file.id, 1)]));
    let chain = lookup_then_insert(&engine, &file).await;
    let driver = Scripted::new(Script::Blocks);
    let _blocker = engine
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "SELECT 1"),
        ))
        .unwrap();
    let plan = engine.plan(chain, None).await.unwrap();
    let mut handle = engine
        .submit_chain(ChainRequest {
            plan,
            values: HashMap::from([("key".to_owned(), "a".to_owned())]),
            connections: HashMap::from([(file.id, file.connection())]),
        })
        .unwrap();
    eventually(Duration::from_secs(5), || {
        engine.overview().iter().any(|entry| {
            entry.step == Some(0) && matches!(entry.status, Some(RunStatus::Queued { .. }))
        })
    })
    .await;
    engine.shutdown().await;
    let ChainStatus::Finished(outcome) = handle.finished().await else {
        unreachable!("the chain ends");
    };
    assert_eq!((outcome.state, outcome.cause), (RunState::Cancelled, None));
    let recorded = engine
        .repository()
        .unwrap()
        .chain_run(outcome.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recorded.state, RunState::Cancelled);
    // Nothing can be saved once the engine shut down.
    assert_eq!(
        engine
            .save_job(&sql_job(ConnectionId::new(), "SELECT 1"), None)
            .await
            .unwrap_err(),
        SaveError::Unavailable
    );
}

#[tokio::test]
async fn try_again_twice_at_once_takes_the_lock_once_and_starts_pruning() {
    let dir = data_dir();
    let first = engine_in(dir.clone(), Limits::default()).await;
    let second = Engine::start(Handle::current(), dir);
    assert_eq!(second.started().await, Availability::ReadOnly);
    first.shutdown().await;
    drop(first);
    let (a, b) = tokio::join!(second.retry(), second.retry());
    assert!(
        a == Availability::Ready || b == Availability::Ready,
        "{a:?} {b:?}"
    );
    assert_eq!(second.availability(), Availability::Ready);
    let driver = Scripted::new(Script::Rows(Vec::new(), Vec::new()));
    let mut handle = second
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "SELECT 1"),
        ))
        .unwrap();
    assert_eq!(finished(&mut handle).await.state, RunState::Succeeded);
}

#[tokio::test]
async fn a_run_outcome_never_shows_cells_in_debug() {
    let file = SqliteFile::new(&[]).await;
    let engine = engine().await;
    let mut handle = engine
        .submit(file.run(file.job("SELECT 'cell-77aa' AS v"), &[]))
        .unwrap();
    let outcome = finished(&mut handle).await;
    assert!(!format!("{outcome:?}").contains("cell-77aa"));
    assert!(!format!("{:?}", handle.status()).contains("cell-77aa"));
}

#[tokio::test]
async fn a_connection_changed_while_its_step_waits_stops_the_chain() {
    let file = SqliteFile::new(&chain_file_setup()).await;
    let limits = Limits {
        global: 1,
        ..Limits::default()
    };
    let engine = engine_in(data_dir(), limits).await;
    engine.set_connection_revisions(HashMap::from([(file.id, 1)]));
    let chain = lookup_then_insert(&engine, &file).await;
    let driver = Scripted::new(Script::Blocks);
    let blocker = engine
        .submit(scripted_run(
            &driver,
            sql_job(ConnectionId::new(), "SELECT 1"),
        ))
        .unwrap();
    let plan = engine.plan(chain, None).await.unwrap();
    let mut handle = engine
        .submit_chain(ChainRequest {
            plan,
            values: HashMap::from([("key".to_owned(), "a".to_owned())]),
            connections: HashMap::from([(file.id, file.connection())]),
        })
        .unwrap();
    eventually(Duration::from_secs(5), || {
        engine.overview().iter().any(|entry| {
            entry.step == Some(0) && matches!(entry.status, Some(RunStatus::Queued { .. }))
        })
    })
    .await;
    engine.set_connection_revisions(HashMap::from([(file.id, 2)]));
    engine.stop_run(blocker.id);
    let ChainStatus::Finished(outcome) = handle.finished().await else {
        unreachable!("the chain ends");
    };
    assert_eq!(
        (outcome.state, outcome.cause, outcome.failed_step.as_deref()),
        (RunState::Failed, Some(Cause::PlanChanged), Some("lookup"))
    );
    assert!(outcome.steps.is_empty(), "the step never started");
}
