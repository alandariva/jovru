//! Application operations, independent of the UI and its executor.
use crate::drivers::*;
use crate::logging;
use crate::metadata_scope::{EtaEstimator, MetadataScope, RefreshPhase, RefreshProgress};
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
    /// Objects the driver delivered.
    received: usize,
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
        self.received += objects.len();
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
    let result = async {
        let driver = builtin_registry()?.get(input.config.driver)?;
        refresh_metadata_in(&*driver, &dir, id, &input, &scope, &cancel)
            .await
            .map(|(version, _)| version)
    }
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

/// Reads `scope` and publishes it. Returns the new cache version and how
/// many objects the driver delivered (none when the scope no longer exists).
async fn refresh_metadata_in(
    driver: &dyn Driver,
    dir: &Path,
    id: ConnectionId,
    input: &ResolvedConnection,
    scope: &ObjectPath,
    cancel: &CancellationToken,
) -> DriverResult<(i64, usize)> {
    driver.validate_connection(&input.config)?;
    let repository = MetadataRepository::open(dir, id, &input.config).await?;
    let mut staged = repository.begin(scope).await?;
    let mut sink = StagedMetadata {
        pending: &mut staged,
        buffered: Vec::new(),
        received: 0,
    };
    let result = driver
        .introspect_scoped(input, scope, &mut sink, cancel.clone())
        .await;
    let result = match result {
        Ok(report) => sink.flush().await.map(|_| report),
        Err(error) => Err(error),
    };
    let received = sink.received;
    drop(sink);
    let published = match result {
        Ok(report) => staged.activate(&report, cancel).await,
        Err(error) => Err(error),
    };
    if let Err(error) = published {
        staged.fail(&error).await?;
        drop(staged);
        repository.close().await;
        return Err(error);
    }
    let version = published?;
    drop(staged);
    repository.close().await;
    Ok((version, received))
}

/// What reading a choice of namespaces did. Each namespace is published on
/// its own, so a failure or a cancellation keeps the ones already read.
#[derive(Debug, Default)]
pub struct SelectionOutcome {
    /// Namespaces read and published.
    pub refreshed: usize,
    /// Namespaces (or lists of namespaces) that could not be read; their
    /// previous cache stays.
    pub failures: Vec<(ObjectPath, DriverError)>,
    /// Chosen namespaces the server no longer has.
    pub missing: Vec<ObjectPath>,
    pub cancelled: bool,
}

/// The last-level namespaces a choice resolves to, listing broader rules on
/// the way.
#[derive(Default)]
struct Plan {
    /// Last-level namespaces to read, in order.
    units: Vec<ObjectPath>,
    /// Broader namespaces whose list was read in full.
    listed: Vec<ObjectPath>,
    /// Broader namespaces whose list failed: their cache is kept as it is.
    unlisted: Vec<ObjectPath>,
}

impl Plan {
    async fn resolve(
        &mut self,
        driver: &dyn Driver,
        input: &ResolvedConnection,
        levels: usize,
        rule: &ObjectPath,
        cancel: &CancellationToken,
        failures: &mut Vec<(ObjectPath, DriverError)>,
    ) -> DriverResult<()> {
        let mut pending = vec![rule.clone()];
        while let Some(path) = pending.pop() {
            if cancel.is_cancelled() {
                return Err(DriverError::cancelled());
            }
            if path.0.len() >= levels {
                self.units.push(path);
                continue;
            }
            match driver
                .discover_namespaces(input, &path, cancel.clone())
                .await
            {
                Ok(children) => {
                    if !path.0.is_empty() {
                        self.listed.push(path);
                    }
                    // Depth first, in name order.
                    pending.extend(children.into_iter().rev().map(|child| child.path));
                }
                Err(error) if error.code == ErrorCode::Cancelled => return Err(error),
                Err(error) => {
                    self.unlisted.push(path.clone());
                    failures.push((path, error));
                }
            }
        }
        Ok(())
    }
}

/// Reads the `read` part of a connection's `selection` (all of it, or only
/// what a change added) and then removes from the cache what the selection
/// no longer covers. Rules above the last namespace level are listed first
/// (`RefreshPhase::Discovering`); then each last-level namespace is read and
/// published on its own, and `progress` receives the count and an estimate
/// of the time left after each one. Cancelling stops between namespaces
/// and keeps what was published; nothing is removed then.
pub async fn apply_selection(
    dir: PathBuf,
    id: ConnectionId,
    input: ResolvedConnection,
    selection: MetadataScope,
    read: MetadataScope,
    cancel: CancellationToken,
    progress: Option<tokio::sync::mpsc::UnboundedSender<RefreshProgress>>,
) -> DriverResult<SelectionOutcome> {
    let driver = builtin_registry()?.get(input.config.driver)?;
    let target = SelectionTarget {
        dir: &dir,
        id,
        input: &input,
        selection: &selection,
        read: &read,
    };
    apply_selection_with(&*driver, target, &cancel, progress).await
}

/// The connection and choice [`apply_selection`] works on.
struct SelectionTarget<'a> {
    dir: &'a Path,
    id: ConnectionId,
    input: &'a ResolvedConnection,
    selection: &'a MetadataScope,
    read: &'a MetadataScope,
}

async fn apply_selection_with(
    driver: &dyn Driver,
    target: SelectionTarget<'_>,
    cancel: &CancellationToken,
    progress: Option<tokio::sync::mpsc::UnboundedSender<RefreshProgress>>,
) -> DriverResult<SelectionOutcome> {
    let SelectionTarget {
        dir,
        id,
        input,
        selection,
        read,
    } = target;
    let span = tracing::info_span!(
        "apply_selection",
        connection_id = %id,
        driver = ?input.config.driver,
    );
    let started = Instant::now();
    let send = |update: RefreshProgress| {
        if let Some(progress) = &progress {
            let _ = progress.send(update);
        }
    };
    let result = async {
        driver.validate_connection(&input.config)?;
        let levels = driver.descriptor().capabilities.namespace_levels().len();
        let mut outcome = SelectionOutcome::default();
        let rules = if read.all {
            vec![ObjectPath::default()]
        } else {
            read.include.clone()
        };
        let mut plan = Plan::default();
        if !rules.is_empty() {
            send(RefreshProgress::discovering());
        }
        for rule in &rules {
            if let Err(error) = plan
                .resolve(driver, input, levels, rule, cancel, &mut outcome.failures)
                .await
            {
                if error.code == ErrorCode::Cancelled {
                    outcome.cancelled = true;
                    return Ok(outcome);
                }
                return Err(error);
            }
        }
        let total = plan.units.len();
        let mut eta = EtaEstimator::default();
        for (done, unit) in plan.units.iter().enumerate() {
            if cancel.is_cancelled() {
                outcome.cancelled = true;
                return Ok(outcome);
            }
            send(RefreshProgress {
                phase: RefreshPhase::Reading,
                done,
                total,
                current: Some(unit.clone()),
                eta: eta.remaining(total - done),
            });
            let unit_started = Instant::now();
            match refresh_metadata_in(driver, dir, id, input, unit, cancel).await {
                Ok((_, 0)) => outcome.missing.push(unit.clone()),
                Ok(_) => outcome.refreshed += 1,
                Err(error) if error.code == ErrorCode::Cancelled => {
                    outcome.cancelled = true;
                    return Ok(outcome);
                }
                Err(error) => outcome.failures.push((unit.clone(), error)),
            }
            eta.record(unit_started.elapsed());
        }
        if total > 0 {
            send(RefreshProgress {
                phase: RefreshPhase::Reading,
                done: total,
                total,
                current: None,
                eta: Some(Duration::ZERO),
            });
        }
        // What the cache keeps: each rule as it is, except the broad rules
        // listed in full here, which keep only the namespaces still on the
        // server.
        let (mut subtrees, mut nodes) = (Vec::new(), Vec::new());
        let kept_rules = if selection.all {
            vec![ObjectPath::default()]
        } else {
            selection.include.clone()
        };
        for rule in kept_rules {
            let listed_in_full = (rule.0.is_empty() && read.all && plan.unlisted.is_empty())
                || plan.listed.contains(&rule)
                    && !plan.unlisted.iter().any(|path| path.is_within(&rule));
            if listed_in_full {
                subtrees.extend(
                    plan.units
                        .iter()
                        .filter(|unit| unit.is_within(&rule))
                        .cloned(),
                );
                nodes.extend(
                    plan.listed
                        .iter()
                        .filter(|path| path.is_within(&rule))
                        .cloned(),
                );
            } else {
                subtrees.push(rule);
            }
        }
        let repository = MetadataRepository::open(dir, id, &input.config).await?;
        let retained = repository.retain(&subtrees, &nodes).await;
        repository.close().await;
        retained?;
        Ok(outcome)
    }
    .instrument(span.clone())
    .await;
    let _entered = span.enter();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(outcome) => tracing::info!(
            elapsed_ms,
            refreshed = outcome.refreshed,
            failed = outcome.failures.len(),
            missing = outcome.missing.len(),
            cancelled = outcome.cancelled,
            "metadata selection applied"
        ),
        Err(error) => {
            tracing::warn!(elapsed_ms, error_code = ?error.code, "metadata selection failed")
        }
    }
    result
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
    /// More rows are readable with [`fetch`] until the session runs
    /// anything else.
    pub has_more: bool,
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
///
/// `progress`, when given, receives the position of each statement as it is
/// sent to the server, so the UI can tell which one is running.
pub async fn query(
    input: ResolvedConnection,
    session: Option<Box<dyn Session>>,
    requests: Vec<QueryRequest>,
    memory_ceiling: usize,
    cancel: CancellationToken,
    progress: Option<Progress>,
) -> DriverResult<SessionRun> {
    let driver = match builtin_registry().and_then(|registry| registry.get(input.config.driver)) {
        Ok(driver) => driver,
        Err(error) => {
            close_session(session).await;
            return Err(error);
        }
    };
    query_with(
        driver,
        input,
        session,
        requests,
        memory_ceiling,
        cancel,
        progress,
    )
    .await
}

/// Positions of the statements of a run, sent as each one starts.
pub type Progress = tokio::sync::mpsc::UnboundedSender<usize>;

/// [`query`] with a given driver.
pub async fn query_with(
    driver: Arc<dyn Driver>,
    input: ResolvedConnection,
    session: Option<Box<dyn Session>>,
    requests: Vec<QueryRequest>,
    memory_ceiling: usize,
    cancel: CancellationToken,
    progress: Option<Progress>,
) -> DriverResult<SessionRun> {
    let span = tracing::info_span!(
        "query",
        driver = ?input.config.driver,
        statements = requests.len()
    );
    let started = Instant::now();
    let run = run_statements(
        driver,
        input,
        session,
        requests,
        memory_ceiling,
        cancel,
        progress,
    )
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
    progress: Option<Progress>,
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
    for (index, request) in requests.into_iter().enumerate() {
        tracing::trace!(statement = index, sql = %request.sql, "statement");
        if let Some(progress) = &progress {
            // The UI may be gone; the run goes on regardless.
            let _ = progress.send(index);
        }
        let start = Instant::now();
        let mut builder = ResultBuilder::new(request.options.max_rows, memory_ceiling);
        match session
            .execute(&request, &mut builder, cancel.clone())
            .await
        {
            Ok(outcome) => {
                let (data, limit) = page_limit(builder, &outcome, request.options.max_rows);
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
                    has_more: outcome.has_more,
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

/// The page's data and the limit that ended it: the memory ceiling when the
/// builder stopped reading, the row limit when the driver says rows are left
/// (readable with [`fetch`]) or that it could not read them.
fn page_limit(
    builder: ResultBuilder,
    outcome: &DriverOutcome,
    max_rows: u32,
) -> (ResultData, Option<ResultLimit>) {
    let (data, limit) = builder.finish();
    let limit =
        limit.or((outcome.has_more || outcome.truncated).then_some(ResultLimit::Rows(max_rows)));
    (data, limit)
}

/// What reading another page leaves behind.
pub struct PageRun {
    pub page: DriverResult<QueryResult>,
    /// The session for the tab's next run, or `None` when it was closed.
    pub session: Option<Box<dyn Session>>,
}

/// Reads the next page of the result the tab's session left open, with the
/// columns of its first page. Running anything else on the session first
/// discards that result.
pub async fn fetch(
    mut session: Box<dyn Session>,
    columns: Vec<crate::result::ResultColumn>,
    options: ExecutionOptions,
    memory_ceiling: usize,
    cancel: CancellationToken,
) -> DriverResult<PageRun> {
    let start = Instant::now();
    let mut builder = ResultBuilder::page(columns, options.max_rows, memory_ceiling);
    let page = session
        .fetch(&options, &mut builder, cancel)
        .await
        .map(|outcome| {
            let (data, limit) = page_limit(builder, &outcome, options.max_rows);
            tracing::debug!(
                rows = data.row_count(),
                bytes = data.memory_bytes(),
                "page read"
            );
            QueryResult {
                has_more: outcome.has_more,
                data,
                outcome,
                elapsed_ms: start.elapsed().as_millis(),
                limit,
            }
        });
    let session = if session.is_open() {
        Some(session)
    } else {
        close_session(Some(session)).await;
        None
    };
    Ok(PageRun { page, session })
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
                    return Ok(DriverOutcome::default());
                }
            }
            // Like a driver: the rows left past the limit stay readable.
            Ok(DriverOutcome {
                has_more: self.rows > request.options.max_rows,
                ..DriverOutcome::default()
            })
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
            None,
        )
        .await
        .unwrap()
        .batch
        .unwrap();
        assert_eq!(batch.completed.len(), 2);
        captured.text()
    }

    #[tokio::test]
    async fn progress_reports_each_statement_as_it_starts() {
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({"host":"localhost","user":"fixture","database":"fixture","sslMode":"disable"}),
            },
            credentials: Credentials::default(),
        };
        let requests = ["SELECT 1", "SELECT 2", "SELECT 3"]
            .into_iter()
            .map(|sql| QueryRequest {
                sql: sql.into(),
                options: ExecutionOptions::default(),
            })
            .collect();
        let (progress, mut started) = tokio::sync::mpsc::unbounded_channel();
        let run = query_with(
            Arc::new(EchoDriver { rows: 1 }),
            input,
            None,
            requests,
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
            Some(progress),
        )
        .await
        .unwrap();
        assert_eq!(run.batch.unwrap().completed.len(), 3);
        let mut positions = Vec::new();
        while let Ok(position) = started.try_recv() {
            positions.push(position);
        }
        assert_eq!(positions, [0, 1, 2]);
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
            None,
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
        /// Rows of the open result not read yet (`ROWS n` opens one).
        pending: u32,
        next: u32,
    }

    impl StateSession {
        fn page(&mut self, max_rows: u32, sink: &mut dyn RowSink) -> DriverOutcome {
            let count = self.pending.min(max_rows);
            for _ in 0..count {
                self.next += 1;
                sink.row(vec![Cell::Text(self.next.to_string())]);
            }
            self.pending -= count;
            DriverOutcome {
                has_more: self.pending > 0,
                ..DriverOutcome::default()
            }
        }
    }

    #[async_trait]
    impl Session for StateSession {
        async fn execute(
            &mut self,
            request: &QueryRequest,
            sink: &mut dyn RowSink,
            _: CancellationToken,
        ) -> DriverResult<DriverOutcome> {
            // A new statement discards the open result.
            self.pending = 0;
            if let Some(rows) = request.sql.strip_prefix("ROWS ") {
                sink.columns(vec![ColumnInfo {
                    name: "n".into(),
                    data_type: "int".into(),
                    nullable: Some(false),
                }]);
                self.pending = rows.parse().unwrap_or(0);
                self.next = 0;
                return Ok(self.page(request.options.max_rows, sink));
            }
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

        async fn fetch(
            &mut self,
            options: &ExecutionOptions,
            sink: &mut dyn RowSink,
            _: CancellationToken,
        ) -> DriverResult<DriverOutcome> {
            if self.pending == 0 {
                return Err(DriverError::validation("No result has more rows to read"));
            }
            Ok(self.page(options.max_rows, sink))
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
                pending: 0,
                next: 0,
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
            None,
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

    #[tokio::test]
    async fn the_next_page_comes_from_the_open_result() {
        let driver = Arc::new(StateDriver::default());
        let options = ExecutionOptions {
            max_rows: 2,
            ..ExecutionOptions::default()
        };
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({}),
            },
            credentials: Credentials::default(),
        };
        let run = query_with(
            driver.clone(),
            input,
            None,
            vec![QueryRequest {
                sql: "ROWS 5".into(),
                options: options.clone(),
            }],
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        let first = &run.batch.as_ref().unwrap().completed[0];
        assert!(first.has_more);
        assert_eq!(first.limit, Some(ResultLimit::Rows(2)));
        let columns = first.data.columns().to_vec();
        let mut session = run.session.unwrap();
        let mut pages = vec![first.data.value(1, 0).unwrap().to_owned()];
        loop {
            let page = fetch(
                session,
                columns.clone(),
                options.clone(),
                crate::result::DEFAULT_MEMORY_CEILING,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            session = page.session.unwrap();
            let page = page.page.unwrap();
            pages.push(
                page.data
                    .value(page.data.row_count() - 1, 0)
                    .unwrap()
                    .to_owned(),
            );
            assert_eq!(page.data.columns(), columns.as_slice());
            if !page.has_more {
                assert_eq!(page.limit, None);
                break;
            }
        }
        // Pages of 2, 2 and 1 rows, read in order without running the
        // statement again.
        assert_eq!(pages, ["2", "4", "5"]);
        let done = fetch(
            session,
            columns,
            options,
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(done.page.unwrap_err().code, ErrorCode::Validation);
        assert!(done.session.is_some());
    }

    /// A two-level server (catalogs, then schemas) kept in memory: each
    /// schema holds one table. It records what it was asked to read.
    #[derive(Debug, Default)]
    struct CatalogDriver {
        server: std::sync::Mutex<Vec<(&'static str, Vec<&'static str>)>>,
        read: std::sync::Mutex<Vec<ObjectPath>>,
        failing: Option<&'static str>,
        cancel_after: Option<(usize, CancellationToken)>,
    }

    impl CatalogDriver {
        fn new(server: Vec<(&'static str, Vec<&'static str>)>) -> Self {
            Self {
                server: std::sync::Mutex::new(server),
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl Driver for CatalogDriver {
        fn descriptor(&self) -> DriverDescriptor {
            DatabricksDriver.descriptor()
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
            Err(DriverError::validation("no SQL here"))
        }
        async fn introspect(
            &self,
            _: &ResolvedConnection,
            _: &mut dyn IntrospectionSink,
            _: CancellationToken,
        ) -> DriverResult<IntrospectionReport> {
            Err(DriverError::validation(
                "a selection never reads everything at once",
            ))
        }
        async fn discover_namespaces(
            &self,
            _: &ResolvedConnection,
            parent: &ObjectPath,
            _: CancellationToken,
        ) -> DriverResult<Vec<MetadataObject>> {
            let server = self.server.lock().unwrap();
            Ok(match parent.0.as_slice() {
                [] => server
                    .iter()
                    .map(|(name, _)| MetadataObject::new(ObjectKind::Catalog, parent, *name))
                    .collect(),
                [catalog] => server
                    .iter()
                    .find(|(name, _)| *name == catalog.name)
                    .ok_or_else(|| DriverError::new(ErrorCode::NotFound, "no catalog"))?
                    .1
                    .iter()
                    .map(|name| MetadataObject::new(ObjectKind::Schema, parent, *name))
                    .collect(),
                _ => Vec::new(),
            })
        }
        async fn introspect_scoped(
            &self,
            _: &ResolvedConnection,
            scope: &ObjectPath,
            sink: &mut dyn IntrospectionSink,
            _: CancellationToken,
        ) -> DriverResult<IntrospectionReport> {
            let count = {
                let mut read = self.read.lock().unwrap();
                read.push(scope.clone());
                read.len()
            };
            if let Some((after, cancel)) = &self.cancel_after
                && count > *after
            {
                cancel.cancel();
                return Err(DriverError::cancelled());
            }
            let [catalog, schema] = scope.0.as_slice() else {
                return Err(DriverError::validation("schemas only"));
            };
            if self.failing == Some(schema.name.as_str()) {
                return Err(DriverError::new(ErrorCode::Connection, "connection lost"));
            }
            let exists = self.server.lock().unwrap().iter().any(|(name, schemas)| {
                *name == catalog.name && schemas.contains(&schema.name.as_str())
            });
            if exists {
                let parent = scope.parent().unwrap();
                sink.batch(vec![
                    MetadataObject::new(ObjectKind::Schema, &parent, schema.name.clone()),
                    MetadataObject::new(ObjectKind::Table, scope, "t"),
                ])
                .await?;
            }
            Ok(IntrospectionReport::default())
        }
    }

    fn catalog_input() -> ResolvedConnection {
        ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Databricks,
                options: serde_json::json!({"profile":"FIXTURE"}),
            },
            credentials: Credentials::default(),
        }
    }

    fn schema(catalog: &str, schema: &str) -> ObjectPath {
        ObjectPath::default()
            .child(ObjectKind::Catalog, catalog)
            .child(ObjectKind::Schema, schema)
    }

    async fn apply(
        driver: &CatalogDriver,
        dir: &Path,
        id: ConnectionId,
        selection: &MetadataScope,
        read: &MetadataScope,
        cancel: &CancellationToken,
    ) -> (SelectionOutcome, Vec<RefreshProgress>) {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let input = catalog_input();
        let target = SelectionTarget {
            dir,
            id,
            input: &input,
            selection,
            read,
        };
        let outcome = apply_selection_with(driver, target, cancel, Some(sender))
            .await
            .unwrap();
        let mut updates = Vec::new();
        while let Ok(update) = receiver.try_recv() {
            updates.push(update);
        }
        (outcome, updates)
    }

    async fn cached(dir: &Path, id: ConnectionId) -> Vec<ObjectPath> {
        let repository = MetadataRepository::open(dir, id, &catalog_input().config)
            .await
            .unwrap();
        let mut paths: Vec<_> = repository
            .all()
            .await
            .unwrap()
            .into_iter()
            .map(|object| object.path)
            .collect();
        repository.close().await;
        paths.sort_by(|a, b| {
            a.0.iter()
                .map(|part| &part.name)
                .cmp(b.0.iter().map(|part| &part.name))
        });
        paths
    }

    #[tokio::test]
    async fn a_selection_reads_only_its_namespaces_with_progress_and_drops_the_rest() {
        let dir = std::env::temp_dir().join(format!("jykre-selection-{}", ConnectionId::new()));
        let id = ConnectionId::new();
        let driver = CatalogDriver::new(vec![("main", vec!["a", "b"]), ("other", vec!["x", "y"])]);
        let main = ObjectPath::default().child(ObjectKind::Catalog, "main");
        let selection = MetadataScope {
            all: false,
            include: vec![main.clone(), schema("other", "x"), schema("other", "gone")],
        };
        let cancel = CancellationToken::new();
        let (outcome, updates) = apply(&driver, &dir, id, &selection, &selection, &cancel).await;
        assert_eq!(outcome.refreshed, 3);
        assert_eq!(outcome.missing, [schema("other", "gone")]);
        assert!(outcome.failures.is_empty() && !outcome.cancelled);
        // Only the chosen schemas were read; other.y never was.
        assert_eq!(
            *driver.read.lock().unwrap(),
            [
                schema("main", "a"),
                schema("main", "b"),
                schema("other", "x"),
                schema("other", "gone")
            ]
        );
        assert_eq!(updates[0], RefreshProgress::discovering());
        let reading: Vec<_> = updates[1..]
            .iter()
            .map(|update| (update.done, update.total))
            .collect();
        assert_eq!(reading, [(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]);
        assert!(updates[1].eta.is_none());
        assert!(updates[2].eta.is_some());
        let other = ObjectPath::default().child(ObjectKind::Catalog, "other");
        assert_eq!(
            cached(&dir, id).await,
            [
                main.clone(),
                schema("main", "a"),
                schema("main", "a").child(ObjectKind::Table, "t"),
                schema("main", "b"),
                schema("main", "b").child(ObjectKind::Table, "t"),
                other.clone(),
                schema("other", "x"),
                schema("other", "x").child(ObjectKind::Table, "t"),
            ]
        );
        // The server drops main.b; a whole catalog follows the server.
        driver.server.lock().unwrap()[0]
            .1
            .retain(|name| *name != "b");
        driver.read.lock().unwrap().clear();
        let (outcome, _) = apply(&driver, &dir, id, &selection, &selection, &cancel).await;
        assert_eq!(outcome.refreshed, 2);
        assert!(!cached(&dir, id).await.contains(&schema("main", "b")));
        // Unchoosing other.x reads nothing and removes it from the cache.
        let narrower = MetadataScope {
            all: false,
            include: vec![main.clone()],
        };
        driver.read.lock().unwrap().clear();
        let added = narrower.added_since(&selection);
        let (outcome, updates) = apply(&driver, &dir, id, &narrower, &added, &cancel).await;
        assert_eq!(outcome.refreshed, 0);
        assert!(updates.is_empty());
        assert!(driver.read.lock().unwrap().is_empty());
        let paths = cached(&dir, id).await;
        assert!(paths.iter().all(|path| path.is_within(&main)), "{paths:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_failed_namespace_keeps_its_cache_and_cancelling_removes_nothing() {
        let dir = std::env::temp_dir().join(format!("jykre-selection-{}", ConnectionId::new()));
        let id = ConnectionId::new();
        let everything = MetadataScope::everything();
        let cancel = CancellationToken::new();
        let driver = CatalogDriver::new(vec![("main", vec!["a", "b", "c"])]);
        apply(&driver, &dir, id, &everything, &everything, &cancel).await;
        assert_eq!(cached(&dir, id).await.len(), 7);
        let failing = CatalogDriver {
            failing: Some("b"),
            ..CatalogDriver::new(vec![("main", vec!["a", "b", "c"])])
        };
        let (outcome, _) = apply(&failing, &dir, id, &everything, &everything, &cancel).await;
        assert_eq!(outcome.refreshed, 2);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].0, schema("main", "b"));
        let kept = schema("main", "b").child(ObjectKind::Table, "t");
        assert!(cached(&dir, id).await.contains(&kept));
        // Cancelled after the first schema, with a schema the server lost:
        // nothing is removed, because the read did not finish.
        let cancel = CancellationToken::new();
        let cancelling = CatalogDriver {
            cancel_after: Some((1, cancel.clone())),
            ..CatalogDriver::new(vec![("main", vec!["a", "b"])])
        };
        let (outcome, _) = apply(&cancelling, &dir, id, &everything, &everything, &cancel).await;
        assert!(outcome.cancelled);
        assert_eq!(outcome.refreshed, 1);
        assert!(cached(&dir, id).await.contains(&schema("main", "c")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
