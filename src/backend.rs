//! Application operations, independent of the UI and its executor.
use crate::drivers::*;
use crate::logging;
use crate::result::{ResultBuilder, ResultData, ResultLimit};
use crate::tr;
use jykre_storage::{MetadataRepository, PendingRefresh, RefreshStatus};
use std::path::{Path, PathBuf};
use std::{
    future::Future,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tracing::Instrument as _;

/// How long a read from the local metadata cache is expected to take
/// ([Performance](../docs/performance.md#budgets)).
const CACHE_READ_BUDGET: Duration = Duration::from_millis(50);

pub struct MetadataSnapshot {
    pub objects: Vec<MetadataObject>,
    pub statuses: Vec<RefreshStatus>,
    pub version: i64,
    pub recovered: bool,
    pub pages: Vec<(ObjectPath, u32, bool)>,
}

pub async fn cached_metadata(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    recover: bool,
) -> DriverResult<MetadataSnapshot> {
    cached_metadata_pages(dir, id, config, recover, vec![(ObjectPath::default(), 200)]).await
}

pub async fn cached_metadata_pages(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    recover: bool,
    pages: Vec<(ObjectPath, u32)>,
) -> DriverResult<MetadataSnapshot> {
    let started = Instant::now();
    let repository = MetadataRepository::open(dir, id, config).await?;
    if recover {
        repository.recover_interrupted().await?;
    }
    let version = repository.version().await?;
    let mut objects = Vec::new();
    let mut loaded_pages = Vec::new();
    for (parent, count) in pages {
        let count = count.clamp(200, 100_000);
        let mut loaded = Vec::new();
        while loaded.len() <= count as usize {
            let take = (count + 1 - loaded.len() as u32).min(500);
            if take == 0 {
                break;
            }
            let page = repository
                .children(&parent, loaded.len() as u32, take)
                .await?;
            let end = page.len() < take as usize;
            loaded.extend(page);
            if end {
                break;
            }
        }
        let more = loaded.len() > count as usize;
        loaded.truncate(count as usize);
        loaded_pages.push((parent, count, more));
        objects.extend(loaded);
    }
    objects.sort_by(|a, b| {
        a.path
            .0
            .iter()
            .map(|part| &part.name)
            .cmp(b.path.0.iter().map(|part| &part.name))
    });
    objects.dedup_by(|a, b| a.path == b.path);
    let statuses = repository.statuses().await?;
    if repository.version().await? != version {
        repository.close().await;
        return Err(DriverError::validation(tr!(
            "The cache changed while reading. Expand the node again."
        )));
    }
    let snapshot = MetadataSnapshot {
        objects,
        statuses,
        version,
        recovered: repository.recovered,
        pages: loaded_pages,
    };
    repository.close().await;
    logging::timing("expand from cache", started, CACHE_READ_BUDGET);
    Ok(snapshot)
}

pub async fn search_metadata(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    query: &str,
) -> DriverResult<Vec<MetadataObject>> {
    let started = Instant::now();
    let repository = MetadataRepository::open(dir, id, config).await?;
    let result = repository.search(query, 200).await;
    repository.close().await;
    logging::timing("metadata search", started, CACHE_READ_BUDGET);
    result
}

struct StagedMetadata<'a> {
    pending: &'a mut PendingRefresh,
    buffered: Vec<MetadataObject>,
}
impl StagedMetadata<'_> {
    async fn flush(&mut self) -> DriverResult<()> {
        if self.buffered.is_empty() {
            return Ok(());
        }
        self.pending.batch(std::mem::take(&mut self.buffered)).await
    }
}
#[async_trait]
impl IntrospectionSink for StagedMetadata<'_> {
    async fn batch(&mut self, objects: Vec<MetadataObject>) -> DriverResult<()> {
        self.buffered.extend(objects);
        if self.buffered.len() >= 500 {
            self.flush().await?;
        }
        Ok(())
    }
}

/// Remote I/O only fills staging. Publication happens in a short transaction
/// after success; no active objects outside the selected scope are rewritten.
pub async fn refresh_metadata(
    dir: PathBuf,
    id: ConnectionId,
    input: ResolvedConnection,
    scope: ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<i64> {
    let span = tracing::info_span!(
        "refresh_metadata",
        connection_id = %id,
        driver = ?input.config.driver,
        depth = scope.0.len()
    );
    let started = Instant::now();
    let result = refresh_metadata_in(dir, id, input, scope, cancel)
        .instrument(span.clone())
        .await;
    let _entered = span.enter();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(version) => tracing::info!(elapsed_ms, version, "metadata refresh finished"),
        Err(error) if error.code == ErrorCode::Cancelled => {
            tracing::info!(elapsed_ms, "metadata refresh cancelled")
        }
        Err(error) => {
            tracing::warn!(elapsed_ms, error_code = ?error.code, "metadata refresh failed")
        }
    }
    result
}

async fn refresh_metadata_in(
    dir: PathBuf,
    id: ConnectionId,
    input: ResolvedConnection,
    scope: ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<i64> {
    let driver = builtin_registry()?.get(input.config.driver)?;
    driver.validate_connection(&input.config)?;
    let repository = MetadataRepository::open(&dir, id, &input.config).await?;
    let mut staged = repository.begin(&scope).await?;
    let mut sink = StagedMetadata {
        pending: &mut staged,
        buffered: Vec::new(),
    };
    let result = driver
        .introspect_scoped(&input, &scope, &mut sink, cancel.clone())
        .await;
    let result = match result {
        Ok(report) => sink.flush().await.map(|_| report),
        Err(error) => Err(error),
    };
    drop(sink);
    let published = match result {
        Ok(report) => staged.activate(&report, &cancel).await,
        Err(error) => Err(error),
    };
    if let Err(error) = published {
        staged.fail(&error).await?;
        repository.close().await;
        return Err(error);
    }
    let version = published?;
    repository.close().await;
    Ok(version)
}

/// The only Tokio runtime, created on first use.
fn runtime() -> DriverResult<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, std::io::Error>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(|_| DriverError::new(ErrorCode::Internal, tr!("Cannot start database runtime")))
}

/// Drive network futures on Tokio while allowing the GUI to await their result.
pub async fn run<T: Send + 'static>(
    work: impl Future<Output = DriverResult<T>> + Send + 'static,
) -> DriverResult<T> {
    runtime()?.spawn(work).await.map_err(|error| {
        // The panic hook already recorded a panic with its backtrace.
        tracing::error!(panicked = error.is_panic(), "database task failed");
        DriverError::new(ErrorCode::Internal, tr!("Database task failed"))
    })?
}

#[derive(Default)]
struct MetadataCollector(Vec<MetadataObject>);
#[async_trait]
impl IntrospectionSink for MetadataCollector {
    async fn batch(&mut self, objects: Vec<MetadataObject>) -> DriverResult<()> {
        self.0.extend(objects);
        Ok(())
    }
}

pub async fn metadata(
    input: ResolvedConnection,
    scope: ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<(Vec<MetadataObject>, IntrospectionReport)> {
    let driver = builtin_registry()?.get(input.config.driver)?;
    let mut sink = MetadataCollector::default();
    let report = driver
        .introspect_scoped(&input, &scope, &mut sink, cancel)
        .await?;
    Ok((sink.0, report))
}

/// Publish atomically after success. Warnings protect unreadable subtrees from
/// deletion, while a successful empty subtree removes objects deleted remotely.
pub fn merge_metadata(
    current: &mut Vec<MetadataObject>,
    scope: &ObjectPath,
    incoming: Vec<MetadataObject>,
    report: &IntrospectionReport,
) {
    current.retain(|object| {
        !object.path.is_within(scope)
            || report
                .warnings
                .iter()
                .any(|warning| object.path.is_within(&warning.path))
    });
    for object in incoming
        .into_iter()
        .filter(|object| object.path.is_within(scope))
    {
        current.retain(|old| old.path != object.path);
        current.push(object);
    }
    current.sort_by(|a, b| {
        a.path
            .0
            .iter()
            .map(|p| &p.name)
            .cmp(b.path.0.iter().map(|p| &p.name))
    });
}

/// One statement's result, built while the driver reads it.
#[derive(Debug, Default)]
pub struct QueryResult {
    /// Display text, stored compactly. Empty for statements without rows.
    pub data: ResultData,
    pub outcome: DriverOutcome,
    pub elapsed_ms: u128,
    /// The limit that stopped reading, if any.
    pub limit: Option<ResultLimit>,
}

pub struct QueryBatch {
    pub completed: Vec<QueryResult>,
    /// The first failed instruction follows the completed results. Earlier
    /// statements may already have committed; never imply they were rolled back.
    pub error: Option<DriverError>,
}

/// What a run leaves behind.
pub struct SessionRun {
    /// The statements' results, or why the run could not start.
    pub batch: DriverResult<QueryBatch>,
    /// The session for the tab's next run, or `None` when it was closed.
    pub session: Option<Box<dyn Session>>,
    /// The tab's session had been lost and this run opened a new one, so
    /// session state (current schema, temporary tables, a transaction)
    /// started over.
    pub reopened: bool,
    /// The session was lost during this run; the next run opens a new one.
    pub ended: bool,
}

/// Runs `requests` in order on the tab's `session`, opening one when there is
/// none, and hands the session back so state set by one run (`USE`, `SET`,
/// temporary tables, a transaction) applies to the next. A failed statement
/// keeps the session; only a session the driver could not bring back to a
/// known state is closed. A kept session is checked with [`Session::ping`]
/// before it is reused, and replaced when it no longer answers. A statement
/// is never retried.
///
/// Each result is converted to [`ResultData`] row by row as the driver reads
/// it, so driver cells are dropped here, on the runtime, and reading stops at
/// the request's row limit or at `memory_ceiling` bytes, whichever comes
/// first.
///
/// The run is logged at `info` with its duration, statement count, rows and
/// error code; SQL text only at `trace`.
pub async fn query(
    input: ResolvedConnection,
    session: Option<Box<dyn Session>>,
    requests: Vec<QueryRequest>,
    memory_ceiling: usize,
    cancel: CancellationToken,
) -> DriverResult<SessionRun> {
    let driver = match builtin_registry().and_then(|registry| registry.get(input.config.driver)) {
        Ok(driver) => driver,
        Err(error) => {
            close_session(session).await;
            return Err(error);
        }
    };
    query_with(driver, input, session, requests, memory_ceiling, cancel).await
}

/// [`query`] with a given driver.
pub async fn query_with(
    driver: Arc<dyn Driver>,
    input: ResolvedConnection,
    session: Option<Box<dyn Session>>,
    requests: Vec<QueryRequest>,
    memory_ceiling: usize,
    cancel: CancellationToken,
) -> DriverResult<SessionRun> {
    let span = tracing::info_span!(
        "query",
        driver = ?input.config.driver,
        statements = requests.len()
    );
    let started = Instant::now();
    let run = run_statements(driver, input, session, requests, memory_ceiling, cancel)
        .instrument(span.clone())
        .await;
    let _entered = span.enter();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &run.batch {
        Ok(batch) => {
            let rows: usize = batch.completed.iter().map(|r| r.data.row_count()).sum();
            let error_code = batch.error.as_ref().map(|error| error.code);
            tracing::info!(
                elapsed_ms,
                completed = batch.completed.len(),
                rows,
                error_code = ?error_code,
                reopened = run.reopened,
                ended = run.ended,
                "run finished"
            );
        }
        Err(error) => tracing::info!(elapsed_ms, error_code = ?error.code, "run failed"),
    }
    Ok(run)
}

async fn run_statements(
    driver: Arc<dyn Driver>,
    input: ResolvedConnection,
    session: Option<Box<dyn Session>>,
    requests: Vec<QueryRequest>,
    memory_ceiling: usize,
    cancel: CancellationToken,
) -> SessionRun {
    let mut run = SessionRun {
        batch: Err(DriverError::cancelled()),
        session: None,
        reopened: false,
        ended: false,
    };
    if let Some(error) = requests.iter().find_map(|request| request.validate().err()) {
        run.batch = Err(error);
        run.session = session;
        return run;
    }
    let kept = match session {
        Some(mut session) => match session.ping().await {
            Ok(()) if session.is_open() => Some(session),
            _ => {
                tracing::debug!("kept session no longer answers; opening another");
                close_session(Some(session)).await;
                run.reopened = true;
                None
            }
        },
        None => None,
    };
    let mut session = match kept {
        Some(session) => session,
        None => match driver.open_session(&input, cancel.clone()).await {
            Ok(session) => session,
            Err(error) => {
                run.batch = Err(error);
                return run;
            }
        },
    };
    let mut results = Vec::new();
    let mut failure = None;
    for (index, mut request) in requests.into_iter().enumerate() {
        tracing::trace!(statement = index, sql = %request.sql, "statement");
        let start = Instant::now();
        let mut builder = ResultBuilder::new(request.options.max_rows, memory_ceiling);
        // One row beyond the limit tells a result of exactly the limit from a
        // longer one; the builder does not keep that row.
        request.options.max_rows = request.options.max_rows.saturating_add(1);
        match session
            .execute(&request, &mut builder, cancel.clone())
            .await
        {
            Ok(outcome) => {
                let (data, limit) = builder.finish();
                tracing::debug!(
                    statement = index,
                    sql_len = request.sql.len(),
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    rows = data.row_count(),
                    bytes = data.memory_bytes(),
                    limit = ?limit,
                    "statement finished"
                );
                results.push(QueryResult {
                    data,
                    outcome,
                    elapsed_ms: start.elapsed().as_millis(),
                    limit,
                });
            }
            Err(error) => {
                tracing::debug!(
                    statement = index,
                    sql_len = request.sql.len(),
                    error_code = ?error.code,
                    "statement failed"
                );
                failure = Some(error);
                break;
            }
        }
    }
    if session.is_open() {
        run.session = Some(session);
    } else {
        run.ended = true;
        close_session(Some(session)).await;
    }
    run.batch = Ok(QueryBatch {
        completed: results,
        error: failure,
    });
    run
}

/// Closes a tab's session within a short deadline; a close failure only
/// means the server drops the connection on its own.
pub async fn close_session(session: Option<Box<dyn Session>>) {
    if let Some(session) = session
        && tokio::time::timeout(Duration::from_secs(5), session.close())
            .await
            .is_err()
    {
        tracing::debug!("session close timed out");
    }
}

/// [`close_session`] on the database runtime, for a tab that closes or
/// changes connection while the UI moves on.
pub fn close_in_background(session: Box<dyn Session>) {
    if let Ok(runtime) = runtime() {
        drop(runtime.spawn(close_session(Some(session))));
    }
}

/// Drops `value` on the database runtime's blocking pool, so freeing a large
/// result never stalls the UI thread. Without a runtime it is dropped here.
pub fn release<T: Send + 'static>(value: T) {
    match runtime() {
        Ok(runtime) => drop(runtime.spawn_blocking(move || drop(value))),
        Err(_) => drop(value),
    }
}

/// A preview of the relation at `path`, quoted by the driver's rules.
pub fn preview_sql(driver: &dyn Driver, path: &ObjectPath) -> String {
    format!("SELECT * FROM {} LIMIT 100;", driver.relation_name(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn offline_cache_pages_and_search_do_not_require_a_remote_connection() {
        let dir = std::env::temp_dir().join(format!("jykre-offline-{}", ConnectionId::new()));
        let id = ConnectionId::new();
        let config = ConnectionConfig {
            driver: DriverKind::MySql,
            options: serde_json::json!({"host":"unreachable.invalid","user":"fixture","sslMode":"disabled"}),
        };
        let repository = MetadataRepository::open(&dir, id, &config).await.unwrap();
        let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "fixture");
        let mut objects = vec![db.clone()];
        for index in 0..250 {
            objects.push(MetadataObject::new(
                ObjectKind::Table,
                &db.path,
                format!("table_{index:03}"),
            ));
        }
        let mut pending = repository.begin(&ObjectPath::default()).await.unwrap();
        pending.batch(objects).await.unwrap();
        pending
            .activate(&IntrospectionReport::default(), &CancellationToken::new())
            .await
            .unwrap();
        drop(pending);
        repository.close().await;
        let first = cached_metadata(&dir, id, &config, true).await.unwrap();
        assert_eq!(first.objects, vec![db.clone()]);
        let expanded = cached_metadata_pages(
            &dir,
            id,
            &config,
            false,
            vec![(ObjectPath::default(), 200), (db.path.clone(), 200)],
        )
        .await
        .unwrap();
        assert_eq!(expanded.objects.len(), 201);
        assert!(
            expanded
                .pages
                .iter()
                .any(|(parent, _, more)| *parent == db.path && *more)
        );
        let search = search_metadata(&dir, id, &config, "table_249")
            .await
            .unwrap();
        assert!(search.contains(&db));
        assert!(search.iter().any(|object| object.name == "table_249"));
        let expanded = cached_metadata_pages(
            &dir,
            id,
            &config,
            false,
            vec![(ObjectPath::default(), 200), (db.path.clone(), 400)],
        )
        .await
        .unwrap();
        assert_eq!(expanded.objects.len(), 251);
        assert!(!expanded.pages.iter().any(|(_, _, more)| *more));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn scoped_replace_preserves_siblings_and_removes_deleted_columns() {
        let db = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "db.a");
        let table = MetadataObject::new(ObjectKind::Table, &db.path, "a");
        let other = MetadataObject::new(ObjectKind::Table, &db.path, "ab");
        let col = MetadataObject::new(ObjectKind::Column, &table.path, "gone");
        let mut current = vec![db.clone(), table.clone(), other.clone(), col];
        merge_metadata(
            &mut current,
            &table.path,
            vec![table.clone()],
            &IntrospectionReport::default(),
        );
        assert_eq!(current.len(), 3);
        assert!(current.contains(&other));
        merge_metadata(
            &mut current,
            &table.path,
            vec![],
            &IntrospectionReport::default(),
        );
        assert_eq!(current.len(), 2);
        assert!(current.contains(&db));
    }
    #[test]
    fn permission_warning_preserves_previous_subtree() {
        let table = MetadataObject::new(ObjectKind::Table, &ObjectPath::default(), "a");
        let mut current = vec![table.clone()];
        let report = IntrospectionReport {
            warnings: vec![IntrospectionWarning {
                path: table.path.clone(),
                message: "denied".into(),
            }],
        };
        merge_metadata(&mut current, &ObjectPath::default(), vec![], &report);
        assert_eq!(current, vec![table]);
    }
    #[test]
    fn preview_quotes_structural_names() {
        let path = ObjectPath::default()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Schema, "a.b")
            .child(ObjectKind::Table, "t\"x");
        assert_eq!(
            preview_sql(&PostgresDriver, &path),
            "SELECT * FROM \"a.b\".\"t\"\"x\" LIMIT 100;"
        );
        let path = ObjectPath::default()
            .child(ObjectKind::Database, "d`b")
            .child(ObjectKind::Table, "orders")
            .child(ObjectKind::Column, "id");
        assert_eq!(
            preview_sql(&MySqlDriver, &path),
            "SELECT * FROM `d``b`.`orders` LIMIT 100;"
        );
        let path = ObjectPath::default()
            .child(ObjectKind::Catalog, "cat")
            .child(ObjectKind::Schema, "sch")
            .child(ObjectKind::Table, "a`b");
        assert_eq!(
            preview_sql(&DatabricksDriver, &path),
            "SELECT * FROM `cat`.`sch`.`a``b` LIMIT 100;"
        );
    }

    /// Answers every statement with `rows` rows, honouring the row limit and
    /// the sink like a driver, without a database.
    #[derive(Debug)]
    struct EchoDriver {
        rows: u32,
    }

    #[derive(Debug)]
    struct EchoSession {
        rows: u32,
    }

    #[async_trait]
    impl Session for EchoSession {
        async fn execute(
            &mut self,
            request: &QueryRequest,
            sink: &mut dyn RowSink,
            _: CancellationToken,
        ) -> DriverResult<DriverOutcome> {
            sink.columns(vec![ColumnInfo {
                name: "value".into(),
                data_type: "text".into(),
                nullable: Some(false),
            }]);
            for _ in 0..self.rows.min(request.options.max_rows) {
                if sink.row(vec![Cell::Text("synthetic-cell-value".into())]) == SinkControl::Stop {
                    break;
                }
            }
            Ok(DriverOutcome::default())
        }

        async fn close(self: Box<Self>) -> DriverResult<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl Driver for EchoDriver {
        fn descriptor(&self) -> DriverDescriptor {
            PostgresDriver.descriptor()
        }
        fn validate_config(&self, _: &serde_json::Value) -> DriverResult<()> {
            Ok(())
        }
        async fn test(
            &self,
            _: &ResolvedConnection,
            _: CancellationToken,
        ) -> DriverResult<ServerInfo> {
            Ok(ServerInfo { version: None })
        }
        async fn open_session(
            &self,
            _: &ResolvedConnection,
            _: CancellationToken,
        ) -> DriverResult<Box<dyn Session>> {
            Ok(Box::new(EchoSession { rows: self.rows }))
        }
        async fn introspect(
            &self,
            _: &ResolvedConnection,
            _: &mut dyn IntrospectionSink,
            _: CancellationToken,
        ) -> DriverResult<IntrospectionReport> {
            Ok(IntrospectionReport::default())
        }
    }

    async fn logged_run(level: tracing::Level) -> String {
        let (subscriber, captured) = crate::logging::capture::subscriber(level);
        let _default = tracing::subscriber::set_default(subscriber);
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({"host":"localhost","user":"fixture","database":"fixture","sslMode":"disable"}),
            },
            credentials: Credentials {
                password: Some(SecretString::from("synthetic-password-1234")),
            },
        };
        let requests = ["SELECT 'synthetic-sql-marker'", "SELECT 2"]
            .into_iter()
            .map(|sql| QueryRequest {
                sql: sql.into(),
                options: ExecutionOptions::default(),
            })
            .collect();
        let batch = query_with(
            Arc::new(EchoDriver { rows: 1 }),
            input,
            None,
            requests,
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .batch
        .unwrap();
        assert_eq!(batch.completed.len(), 2);
        captured.text()
    }

    #[tokio::test]
    async fn runs_never_log_passwords_cell_values_or_sql_above_trace() {
        let text = logged_run(tracing::Level::DEBUG).await;
        assert!(text.contains("run finished"), "{text}");
        assert!(text.contains("elapsed_ms"), "{text}");
        for secret in [
            "synthetic-password-1234",
            "synthetic-sql-marker",
            "synthetic-cell-value",
        ] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        // At trace the SQL text appears, so the capture does see it; the
        // password and values still do not.
        let text = logged_run(tracing::Level::TRACE).await;
        assert!(text.contains("synthetic-sql-marker"), "{text}");
        assert!(!text.contains("synthetic-password-1234"), "{text}");
        assert!(!text.contains("synthetic-cell-value"), "{text}");
    }

    async fn rows_and_limit(available: u32, max_rows: u32) -> (usize, Option<ResultLimit>) {
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({}),
            },
            credentials: Credentials::default(),
        };
        let request = QueryRequest {
            sql: "SELECT 1".into(),
            options: ExecutionOptions {
                max_rows,
                ..ExecutionOptions::default()
            },
        };
        let mut batch = query_with(
            Arc::new(EchoDriver { rows: available }),
            input,
            None,
            vec![request],
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .batch
        .unwrap();
        let result = batch.completed.remove(0);
        (result.data.row_count(), result.limit)
    }

    #[tokio::test]
    async fn only_rows_beyond_the_limit_mark_it_as_reached() {
        assert_eq!(rows_and_limit(2, 3).await, (2, None));
        assert_eq!(rows_and_limit(3, 3).await, (3, None));
        assert_eq!(rows_and_limit(4, 3).await, (3, Some(ResultLimit::Rows(3))));
        assert_eq!(rows_and_limit(50, 3).await, (3, Some(ResultLimit::Rows(3))));
    }

    /// A driver whose sessions remember `SET` statements, so a test can tell
    /// whether a later run reused the session.
    #[derive(Debug, Default)]
    struct StateDriver {
        opened: std::sync::atomic::AtomicUsize,
        /// Sessions stop answering pings, as after an idle disconnect.
        dead: std::sync::atomic::AtomicBool,
    }

    #[derive(Debug)]
    struct StateSession {
        id: usize,
        settings: Vec<String>,
        open: bool,
        dead: bool,
    }

    #[async_trait]
    impl Session for StateSession {
        async fn execute(
            &mut self,
            request: &QueryRequest,
            sink: &mut dyn RowSink,
            _: CancellationToken,
        ) -> DriverResult<DriverOutcome> {
            match request.sql.as_str() {
                "FAIL" => return Err(DriverError::new(ErrorCode::Database, "synthetic")),
                "LOSE" => {
                    self.open = false;
                    return Err(DriverError::new(ErrorCode::Connection, "synthetic"));
                }
                sql if sql.starts_with("SET ") => self.settings.push(sql[4..].to_owned()),
                _ => {}
            }
            sink.columns(vec![ColumnInfo {
                name: "state".into(),
                data_type: "text".into(),
                nullable: Some(false),
            }]);
            sink.row(vec![Cell::Text(format!(
                "{}:{}",
                self.id,
                self.settings.join(",")
            ))]);
            Ok(DriverOutcome::default())
        }

        fn is_open(&self) -> bool {
            self.open
        }

        async fn ping(&mut self) -> DriverResult<()> {
            if self.dead {
                self.open = false;
                return Err(DriverError::new(ErrorCode::Connection, "synthetic"));
            }
            Ok(())
        }

        async fn close(self: Box<Self>) -> DriverResult<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl Driver for StateDriver {
        fn descriptor(&self) -> DriverDescriptor {
            PostgresDriver.descriptor()
        }
        fn validate_config(&self, _: &serde_json::Value) -> DriverResult<()> {
            Ok(())
        }
        async fn test(
            &self,
            _: &ResolvedConnection,
            _: CancellationToken,
        ) -> DriverResult<ServerInfo> {
            Ok(ServerInfo { version: None })
        }
        async fn open_session(
            &self,
            _: &ResolvedConnection,
            _: CancellationToken,
        ) -> DriverResult<Box<dyn Session>> {
            use std::sync::atomic::Ordering;
            Ok(Box::new(StateSession {
                id: self.opened.fetch_add(1, Ordering::SeqCst) + 1,
                settings: Vec::new(),
                open: true,
                dead: self.dead.load(Ordering::SeqCst),
            }))
        }
        async fn introspect(
            &self,
            _: &ResolvedConnection,
            _: &mut dyn IntrospectionSink,
            _: CancellationToken,
        ) -> DriverResult<IntrospectionReport> {
            Ok(IntrospectionReport::default())
        }
    }

    /// Runs `statements` on `session` and returns the run with each completed
    /// statement's single cell.
    async fn run_on(
        driver: &Arc<StateDriver>,
        session: Option<Box<dyn Session>>,
        statements: &[&str],
    ) -> (SessionRun, Vec<String>) {
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({}),
            },
            credentials: Credentials::default(),
        };
        let requests = statements
            .iter()
            .map(|sql| QueryRequest {
                sql: (*sql).into(),
                options: ExecutionOptions::default(),
            })
            .collect();
        let mut run = query_with(
            driver.clone(),
            input,
            session,
            requests,
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let cells = match &mut run.batch {
            Ok(batch) => batch
                .completed
                .iter()
                .map(|result| result.data.value(0, 0).unwrap_or_default().to_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        (run, cells)
    }

    #[tokio::test]
    async fn a_tab_session_keeps_its_state_between_runs_and_after_errors() {
        let driver = Arc::new(StateDriver::default());
        let (run, cells) = run_on(&driver, None, &["SET schema=sales", "SELECT"]).await;
        assert_eq!(cells, ["1:schema=sales", "1:schema=sales"]);
        assert!(!run.reopened && !run.ended);
        // A failed statement stops the run but keeps the session.
        let (run, cells) = run_on(&driver, run.session, &["SET role=x", "FAIL", "SELECT"]).await;
        assert_eq!(cells, ["1:schema=sales,role=x"]);
        assert_eq!(
            run.batch.as_ref().unwrap().error.as_ref().unwrap().code,
            ErrorCode::Database
        );
        let (run, cells) = run_on(&driver, run.session, &["SELECT"]).await;
        assert_eq!(cells, ["1:schema=sales,role=x"]);
        assert_eq!(driver.opened.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A lost connection ends the session; the next run starts over.
        let (run, _) = run_on(&driver, run.session, &["LOSE"]).await;
        assert!(run.ended && run.session.is_none());
        let (run, cells) = run_on(&driver, run.session, &["SELECT"]).await;
        assert_eq!(cells, ["2:"]);
        assert!(!run.reopened);
        assert!(run.session.is_some());
    }

    #[tokio::test]
    async fn a_session_that_stopped_answering_is_replaced_without_retrying() {
        let driver = Arc::new(StateDriver::default());
        driver.dead.store(true, std::sync::atomic::Ordering::SeqCst);
        let (run, _) = run_on(&driver, None, &["SET schema=sales"]).await;
        driver
            .dead
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let (run, cells) = run_on(&driver, run.session, &["SELECT"]).await;
        assert!(run.reopened, "the dead session was replaced");
        assert_eq!(cells, ["2:"], "state starts over in the new session");
        // An invalid request runs nothing and hands the session back.
        let invalid = QueryRequest {
            sql: String::new(),
            options: ExecutionOptions::default(),
        };
        assert!(invalid.validate().is_err());
        let (run, cells) = run_on(&driver, run.session, &[""]).await;
        assert!(run.batch.is_err() && cells.is_empty());
        let (_, cells) = run_on(&driver, run.session, &["SELECT"]).await;
        assert_eq!(cells, ["2:"]);
    }
}
