//! Application operations, independent of the UI and its executor.
use crate::drivers::*;
use crate::logging;
use crate::metadata_scope::{EtaEstimator, MetadataScope, RefreshPhase, RefreshProgress};
use crate::result::{ResultBuilder, ResultData, ResultLimit};
use crate::telemetry::attributes::{
    db_system, namespace, operation_name, path_namespace, record_error, record_outcome,
};
use crate::tr;
use jykre_storage::{
    ChildPage, MetadataRepository, PageCursor, PageRequest, PendingRefresh, RefreshStatus,
    SearchCursor, SearchPage,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{
    future::Future,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tracing::Instrument as _;

mod exporting;
pub use exporting::{
    ExportProgress, ExportRead, ExportRequest, ExportRun, Exported, FILE_TIMEOUT, export,
    export_with,
};

/// How long a read from the local metadata cache is expected to take
/// ([Performance](../docs/standards/performance.md#budgets)).
const CACHE_READ_BUDGET: Duration = Duration::from_millis(50);

/// Children read per page when a node is expanded or "Load more" is used.
pub const CHILD_PAGE: u32 = 200;

/// Pages of the explorer tree read from one snapshot of the cache.
pub struct MetadataSnapshot {
    /// Each page read, with the node it belongs to, in the order asked.
    pub pages: Vec<(ObjectPath, ChildPage)>,
    pub statuses: Vec<RefreshStatus>,
    pub version: i64,
    /// The cache was corrupted and recreated when this read opened it.
    pub recovered: bool,
}

impl MetadataSnapshot {
    /// Whether the cache holds nothing at the root.
    pub fn is_empty(&self) -> bool {
        self.pages
            .iter()
            .all(|(parent, page)| !parent.0.is_empty() || page.objects.is_empty())
    }
}

type CacheKey = (PathBuf, ConnectionId);

fn caches() -> std::sync::MutexGuard<'static, HashMap<CacheKey, MetadataRepository>> {
    static CACHES: OnceLock<std::sync::Mutex<HashMap<CacheKey, MetadataRepository>>> =
        OnceLock::new();
    CACHES
        .get_or_init(Default::default)
        .lock()
        // The map stays consistent even if a holder panicked: entries are
        // only inserted or removed whole.
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The connection's metadata cache, kept open between operations. Only the
/// first use after start, after the profile changed or after a failed read
/// opens the file and checks it (`quick_check`). Returns whether this call
/// opened it.
async fn cache(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
) -> DriverResult<(MetadataRepository, bool)> {
    let config = &cache_config(config)?;
    let key = (dir.to_path_buf(), id);
    let kept = caches()
        .get(&key)
        .filter(|repository| repository.belongs_to(config))
        .cloned();
    if let Some(repository) = kept {
        return Ok((repository, false));
    }
    let repository = MetadataRepository::open(dir, id, config).await?;
    caches().insert(key, repository.clone());
    Ok((repository, true))
}

/// The configuration a cache is keyed by: the driver's identity of the
/// catalog ([`Driver::cache_identity`]), so changing compute, defaults or TLS
/// keeps the cache.
pub fn cache_config(config: &ConnectionConfig) -> DriverResult<ConnectionConfig> {
    let driver = builtin_registry()?.get(config.driver)?;
    Ok(ConnectionConfig {
        driver: config.driver,
        options: driver.cache_identity(config),
    })
}

/// Keeps `repository` as the connection's open cache, as `prepare` left it.
fn keep_cache(dir: &Path, id: ConnectionId, repository: MetadataRepository) {
    caches().insert((dir.to_path_buf(), id), repository);
}

/// Forgets the connection's open cache after a failed read, so the next
/// read opens and checks the file again.
fn forget_cache(dir: &Path, id: ConnectionId) {
    caches().remove(&(dir.to_path_buf(), id));
}

/// Closes the connection's cache, if one is kept open, and removes its
/// files: what deleting the connection leaves behind.
pub async fn remove_cache(dir: &Path, id: ConnectionId) -> DriverResult<()> {
    let kept = caches().remove(&(dir.to_path_buf(), id));
    if let Some(repository) = kept {
        repository.close().await;
    }
    MetadataRepository::remove(dir, id).await
}

/// Closes every cache kept open under `dir`, so its files can be removed.
pub async fn release_caches(dir: &Path) {
    let released: Vec<_> = {
        let mut caches = caches();
        let keys: Vec<_> = caches
            .keys()
            .filter(|(folder, _)| folder == dir)
            .cloned()
            .collect();
        keys.iter().filter_map(|key| caches.remove(key)).collect()
    };
    for repository in released {
        repository.close().await;
    }
}

/// Reconciles the connection's cache with its saved profile (a changed
/// configuration empties it) and keeps it open for the reads that follow.
/// Returns whether the cache was corrupted and recreated.
pub async fn prepare_cache(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
) -> DriverResult<bool> {
    let identity = cache_config(config)?;
    // A cache an earlier version keyed by the whole configuration keeps its
    // objects under the driver's narrower identity.
    MetadataRepository::adopt(dir, id, config, &identity).await?;
    let repository = MetadataRepository::prepare(dir, id, &identity).await?;
    let recovered = repository.recovered;
    keep_cache(dir, id, repository);
    Ok(recovered)
}

pub async fn cached_metadata(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    recover: bool,
) -> DriverResult<MetadataSnapshot> {
    cached_metadata_pages(
        dir,
        id,
        config,
        recover,
        vec![(ObjectPath::default(), CHILD_PAGE)],
    )
    .await
}

/// Reads the first `count` children (at least one page) of each node in
/// `pages` from one snapshot of the cache, with the refresh statuses: what
/// the explorer shows again after a refresh or at start.
pub async fn cached_metadata_pages(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    recover: bool,
    pages: Vec<(ObjectPath, u32)>,
) -> DriverResult<MetadataSnapshot> {
    let started = Instant::now();
    let result = async {
        let (repository, opened) = cache(dir, id, config).await?;
        if recover {
            repository.recover_interrupted().await?;
        }
        let requests: Vec<_> = pages
            .into_iter()
            .map(|(parent, count)| PageRequest {
                parent,
                after: None,
                limit: count.clamp(CHILD_PAGE, 100_000),
            })
            .collect();
        let (version, read) = repository.pages(&requests).await?;
        let statuses = repository.statuses().await?;
        tracing::debug!(
            connection_id = %id,
            pages = requests.len(),
            objects = read.iter().map(|page| page.objects.len()).sum::<usize>(),
            "explorer pages read from the cache"
        );
        Ok(MetadataSnapshot {
            pages: requests
                .into_iter()
                .map(|request| request.parent)
                .zip(read)
                .collect(),
            statuses,
            version,
            recovered: opened && repository.recovered,
        })
    }
    .await;
    if result.is_err() {
        forget_cache(dir, id);
    }
    logging::timing("explorer pages from cache", started, CACHE_READ_BUDGET);
    result
}

/// Reads the next page of `parent`'s children after `after` (its first
/// page when `None`) and the cache version it was read from. Only that page
/// is read, whatever the pages already loaded.
pub async fn cached_children(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    parent: ObjectPath,
    after: Option<PageCursor>,
) -> DriverResult<(i64, ChildPage)> {
    let started = Instant::now();
    let depth = parent.0.len();
    let result = async {
        let (repository, _) = cache(dir, id, config).await?;
        let (version, mut pages) = repository
            .pages(&[PageRequest {
                parent,
                after,
                limit: CHILD_PAGE,
            }])
            .await?;
        let page = pages.pop().unwrap_or_default();
        tracing::debug!(
            connection_id = %id,
            depth,
            objects = page.objects.len(),
            "explorer page read from the cache"
        );
        Ok((version, page))
    }
    .await;
    if result.is_err() {
        forget_cache(dir, id);
    }
    logging::timing("expand from cache", started, CACHE_READ_BUDGET);
    result
}

/// The last-level namespaces (schemas, or MySQL databases) the cache holds,
/// in key order: what a tab's context selector offers.
pub async fn cached_namespaces(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
) -> DriverResult<Vec<ObjectPath>> {
    let driver = builtin_registry()?.get(config.driver)?;
    let Some(last) = driver
        .descriptor()
        .capabilities
        .namespace_levels()
        .last()
        .copied()
    else {
        return Ok(Vec::new());
    };
    let result = match cache(dir, id, config).await {
        Ok((repository, _)) => repository.paths(last).await,
        Err(error) => Err(error),
    };
    if result.is_err() {
        forget_cache(dir, id);
    }
    result
}

/// Every relation the connection's cache holds (tables, views and
/// materialized views) with its kind, and the cache version they were read
/// from: what the editor builds its name index from. Only the read happens
/// here; what the editor makes of the names is the editor's business.
pub async fn cached_relations(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
) -> DriverResult<(i64, Vec<(ObjectPath, ObjectKind)>)> {
    let result = match cache(dir, id, config).await {
        Ok((repository, _)) => repository.relations().await,
        Err(error) => Err(error),
    };
    if result.is_err() {
        forget_cache(dir, id);
    }
    result
}

/// The texts of `files` (paths relative to `dir`), read on the runtime: how
/// the editor's SQL catalogs are read from their data directory. A file that
/// is missing, larger than `max_bytes` or not UTF-8 is `None`; one that exists
/// but cannot be used is logged.
pub async fn read_text_files(dir: &Path, files: &[String], max_bytes: u64) -> Vec<Option<String>> {
    let span = crate::client_span!(
        "read_files",
        jykre.files = crate::telemetry::attributes::count(files.len()),
        jykre.bytes = tracing::field::Empty
    );
    let texts = read_text_files_in(dir, files, max_bytes)
        .instrument(span.clone())
        .await;
    let bytes: usize = texts.iter().flatten().map(String::len).sum();
    span.record("jykre.bytes", crate::telemetry::attributes::count(bytes));
    texts
}

async fn read_text_files_in(dir: &Path, files: &[String], max_bytes: u64) -> Vec<Option<String>> {
    let mut texts = Vec::with_capacity(files.len());
    for file in files {
        let path = dir.join(file);
        let text = match tokio::fs::metadata(&path).await {
            Ok(found) if found.len() > max_bytes => {
                tracing::warn!(file = %file, bytes = found.len(), "data file too large; ignored");
                None
            }
            Ok(_) => match tokio::fs::read_to_string(&path).await {
                Ok(text) => Some(text),
                Err(error) => {
                    tracing::warn!(file = %file, error = %error, "data file unreadable; ignored");
                    None
                }
            },
            Err(_) => None,
        };
        texts.push(text);
    }
    texts
}

/// The columns and keys of one relation, read from the cache: what the
/// editor suggests after `alias.` and shows on hover.
pub async fn relation_columns(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    relation: ObjectPath,
) -> DriverResult<Vec<MetadataObject>> {
    let result = match cache(dir, id, config).await {
        Ok((repository, _)) => {
            repository
                .pages(&[PageRequest {
                    parent: relation,
                    after: None,
                    limit: 100_000,
                }])
                .await
        }
        Err(error) => Err(error),
    };
    match result {
        Ok((_, mut pages)) => Ok(pages.pop().map(|page| page.objects).unwrap_or_default()),
        Err(error) => {
            forget_cache(dir, id);
            Err(error)
        }
    }
}

/// **Test connection**: connects with the form's settings and asks the
/// server who it is.
pub async fn test_connection(
    input: ResolvedConnection,
    cancel: CancellationToken,
) -> DriverResult<ServerInfo> {
    let span = crate::client_span!(
        "test_connection",
        db.system.name = db_system(input.config.driver)
    );
    let result = async {
        let driver = builtin_registry()?.get(input.config.driver)?;
        driver.test(&input, cancel).await
    }
    .instrument(span.clone())
    .await;
    record_outcome(&span, &result);
    result
}

/// The namespaces under `parent` on the server, one level, for the choice of
/// what to introspect.
pub async fn discover_namespaces(
    input: ResolvedConnection,
    parent: ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Vec<MetadataObject>> {
    let span = crate::client_span!(
        "discover_namespaces",
        db.system.name = db_system(input.config.driver),
        db.namespace = path_namespace(&parent)
    );
    let result = async {
        let driver = builtin_registry()?.get(input.config.driver)?;
        driver.discover_namespaces(&input, &parent, cancel).await
    }
    .instrument(span.clone())
    .await;
    record_outcome(&span, &result);
    result
}

/// What the object at `path` has beyond the refreshed tree, read from the
/// server when the user asks (**Inspect**); never cached.
pub async fn inspect_object(
    input: ResolvedConnection,
    path: ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<Inspection> {
    let span = crate::client_span!(
        "inspect_object",
        db.system.name = db_system(input.config.driver),
        db.namespace = path_namespace(&path)
    );
    let result = async {
        let driver = builtin_registry()?.get(input.config.driver)?;
        driver.inspect(&input, &path, cancel).await
    }
    .instrument(span.clone())
    .await;
    record_outcome(&span, &result);
    result
}

/// The DDL or source of the object at `path`, read when the user asks.
pub async fn object_definition(
    input: ResolvedConnection,
    path: ObjectPath,
    cancel: CancellationToken,
) -> DriverResult<String> {
    let span = crate::client_span!(
        "object_definition",
        db.system.name = db_system(input.config.driver),
        db.namespace = path_namespace(&path)
    );
    let result = async {
        let driver = builtin_registry()?.get(input.config.driver)?;
        driver.definition(&input, &path, cancel).await
    }
    .instrument(span.clone())
    .await;
    record_outcome(&span, &result);
    result
}

/// Searches the connection's cache by name (Unicode, case-insensitive),
/// keeping only `kinds` when given, up to 200 matches with their ancestors.
pub async fn search_metadata(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    query: &str,
    kinds: Option<&[ObjectKind]>,
) -> DriverResult<Vec<MetadataObject>> {
    let started = Instant::now();
    let result = match cache(dir, id, config).await {
        Ok((repository, _)) => repository.search(query, kinds, 200).await,
        Err(error) => Err(error),
    };
    if result.is_err() {
        forget_cache(dir, id);
    }
    logging::timing("metadata search", started, CACHE_READ_BUDGET);
    result
}

/// A wildcard search of a connection's cache, read by
/// [`search_metadata_pattern`].
#[derive(Clone, Copy, Debug)]
pub struct PatternSearch<'a> {
    /// `*` matches any run of characters and `?` exactly one; a pattern
    /// with neither means "contains".
    pub pattern: &'a str,
    /// Only these kinds, when given.
    pub kinds: Option<&'a [ObjectKind]>,
    /// Only the objects inside this schema, catalog or database, when given.
    pub namespace: Option<&'a ObjectPath>,
    /// Where the page starts: the `next` of the previous page.
    pub after: Option<&'a SearchCursor>,
    /// At most this many matches (1 to 200).
    pub limit: u32,
}

/// One page of the connection's cached objects whose name matches
/// `search` (Unicode, case-insensitive), in folded-name order, each with its
/// full path.
pub async fn search_metadata_pattern(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    search: PatternSearch<'_>,
) -> DriverResult<SearchPage> {
    let started = Instant::now();
    let result = match cache(dir, id, config).await {
        Ok((repository, _)) => {
            repository
                .search_pattern(
                    search.pattern,
                    search.kinds,
                    search.namespace,
                    search.after,
                    search.limit,
                )
                .await
        }
        Err(error) => Err(error),
    };
    if result.is_err() {
        forget_cache(dir, id);
    }
    logging::timing("metadata pattern search", started, CACHE_READ_BUDGET);
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
    let span = crate::client_span!(
        "refresh_metadata",
        connection_id = %id,
        driver = ?input.config.driver,
        depth = scope.0.len(),
        db.system.name = db_system(input.config.driver),
        db.namespace = path_namespace(&scope),
        jykre.connection.id = %id,
        jykre.metadata.depth = crate::telemetry::attributes::count(scope.0.len())
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
    record_outcome(&span, &result);
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
    let (repository, _) = cache(dir, id, &input.config).await?;
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
        return Err(error);
    }
    Ok((published?, received))
}

/// How many namespaces a choice reads from the server at once. Publishing
/// stays one at a time: a cache has one writer.
const PARALLEL_READS: usize = 4;

/// Objects a read delivered, held until they are published.
#[derive(Default)]
struct Collected(Vec<MetadataObject>);

#[async_trait]
impl IntrospectionSink for Collected {
    async fn batch(&mut self, objects: Vec<MetadataObject>) -> DriverResult<()> {
        self.0.extend(objects);
        Ok(())
    }
}

/// Publishes what a read of `scope` delivered, in one refresh of the cache.
/// Returns how many objects it held (none when the scope no longer exists).
async fn publish(
    repository: &MetadataRepository,
    scope: &ObjectPath,
    objects: Vec<MetadataObject>,
    report: &IntrospectionReport,
    cancel: &CancellationToken,
) -> DriverResult<usize> {
    let received = objects.len();
    let mut staged = repository.begin(scope).await?;
    let mut result = Ok(());
    for chunk in objects.chunks(500) {
        result = staged.batch(chunk.to_vec()).await;
        if result.is_err() {
            break;
        }
    }
    let published = match result {
        Ok(()) => staged.activate(report, cancel).await,
        Err(error) => Err(error),
    };
    if let Err(error) = published {
        staged.fail(&error).await?;
        return Err(error);
    }
    Ok(received)
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
                    // Everything leaves out what the driver says waits to be
                    // chosen by name, such as a catalog of system tables.
                    let everything = path.0.is_empty() && rule.0.is_empty();
                    if !path.0.is_empty() {
                        self.listed.push(path);
                    }
                    // Depth first, in name order.
                    pending.extend(
                        children
                            .into_iter()
                            .rev()
                            .map(|child| child.path)
                            .filter(|child| !(everything && driver.left_out_of_everything(child))),
                    );
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
    apply_selection_with(driver, target, &cancel, progress).await
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
    driver: Arc<dyn Driver>,
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
    let span = crate::client_span!(
        "apply_selection",
        connection_id = %id,
        driver = ?input.config.driver,
        db.system.name = db_system(input.config.driver),
        jykre.connection.id = %id,
        jykre.namespaces.refreshed = tracing::field::Empty,
        jykre.namespaces.failed = tracing::field::Empty
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
                .resolve(&*driver, input, levels, rule, cancel, &mut outcome.failures)
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
        let (repository, _) = cache(dir, id, &input.config).await?;
        // A namespace the cache does not hold yet shows its names first and
        // its columns right after; one already cached is read in full only,
        // so its columns never vanish in between.
        let mut queue = std::collections::VecDeque::new();
        for unit in &plan.units {
            let cached = !repository.children(unit, None, 1).await?.objects.is_empty();
            let level = if cached {
                IntrospectionLevel::Full
            } else {
                IntrospectionLevel::Names
            };
            queue.push_back((unit.clone(), level));
        }
        if total > 0 {
            send(RefreshProgress {
                phase: RefreshPhase::Reading,
                done: 0,
                total,
                current: plan.units.first().cloned(),
                eta: None,
            });
        }
        // Up to PARALLEL_READS namespaces are read from the server at once;
        // each is published on its own, one at a time.
        let mut reads = tokio::task::JoinSet::new();
        let mut done = 0;
        loop {
            while reads.len() < PARALLEL_READS {
                let Some((unit, level)) = queue.pop_front() else {
                    break;
                };
                let (driver, input, cancel) = (driver.clone(), input.clone(), cancel.clone());
                let read_span = crate::client_span!(
                    "read_namespace",
                    db.system.name = db_system(input.config.driver),
                    db.namespace = path_namespace(&unit),
                    jykre.metadata.objects = tracing::field::Empty
                );
                reads.spawn(
                    async move {
                        let started = Instant::now();
                        let mut sink = Collected::default();
                        let read = driver
                            .introspect_level(&input, &unit, level, &mut sink, cancel)
                            .await
                            .map(|report| (sink.0, report));
                        let span = tracing::Span::current();
                        match &read {
                            Ok((objects, _)) => {
                                span.record(
                                    "jykre.metadata.objects",
                                    crate::telemetry::attributes::count(objects.len()),
                                );
                            }
                            Err(error) => record_error(&span, error),
                        }
                        (unit, level, read, started.elapsed())
                    }
                    .instrument(read_span),
                );
            }
            let Some(joined) = reads.join_next().await else {
                break;
            };
            if cancel.is_cancelled() {
                reads.abort_all();
                outcome.cancelled = true;
                return Ok(outcome);
            }
            let Ok((unit, level, read, took)) = joined else {
                // A read task panicked; the panic hook recorded it.
                continue;
            };
            let published = match read {
                Ok((objects, report)) => {
                    publish(&repository, &unit, objects, &report, cancel).await
                }
                Err(error) => Err(error),
            };
            match (level, published) {
                (_, Err(error)) if error.code == ErrorCode::Cancelled => {
                    reads.abort_all();
                    outcome.cancelled = true;
                    return Ok(outcome);
                }
                (IntrospectionLevel::Names, Ok(0)) | (IntrospectionLevel::Full, Ok(0)) => {
                    done += 1;
                    outcome.missing.push(unit.clone());
                }
                // The names are shown; the full read follows before others.
                (IntrospectionLevel::Names, Ok(_)) => {
                    queue.push_front((unit.clone(), IntrospectionLevel::Full));
                    continue;
                }
                (_, Ok(_)) => {
                    done += 1;
                    outcome.refreshed += 1;
                }
                (_, Err(error)) => {
                    done += 1;
                    outcome.failures.push((unit.clone(), error));
                }
            }
            eta.record(took);
            // The last one is reported once all are done, below.
            if done < total {
                send(RefreshProgress {
                    phase: RefreshPhase::Reading,
                    done,
                    total,
                    current: Some(unit),
                    eta: eta.remaining(total - done),
                });
            }
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
        let (repository, _) = cache(dir, id, &input.config).await?;
        repository.retain(&subtrees, &nodes).await?;
        Ok(outcome)
    }
    .instrument(span.clone())
    .await;
    match &result {
        Ok(outcome) => {
            span.record(
                "jykre.namespaces.refreshed",
                crate::telemetry::attributes::count(outcome.refreshed),
            );
            span.record(
                "jykre.namespaces.failed",
                crate::telemetry::attributes::count(outcome.failures.len()),
            );
        }
        Err(error) => record_error(&span, error),
    }
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

/// A handle to the only runtime, for a service that spawns tasks of its own
/// there (the automation engine, ADR-0034); never a second runtime.
pub fn runtime_handle() -> DriverResult<tokio::runtime::Handle> {
    runtime().map(|runtime| runtime.handle().clone())
}

/// Drive network futures on Tokio while allowing the GUI to await their result.
pub async fn run<T: Send + 'static>(
    work: impl Future<Output = DriverResult<T>> + Send + 'static,
) -> DriverResult<T> {
    runtime()?
        .spawn(work.in_current_span())
        .await
        .map_err(|error| {
            // The panic hook already recorded a panic with its backtrace.
            tracing::error!(panicked = error.is_panic(), "database task failed");
            DriverError::new(ErrorCode::Internal, tr!("Database task failed"))
        })?
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
    /// The kept session held an open transaction and no longer answered, so
    /// nothing ran: the server rolled that transaction back, and statements
    /// meant for it would have committed one by one in a new session.
    pub transaction_lost: bool,
    /// A report's session came back with a transaction open, so it was
    /// closed, which rolled the transaction back ([`query_in_context`]).
    pub transaction_closed: bool,
    /// Where unqualified names resolve after the run, as the session reports
    /// it ([`Session::context`]). `None` when it could not be asked: the
    /// last result is still open for paging, or the session ended.
    pub context: Option<SessionContext>,
    /// The session's transaction state after the run, as the driver reports
    /// it ([`Session::transaction`]). `None` when the driver cannot tell or
    /// could not be asked, under the same conditions as `context`; the
    /// caller then infers it.
    pub transaction: Option<TransactionStatus>,
    /// Whether the session this run opened is encrypted, asked before its
    /// first statement ([`Session::encryption`]). `None` when the run kept
    /// the tab's session, whose answer has not changed, or opened none.
    pub encryption: Option<Encryption>,
}

/// A tab's kept session, and whether the tab knows it holds an open
/// transaction.
pub struct TabSession {
    pub session: Option<Box<dyn Session>>,
    pub holds_transaction: bool,
}

/// Runs `requests` in order on the tab's session, opening one when there is
/// none, and hands the session back so state set by one run (`USE`, `SET`,
/// temporary tables, a transaction) applies to the next. A failed statement
/// keeps the session; only a session the driver could not bring back to a
/// known state is closed. A kept session is checked with [`Session::ping`]
/// before it is reused, and replaced when it no longer answers, unless it
/// held an open transaction: then nothing runs
/// ([`SessionRun::transaction_lost`]), since the statements would commit one
/// by one in a new session. A statement is never retried.
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
/// sent to the server, so the UI can tell which one is running. `connection`
/// only labels the run's telemetry span.
pub async fn query(
    connection: Option<ConnectionId>,
    input: ResolvedConnection,
    tab: TabSession,
    requests: Vec<QueryRequest>,
    memory_ceiling: usize,
    cancel: CancellationToken,
    progress: Option<Progress>,
) -> DriverResult<SessionRun> {
    let TabSession {
        session,
        holds_transaction,
    } = tab;
    let driver = match builtin_registry().and_then(|registry| registry.get(input.config.driver)) {
        Ok(driver) => driver,
        Err(error) => {
            close_session(session).await;
            return Err(error);
        }
    };
    Ok(run_query(
        driver,
        connection,
        input,
        session,
        Statements {
            requests,
            memory_ceiling,
            progress,
            settle: false,
            holds_transaction,
        },
        cancel,
    )
    .await)
}

/// Positions of the statements of a run, sent as each one starts.
pub type Progress = tokio::sync::mpsc::UnboundedSender<usize>;

/// [`query`] with a given driver, for no saved connection in particular.
pub async fn query_with(
    driver: Arc<dyn Driver>,
    input: ResolvedConnection,
    session: Option<Box<dyn Session>>,
    requests: Vec<QueryRequest>,
    memory_ceiling: usize,
    cancel: CancellationToken,
    progress: Option<Progress>,
) -> DriverResult<SessionRun> {
    Ok(run_query(
        driver,
        None,
        input,
        session,
        Statements {
            requests,
            memory_ceiling,
            progress,
            settle: false,
            holds_transaction: false,
        },
        cancel,
    )
    .await)
}

/// A run inside its span, logged when it ends; `connection` only labels
/// the span.
async fn run_query(
    driver: Arc<dyn Driver>,
    connection: Option<ConnectionId>,
    input: ResolvedConnection,
    session: Option<Box<dyn Session>>,
    statements: Statements,
    cancel: CancellationToken,
) -> SessionRun {
    let span = run_span(connection, &input, &statements.requests);
    let started = Instant::now();
    let run = run_statements(driver, input, session, statements, cancel)
        .instrument(span.clone())
        .await;
    log_run(&span, started, &run);
    run
}

/// [`query`] for a report panel, whose session belongs to one `context`
/// (ADR-0025): the session opens, or reopens when it was lost, with the
/// connection the driver gives for that context
/// ([`Driver::session_connection`]: another database's for a PostgreSQL
/// profile that offers the server's databases), and every request carries
/// it. The session is always settled after the run, even when the last
/// result has more rows (a report does not page): asking the session where
/// names resolve discards that result, and a `USE` the panel ran is undone
/// for the next panel of the same session. A session left in a transaction
/// is closed, which rolls it back ([`SessionRun::transaction_closed`]).
pub async fn query_in_context(
    connection: Option<ConnectionId>,
    input: ResolvedConnection,
    context: SessionContext,
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
    in_context(
        driver,
        connection,
        input,
        context,
        session,
        Statements {
            requests,
            memory_ceiling,
            progress: None,
            settle: true,
            holds_transaction: false,
        },
        cancel,
    )
    .await
}

/// [`query_in_context`] with a given driver, for no saved connection in
/// particular.
pub async fn query_in_context_with(
    driver: Arc<dyn Driver>,
    input: ResolvedConnection,
    context: SessionContext,
    session: Option<Box<dyn Session>>,
    requests: Vec<QueryRequest>,
    memory_ceiling: usize,
    cancel: CancellationToken,
) -> DriverResult<SessionRun> {
    in_context(
        driver,
        None,
        input,
        context,
        session,
        Statements {
            requests,
            memory_ceiling,
            progress: None,
            settle: true,
            holds_transaction: false,
        },
        cancel,
    )
    .await
}

async fn in_context(
    driver: Arc<dyn Driver>,
    connection: Option<ConnectionId>,
    input: ResolvedConnection,
    context: SessionContext,
    session: Option<Box<dyn Session>>,
    mut statements: Statements,
    cancel: CancellationToken,
) -> DriverResult<SessionRun> {
    let input = match driver.session_connection(&input, &context) {
        Ok(input) => input,
        Err(error) => {
            close_session(session).await;
            return Err(error);
        }
    };
    for request in &mut statements.requests {
        request.options.context = context.clone();
    }
    let mut run = run_query(driver, connection, input, session, statements, cancel).await;
    // A report keeps no transaction: one its panel left open (or failed, or
    // MySQL's autocommit off) would hold locks and a snapshot between runs
    // and reach the next panel. Closing the session rolls it back, and the
    // panel says so.
    if !matches!(run.transaction, None | Some(TransactionStatus::Idle)) {
        close_session(run.session.take()).await;
        run.transaction_closed = true;
    }
    Ok(run)
}

/// The span of a run: the local log's fields (`driver`, `statements`) and,
/// for telemetry, the database system, the connection and the namespace the
/// statements start in (replaced by where they ended, see [`log_run`]).
fn run_span(
    connection: Option<ConnectionId>,
    input: &ResolvedConnection,
    requests: &[QueryRequest],
) -> tracing::Span {
    let span = crate::client_span!(
        "query",
        driver = ?input.config.driver,
        statements = requests.len(),
        db.system.name = db_system(input.config.driver),
        db.namespace = tracing::field::Empty,
        db.response.returned_rows = tracing::field::Empty,
        jykre.connection.id = tracing::field::Empty,
        jykre.statements = crate::telemetry::attributes::count(requests.len()),
        jykre.statements.completed = tracing::field::Empty,
        jykre.session.reopened = tracing::field::Empty
    );
    if let Some(connection) = connection {
        span.record("jykre.connection.id", connection.to_string());
    }
    if let Some(namespace) = requests
        .first()
        .and_then(|request| namespace(&request.options.context))
    {
        span.record("db.namespace", namespace);
    }
    span
}

/// Logs how a run ended, with its duration, rows and error code, and
/// records it on the run's span.
fn log_run(span: &tracing::Span, started: Instant, run: &SessionRun) {
    if let Some(namespace) = run.context.as_ref().and_then(namespace) {
        span.record("db.namespace", namespace);
    }
    span.record("jykre.session.reopened", run.reopened);
    match &run.batch {
        Ok(batch) => {
            let rows: usize = batch.completed.iter().map(|r| r.data.row_count()).sum();
            span.record(
                "db.response.returned_rows",
                crate::telemetry::attributes::count(rows),
            );
            span.record(
                "jykre.statements.completed",
                crate::telemetry::attributes::count(batch.completed.len()),
            );
            if let Some(error) = &batch.error {
                record_error(span, error);
            }
        }
        Err(error) => record_error(span, error),
    }
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
}

/// What [`run_statements`] runs and how it ends.
struct Statements {
    requests: Vec<QueryRequest>,
    memory_ceiling: usize,
    progress: Option<Progress>,
    /// Ask the session's context and transaction even when the last result
    /// is still open, discarding it (a report never pages).
    settle: bool,
    /// The kept session holds an open transaction: if it was lost, the run
    /// stops instead of running the statements on a new session.
    holds_transaction: bool,
}

async fn run_statements(
    driver: Arc<dyn Driver>,
    input: ResolvedConnection,
    session: Option<Box<dyn Session>>,
    statements: Statements,
    cancel: CancellationToken,
) -> SessionRun {
    let Statements {
        requests,
        memory_ceiling,
        progress,
        settle,
        holds_transaction,
    } = statements;
    let mut run = SessionRun {
        batch: Err(DriverError::cancelled()),
        session: None,
        reopened: false,
        ended: false,
        transaction_lost: false,
        transaction_closed: false,
        context: None,
        transaction: None,
        encryption: None,
    };
    if let Some(error) = requests.iter().find_map(|request| request.validate().err()) {
        run.batch = Err(error);
        run.session = session;
        return run;
    }
    let kept = session.is_some();
    let (session, lost) =
        session_for_run(&*driver, &input, session, &cancel, !holds_transaction).await;
    if lost && holds_transaction {
        run.ended = true;
        run.transaction_lost = true;
    } else {
        run.reopened = lost;
    }
    let mut session = match session {
        Ok(session) => session,
        Err(error) => {
            run.batch = Err(error);
            return run;
        }
    };
    if !kept || lost {
        // Asked once per session, before any result is open or any
        // transaction has a snapshot. Not knowing never stops the run.
        run.encryption = Some(match session.encryption(cancel.clone()).await {
            Ok(encryption) => encryption,
            Err(error) => {
                tracing::debug!(error_code = ?error.code, "could not read the session's encryption");
                Encryption::Unknown
            }
        });
    }
    let mut results = Vec::new();
    let mut failure = None;
    let descriptor = driver.descriptor();
    for (index, request) in requests.into_iter().enumerate() {
        tracing::trace!(statement = index, sql = %request.sql, "statement");
        if let Some(progress) = &progress {
            // The UI may be gone; the run goes on regardless.
            let _ = progress.send(index);
        }
        let start = Instant::now();
        let mut builder = ResultBuilder::new(request.options.max_rows, memory_ceiling);
        let operation = operation_name(&request.sql, Some(descriptor.dialect));
        let span = crate::client_span!(
            "statement",
            otel.name = operation,
            db.system.name = db_system(descriptor.kind),
            db.operation.name = operation,
            db.namespace = tracing::field::Empty,
            db.response.returned_rows = tracing::field::Empty,
            db.response.status_code = tracing::field::Empty,
            jykre.bytes = tracing::field::Empty,
            jykre.rows.limit = crate::telemetry::attributes::count(request.options.max_rows)
        );
        if let Some(namespace) = namespace(&request.options.context) {
            span.record("db.namespace", namespace);
        }
        let executed = session
            .execute(&request, &mut builder, cancel.clone())
            .instrument(span.clone())
            .await;
        let _entered = span.enter();
        match executed {
            Ok(outcome) => {
                let (data, limit) = page_limit(builder, &outcome, request.options.max_rows);
                span.record(
                    "db.response.returned_rows",
                    crate::telemetry::attributes::count(data.row_count()),
                );
                span.record(
                    "jykre.bytes",
                    crate::telemetry::attributes::count(data.memory_bytes()),
                );
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
                record_error(&span, &error);
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
    // Asking would close a result still open for paging; its context is
    // asked once it is read to the end, unless the run is settled anyway.
    if session.is_open() && (settle || !results.last().is_some_and(|result| result.has_more)) {
        run.context = session_context(&mut *session, &cancel).await;
        run.transaction = transaction_status(&mut *session, &cancel).await;
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

/// Why a run on a tab whose session held an open transaction did not start:
/// that session no longer answers, so the server rolled the transaction
/// back, and a new session would commit each statement on its own.
fn transaction_lost() -> DriverError {
    DriverError::new(
        ErrorCode::Connection,
        "The session that held the open transaction no longer answers, so the server rolled the transaction back. Nothing ran.",
    )
}

/// The tab's kept session when it still answers its ping, otherwise a new
/// one; `true` when a kept session was lost, so the caller tells the user
/// that session state started over. A lost session is not replaced unless
/// `replace`: the run then fails with [`transaction_lost`].
async fn session_for_run(
    driver: &dyn Driver,
    input: &ResolvedConnection,
    session: Option<Box<dyn Session>>,
    cancel: &CancellationToken,
    replace: bool,
) -> (DriverResult<Box<dyn Session>>, bool) {
    let mut reopened = false;
    let system = db_system(input.config.driver);
    if let Some(mut session) = session {
        let span = crate::client_span!("ping", db.system.name = system);
        let pinged = session.ping().instrument(span.clone()).await;
        match pinged {
            Ok(()) if session.is_open() => return (Ok(session), false),
            _ => {
                if let Err(error) = &pinged {
                    record_error(&span, error);
                }
                close_session(Some(session)).await;
                if !replace {
                    tracing::debug!("kept session holding a transaction no longer answers");
                    return (Err(transaction_lost()), true);
                }
                tracing::debug!("kept session no longer answers; opening another");
                reopened = true;
            }
        }
    }
    let span = crate::client_span!("open_session", db.system.name = system);
    let opened = driver
        .open_session(input, cancel.clone())
        .instrument(span.clone())
        .await;
    record_outcome(&span, &opened);
    (opened, reopened)
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
    /// Where unqualified names resolve, asked once the result was read to
    /// the end.
    pub context: Option<SessionContext>,
    /// The session's transaction state, asked when `context` is.
    pub transaction: Option<TransactionStatus>,
    /// The session for the tab's next run, or `None` when it was closed.
    pub session: Option<Box<dyn Session>>,
}

/// Asks the session where unqualified names resolve. A failure only means
/// the tab keeps showing the context it had; the session's own state is
/// what tells the next run whether it is still usable.
async fn session_context(
    session: &mut dyn Session,
    cancel: &CancellationToken,
) -> Option<SessionContext> {
    let span = crate::client_span!("session_context");
    match session
        .context(cancel.clone())
        .instrument(span.clone())
        .await
    {
        Ok(context) => Some(context),
        Err(error) => {
            record_error(&span, &error);
            tracing::debug!(error_code = ?error.code, "session context unavailable");
            None
        }
    }
}

/// Asks the session for its transaction state. A failure only means the tab
/// infers the state from the statements it ran.
async fn transaction_status(
    session: &mut dyn Session,
    cancel: &CancellationToken,
) -> Option<TransactionStatus> {
    let span = crate::client_span!("transaction_state");
    match session
        .transaction(cancel.clone())
        .instrument(span.clone())
        .await
    {
        Ok(status) => status,
        Err(error) => {
            record_error(&span, &error);
            tracing::debug!(error_code = ?error.code, "transaction state unavailable");
            None
        }
    }
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
    let span = crate::client_span!(
        "fetch",
        db.namespace = tracing::field::Empty,
        db.response.returned_rows = tracing::field::Empty,
        jykre.bytes = tracing::field::Empty,
        jykre.rows.limit = crate::telemetry::attributes::count(options.max_rows)
    );
    if let Some(namespace) = namespace(&options.context) {
        span.record("db.namespace", namespace);
    }
    let fetched = session
        .fetch(&options, &mut builder, cancel.clone())
        .instrument(span.clone())
        .await;
    record_outcome(&span, &fetched);
    let page = fetched.map(|outcome| {
        let _entered = span.enter();
        let (data, limit) = page_limit(builder, &outcome, options.max_rows);
        span.record(
            "db.response.returned_rows",
            crate::telemetry::attributes::count(data.row_count()),
        );
        span.record(
            "jykre.bytes",
            crate::telemetry::attributes::count(data.memory_bytes()),
        );
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
    let (context, transaction) =
        if session.is_open() && page.as_ref().is_ok_and(|page| !page.has_more) {
            (
                session_context(&mut *session, &cancel).await,
                transaction_status(&mut *session, &cancel).await,
            )
        } else {
            (None, None)
        };
    let session = if session.is_open() {
        Some(session)
    } else {
        close_session(Some(session)).await;
        None
    };
    Ok(PageRun {
        page,
        context,
        transaction,
        session,
    })
}

/// Closes a tab's session within a short deadline; a close failure only
/// means the server drops the connection on its own.
pub async fn close_session(session: Option<Box<dyn Session>>) {
    let Some(session) = session else {
        return;
    };
    let span = crate::client_span!("close_session");
    let closed = tokio::time::timeout(Duration::from_secs(5), session.close())
        .instrument(span.clone())
        .await;
    match closed {
        Ok(result) => record_outcome(&span, &result),
        Err(_) => {
            record_error(&span, &DriverError::new(ErrorCode::Timeout, ""));
            let _entered = span.enter();
            tracing::debug!("session close timed out");
        }
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

/// **New query for table**: a query for the relation at `path`, quoted by the
/// driver's rules, that reads at most `row_limit` rows, the connection's row
/// limit, written into its `LIMIT`.
pub fn preview_sql(driver: &dyn Driver, path: &ObjectPath, row_limit: u32) -> String {
    format!(
        "SELECT * FROM {} LIMIT {row_limit};",
        driver.relation_name(path)
    )
}

/// The statement the **Data** view reads the relation at `path` with: every
/// row, quoted by the driver's rules and without a `LIMIT`. The row limit
/// applies while it is read, as the size of each page, and the next pages
/// are read from the result the session keeps open ([`fetch`]), so the text
/// says what runs and no page is cut by it.
pub fn data_sql(driver: &dyn Driver, path: &ObjectPath) -> String {
    format!("SELECT * FROM {};", driver.relation_name(path))
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
        let repository = MetadataRepository::open(&dir, id, &cache_config(&config).unwrap())
            .await
            .unwrap();
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
        assert_eq!(first.pages.len(), 1);
        assert_eq!(first.pages[0].1.objects, vec![db.clone()]);
        assert!(!first.is_empty());
        // The cache stays open for the reads that follow.
        assert!(caches().contains_key(&(dir.clone(), id)));
        let expanded = cached_metadata_pages(
            &dir,
            id,
            &config,
            false,
            vec![(ObjectPath::default(), 200), (db.path.clone(), 200)],
        )
        .await
        .unwrap();
        let (parent, page) = &expanded.pages[1];
        assert_eq!(*parent, db.path);
        assert_eq!(page.objects.len(), 200);
        // The next page is read on its own, from where the first ended.
        let (version, rest) =
            cached_children(&dir, id, &config, db.path.clone(), page.next.clone())
                .await
                .unwrap();
        assert_eq!(version, expanded.version);
        assert_eq!(rest.objects.len(), 50);
        assert_eq!(rest.objects[0].name, "table_200");
        assert!(rest.next.is_none());
        let search = search_metadata(&dir, id, &config, "table_249", None)
            .await
            .unwrap();
        assert!(search.contains(&db));
        assert!(search.iter().any(|object| object.name == "table_249"));
        let pattern = PatternSearch {
            pattern: "TABLE_24?",
            kinds: Some(&[ObjectKind::Table]),
            namespace: Some(&db.path),
            after: None,
            limit: 200,
        };
        let page = search_metadata_pattern(&dir, id, &config, pattern)
            .await
            .unwrap();
        assert_eq!(page.objects.len(), 10);
        assert_eq!(page.objects[0].name, "table_240");
        assert!(page.next.is_none());
        let expanded = cached_metadata_pages(
            &dir,
            id,
            &config,
            false,
            vec![(ObjectPath::default(), 200), (db.path.clone(), 400)],
        )
        .await
        .unwrap();
        assert_eq!(expanded.pages[1].1.objects.len(), 250);
        assert!(expanded.pages.iter().all(|(_, page)| page.next.is_none()));
        // Another configuration is not served from the open cache, and the
        // failed read lets the next one open and check the file again.
        let mut other = config.clone();
        other.options["user"] = serde_json::json!("another-fixture-user");
        assert!(cached_metadata(&dir, id, &other, false).await.is_err());
        assert!(!caches().contains_key(&(dir.clone(), id)));
        assert!(cached_metadata(&dir, id, &config, false).await.is_ok());
        release_caches(&dir).await;
        assert!(!caches().contains_key(&(dir.clone(), id)));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn preview_quotes_structural_names() {
        let path = ObjectPath::default()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Schema, "a.b")
            .child(ObjectKind::Table, "t\"x");
        assert_eq!(
            preview_sql(&PostgresDriver, &path, 100),
            "SELECT * FROM \"a.b\".\"t\"\"x\" LIMIT 100;"
        );
        let path = ObjectPath::default()
            .child(ObjectKind::Database, "d`b")
            .child(ObjectKind::Table, "orders")
            .child(ObjectKind::Column, "id");
        // The connection's row limit, not a fixed 100.
        assert_eq!(
            preview_sql(&MySqlDriver, &path, 1_000),
            "SELECT * FROM `d``b`.`orders` LIMIT 1000;"
        );
        let path = ObjectPath::default()
            .child(ObjectKind::Catalog, "cat")
            .child(ObjectKind::Schema, "sch")
            .child(ObjectKind::Table, "a`b");
        assert_eq!(
            preview_sql(&DatabricksDriver, &path, 50_000),
            "SELECT * FROM `cat`.`sch`.`a``b` LIMIT 50000;"
        );
    }

    #[test]
    fn the_data_view_reads_every_row_without_a_limit_in_its_text() {
        let path = ObjectPath::default()
            .child(ObjectKind::Database, "db")
            .child(ObjectKind::Schema, "a.b")
            .child(ObjectKind::Table, "t\"x");
        assert_eq!(
            data_sql(&PostgresDriver, &path),
            "SELECT * FROM \"a.b\".\"t\"\"x\";"
        );
        let path = ObjectPath::default()
            .child(ObjectKind::Database, "d`b")
            .child(ObjectKind::View, "orders");
        assert_eq!(
            data_sql(&MySqlDriver, &path),
            "SELECT * FROM `d``b`.`orders`;"
        );
        let path = ObjectPath::default()
            .child(ObjectKind::Catalog, "cat")
            .child(ObjectKind::Schema, "sch")
            .child(ObjectKind::Table, "a`b");
        assert_eq!(
            data_sql(&DatabricksDriver, &path),
            "SELECT * FROM `cat`.`sch`.`a``b`;"
        );
        // One statement, which the run takes whole.
        for dialect in [
            SqlDialect::Postgres,
            SqlDialect::MySql,
            SqlDialect::DatabricksSql,
        ] {
            let sql = data_sql(&PostgresDriver, &path);
            assert_eq!(
                crate::sql::statements(&sql, Some(dialect)),
                vec![0..sql.len()]
            );
        }
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
                parameters: Vec::new(),
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
                parameters: Vec::new(),
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

    #[tokio::test]
    async fn telemetry_of_a_run_carries_metadata_only() {
        let (subscriber, capture) = crate::telemetry::testing::capture("jykre=debug");
        let _default = tracing::subscriber::set_default(subscriber);
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({"host":"synthetic-host.internal","user":"fixture","database":"fixture","sslMode":"disable"}),
            },
            credentials: Credentials {
                password: Some(SecretString::from("synthetic-password-1234")),
            },
        };
        let context = SessionContext {
            catalog: None,
            schema: Some("sales".into()),
        };
        let requests = ["SELECT 'synthetic-sql-marker'", "UPDATE t SET a = 'x'"]
            .into_iter()
            .map(|sql| QueryRequest {
                parameters: Vec::new(),
                sql: sql.into(),
                options: ExecutionOptions {
                    context: context.clone(),
                    ..ExecutionOptions::default()
                },
            })
            .collect();
        let connection = ConnectionId::new();
        let batch = run_query(
            Arc::new(EchoDriver { rows: 2 }),
            Some(connection),
            input,
            None,
            Statements {
                requests,
                memory_ceiling: crate::result::DEFAULT_MEMORY_CEILING,
                progress: None,
                settle: false,
                holds_transaction: false,
            },
            CancellationToken::new(),
        )
        .await
        .batch
        .unwrap();
        assert_eq!(batch.completed.len(), 2);
        let text = capture.exported_text();
        for secret in [
            "synthetic-password-1234",
            "synthetic-sql-marker",
            "synthetic-cell-value",
            "synthetic-host.internal",
        ] {
            assert!(
                !text.contains(secret),
                "{secret} reached the collector: {text}"
            );
        }
        let spans = capture.spans.get_finished_spans().unwrap();
        let named = |name: &str| spans.iter().filter(|span| span.name == name).count();
        assert_eq!(named("query"), 1, "{text}");
        assert_eq!(named("open_session"), 1, "{text}");
        assert_eq!((named("SELECT"), named("UPDATE")), (1, 1), "{text}");
        for expected in [
            "db.system.name=postgresql",
            "db.operation.name=SELECT",
            "db.namespace=sales",
            "db.response.returned_rows=2 ",
            "db.response.returned_rows=4 ",
            "jykre.statements=2 ",
            &format!("jykre.connection.id={connection}"),
        ] {
            assert!(text.contains(expected), "{expected} missing: {text}");
        }
        let run = spans.iter().find(|span| span.name == "query").unwrap();
        let statement = spans.iter().find(|span| span.name == "SELECT").unwrap();
        assert_eq!(statement.parent_span_id, run.span_context.span_id());
        let rows = run
            .attributes
            .iter()
            .find(|pair| pair.key.as_str() == "db.response.returned_rows")
            .map(|pair| pair.value.clone());
        assert_eq!(
            rows,
            Some(opentelemetry::Value::I64(4)),
            "counts are integers"
        );
        assert_eq!(statement.span_kind, opentelemetry::trace::SpanKind::Client);
        assert!(
            text.contains("run finished"),
            "the run's log is exported too: {text}"
        );
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
            parameters: Vec::new(),
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
        /// The database each session opened on.
        databases: std::sync::Mutex<Vec<Option<String>>>,
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
        /// `BEGIN` opened a transaction and `COMMIT` ended it.
        in_transaction: bool,
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
                "BEGIN" => self.in_transaction = true,
                "COMMIT" => self.in_transaction = false,
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

        /// Reports the last `SET` as the schema; like a real connection,
        /// asking discards a result still open.
        async fn context(&mut self, _: CancellationToken) -> DriverResult<SessionContext> {
            self.pending = 0;
            Ok(SessionContext {
                catalog: None,
                schema: self.settings.last().cloned(),
            })
        }

        /// Reports the transaction `BEGIN` opened; asking discards a result
        /// still open, like a real connection.
        async fn transaction(
            &mut self,
            _: CancellationToken,
        ) -> DriverResult<Option<TransactionStatus>> {
            self.pending = 0;
            Ok(Some(if self.in_transaction {
                TransactionStatus::Open { changes: None }
            } else {
                TransactionStatus::Idle
            }))
        }

        async fn ping(&mut self) -> DriverResult<()> {
            if self.dead {
                self.open = false;
                return Err(DriverError::new(ErrorCode::Connection, "synthetic"));
            }
            Ok(())
        }

        /// Each session names itself, so a test tells which one was asked;
        /// asking with a result open or a statement run would be a bug.
        async fn encryption(&mut self, _: CancellationToken) -> DriverResult<Encryption> {
            if self.pending > 0 || !self.settings.is_empty() || self.in_transaction {
                return Err(DriverError::validation("asked after a statement"));
            }
            Ok(Encryption::Tls {
                version: Some(format!("session {}", self.id)),
            })
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
        /// Sessions open on the context's catalog, as PostgreSQL's do.
        fn session_connection(
            &self,
            connection: &ResolvedConnection,
            context: &SessionContext,
        ) -> DriverResult<ResolvedConnection> {
            let mut other = connection.clone();
            if let Some(catalog) = &context.catalog {
                other.config.options["database"] = serde_json::json!(catalog);
            }
            Ok(other)
        }
        async fn open_session(
            &self,
            connection: &ResolvedConnection,
            _: CancellationToken,
        ) -> DriverResult<Box<dyn Session>> {
            use std::sync::atomic::Ordering;
            if let Ok(mut databases) = self.databases.lock() {
                databases.push(
                    connection.config.options["database"]
                        .as_str()
                        .map(str::to_owned),
                );
            }
            Ok(Box::new(StateSession {
                id: self.opened.fetch_add(1, Ordering::SeqCst) + 1,
                settings: Vec::new(),
                open: true,
                dead: self.dead.load(Ordering::SeqCst),
                pending: 0,
                next: 0,
                in_transaction: false,
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
                parameters: Vec::new(),
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
        // The run hands back where the session says names resolve.
        assert_eq!(
            run.context
                .as_ref()
                .and_then(|context| context.schema.as_deref()),
            Some("schema=sales")
        );
        assert_eq!(run.transaction, Some(TransactionStatus::Idle));
        // The run hands back the transaction the session reports.
        let (run, _) = run_on(&driver, run.session, &["BEGIN"]).await;
        assert_eq!(
            run.transaction,
            Some(TransactionStatus::Open { changes: None })
        );
        let (run, _) = run_on(&driver, run.session, &["COMMIT"]).await;
        assert_eq!(run.transaction, Some(TransactionStatus::Idle));
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
    async fn a_report_session_opens_and_reopens_in_its_context_and_always_settles() {
        let driver = Arc::new(StateDriver::default());
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({"database": "home"}),
            },
            credentials: Credentials::default(),
        };
        let context = SessionContext {
            catalog: Some("archive".into()),
            schema: Some("public".into()),
        };
        let requests = |sql: &str| {
            vec![QueryRequest {
                parameters: Vec::new(),
                sql: sql.into(),
                options: ExecutionOptions {
                    max_rows: 2,
                    ..ExecutionOptions::default()
                },
            }]
        };
        // The first session will stop answering, as after an idle
        // disconnect.
        driver.dead.store(true, std::sync::atomic::Ordering::SeqCst);
        let run = query_in_context_with(
            driver.clone(),
            input.clone(),
            context.clone(),
            None,
            requests("ROWS 5"),
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let first = &run.batch.as_ref().unwrap().completed[0];
        assert!(first.has_more, "the panel stopped at its row limit");
        // A report does not page: the session is asked where names resolve
        // even though rows were left, so a USE cannot leak to the next panel.
        assert!(run.context.is_some());
        assert_eq!(run.transaction, Some(TransactionStatus::Idle));
        // A session that stopped answering reopens in the same database,
        // not in the one the profile names.
        driver
            .dead
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let run = query_in_context_with(
            driver.clone(),
            input.clone(),
            context.clone(),
            run.session,
            requests("SELECT"),
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(run.reopened);
        assert!(run.batch.is_ok());
        let opened = driver.databases.lock().unwrap().clone();
        assert_eq!(
            opened,
            [Some("archive".to_owned()), Some("archive".to_owned())]
        );
    }

    #[tokio::test]
    async fn a_new_session_is_asked_its_encryption_once_before_its_statements() {
        let tls = |id: usize| {
            Some(Encryption::Tls {
                version: Some(format!("session {id}")),
            })
        };
        let driver = Arc::new(StateDriver::default());
        let (run, _) = run_on(&driver, None, &["SET schema=sales", "SELECT"]).await;
        assert_eq!(run.encryption, tls(1));
        // A kept session is not asked again: its answer has not changed.
        let (run, _) = run_on(&driver, run.session, &["SELECT"]).await;
        assert_eq!(run.encryption, None);
        // A session opened in place of a lost one is asked.
        let (run, _) = run_on(&driver, run.session, &["LOSE"]).await;
        assert_eq!(run.encryption, None);
        let (run, _) = run_on(&driver, run.session, &["SELECT"]).await;
        assert_eq!(run.encryption, tls(2));
        // So is one replacing a kept session that stopped answering.
        let driver = Arc::new(StateDriver::default());
        driver.dead.store(true, std::sync::atomic::Ordering::SeqCst);
        let (run, _) = run_on(&driver, None, &["SELECT"]).await;
        driver
            .dead
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let (run, _) = run_on(&driver, run.session, &["SELECT"]).await;
        assert!(run.reopened);
        assert_eq!(run.encryption, tls(2));
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
            parameters: Vec::new(),
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
    async fn a_lost_session_holding_a_transaction_is_not_replaced() {
        use std::sync::atomic::Ordering::SeqCst;
        let driver = Arc::new(StateDriver::default());
        driver.dead.store(true, SeqCst);
        let (run, _) = run_on(&driver, None, &["BEGIN"]).await;
        assert_eq!(
            run.transaction,
            Some(TransactionStatus::Open { changes: None })
        );
        driver.dead.store(false, SeqCst);
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({}),
            },
            credentials: Credentials::default(),
        };
        // The statements were meant for the transaction: in a new session
        // they would commit one by one, so nothing runs.
        let run = run_query(
            driver.clone(),
            None,
            input,
            run.session,
            Statements {
                requests: vec![QueryRequest {
                    parameters: Vec::new(),
                    sql: "SET schema=sales".into(),
                    options: ExecutionOptions::default(),
                }],
                memory_ceiling: crate::result::DEFAULT_MEMORY_CEILING,
                progress: None,
                settle: false,
                holds_transaction: true,
            },
            CancellationToken::new(),
        )
        .await;
        assert!(run.transaction_lost && run.ended && !run.reopened);
        assert!(run.session.is_none());
        assert_eq!(
            run.batch.as_ref().err().map(|error| error.code),
            Some(ErrorCode::Connection)
        );
        assert_eq!(driver.opened.load(SeqCst), 1, "no session was opened");
        // The next run, with nothing to keep, opens a new session.
        let (run, cells) = run_on(&driver, None, &["SELECT"]).await;
        assert_eq!(cells, ["2:"]);
        assert!(!run.transaction_lost);
    }

    #[tokio::test]
    async fn a_report_session_left_in_a_transaction_is_closed() {
        let driver = Arc::new(StateDriver::default());
        let input = ResolvedConnection {
            config: ConnectionConfig {
                driver: DriverKind::Postgres,
                options: serde_json::json!({}),
            },
            credentials: Credentials::default(),
        };
        let panel = |sql: &str| {
            vec![QueryRequest {
                parameters: Vec::new(),
                sql: sql.into(),
                options: ExecutionOptions::default(),
            }]
        };
        let run = query_in_context_with(
            driver.clone(),
            input.clone(),
            SessionContext::default(),
            None,
            panel("BEGIN"),
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(run.transaction_closed && run.session.is_none());
        // A panel that leaves none keeps its session for the next run.
        let run = query_in_context_with(
            driver.clone(),
            input,
            SessionContext::default(),
            None,
            panel("SELECT"),
            crate::result::DEFAULT_MEMORY_CEILING,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!run.transaction_closed && run.session.is_some());
        assert_eq!(driver.opened.load(std::sync::atomic::Ordering::SeqCst), 2);
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
                parameters: Vec::new(),
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
        // Asking for the context or the transaction would close the open
        // result.
        assert!(run.context.is_none() && run.transaction.is_none());
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
            let context = page.context;
            let transaction = page.transaction;
            let page = page.page.unwrap();
            // Only the page that ends the result asks for the context and
            // the transaction.
            assert_eq!(context.is_some(), !page.has_more);
            assert_eq!(transaction.is_some(), !page.has_more);
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
    /// Catalogs and their schemas, shared by the clones of a test driver.
    type CatalogServer = Arc<std::sync::Mutex<Vec<(&'static str, Vec<&'static str>)>>>;

    #[derive(Clone, Debug, Default)]
    struct CatalogDriver {
        server: CatalogServer,
        read: Arc<std::sync::Mutex<Vec<ObjectPath>>>,
        failing: Option<&'static str>,
        cancel_after: Option<(usize, CancellationToken)>,
        /// How long each read takes, like a remote server.
        latency: Option<Duration>,
        /// Reads under way now, and the most there ever were at once.
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CatalogDriver {
        fn new(server: Vec<(&'static str, Vec<&'static str>)>) -> Self {
            Self {
                server: Arc::new(std::sync::Mutex::new(server)),
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
        fn left_out_of_everything(&self, path: &ObjectPath) -> bool {
            DatabricksDriver.left_out_of_everything(path)
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
            if let Some(latency) = self.latency {
                use std::sync::atomic::Ordering::SeqCst;
                let now = self.in_flight.fetch_add(1, SeqCst) + 1;
                self.peak.fetch_max(now, SeqCst);
                tokio::time::sleep(latency).await;
                self.in_flight.fetch_sub(1, SeqCst);
            }
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
        // The reader gets a handle to the same server and record.
        let shared: Arc<dyn Driver> = Arc::new(driver.clone());
        let outcome = apply_selection_with(shared, target, cancel, Some(sender))
            .await
            .unwrap();
        let mut updates = Vec::new();
        while let Ok(update) = receiver.try_recv() {
            updates.push(update);
        }
        (outcome, updates)
    }

    async fn cached(dir: &Path, id: ConnectionId) -> Vec<ObjectPath> {
        let repository =
            MetadataRepository::open(dir, id, &cache_config(&catalog_input().config).unwrap())
                .await
                .unwrap();
        // Walks the cache through the paged reads the explorer uses.
        let mut paths = Vec::new();
        let mut parents = vec![ObjectPath::default()];
        while let Some(parent) = parents.pop() {
            let mut after = None;
            loop {
                let page = repository
                    .children(&parent, after.as_ref(), 500)
                    .await
                    .unwrap();
                for object in page.objects {
                    parents.push(object.path.clone());
                    paths.push(object.path);
                }
                match page.next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
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
        // Only the chosen schemas were read; other.y never was. A first read
        // takes their names, then the full read of those that exist.
        let read = driver.read.lock().unwrap().clone();
        let mut names = read[..4].to_vec();
        names.sort_by_key(|path| format!("{path:?}"));
        let mut chosen = vec![
            schema("main", "a"),
            schema("main", "b"),
            schema("other", "gone"),
            schema("other", "x"),
        ];
        chosen.sort_by_key(|path| format!("{path:?}"));
        assert_eq!(names, chosen);
        let mut full = read[4..].to_vec();
        full.sort_by_key(|path| format!("{path:?}"));
        assert_eq!(
            full,
            [
                schema("main", "a"),
                schema("main", "b"),
                schema("other", "x")
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
        release_caches(&dir).await;
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
        // Cancelled after the first read, with a schema the server lost:
        // nothing is removed, because the read did not finish. Reads already
        // done when the cancellation came are not published either.
        let cancel = CancellationToken::new();
        let cancelling = CatalogDriver {
            cancel_after: Some((1, cancel.clone())),
            ..CatalogDriver::new(vec![("main", vec!["a", "b"])])
        };
        let (outcome, _) = apply(&cancelling, &dir, id, &everything, &everything, &cancel).await;
        assert!(outcome.cancelled);
        assert!(outcome.refreshed <= 1);
        assert!(cached(&dir, id).await.contains(&schema("main", "c")));
        release_caches(&dir).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn namespaces_are_read_in_parallel_and_system_waits_to_be_chosen() {
        let dir = std::env::temp_dir().join(format!("jykre-parallel-{}", ConnectionId::new()));
        let id = ConnectionId::new();
        // 4 catalogs × 5 schemas, plus the system catalog, each read taking
        // 40 ms, long enough for the reads started together to overlap. How
        // many overlap is counted rather than timed, so a slow machine does
        // not decide the result.
        let names = ["s1", "s2", "s3", "s4", "s5"];
        let driver = CatalogDriver {
            latency: Some(Duration::from_millis(40)),
            ..CatalogDriver::new(vec![
                ("c1", names.to_vec()),
                ("c2", names.to_vec()),
                ("c3", names.to_vec()),
                ("c4", names.to_vec()),
                ("system", vec!["access"]),
            ])
        };
        let everything = MetadataScope::everything();
        let (outcome, updates) = apply(
            &driver,
            &dir,
            id,
            &everything,
            &everything,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.refreshed, 20);
        // As many reads at once as allowed, and never more.
        let peak = driver.peak.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(peak, PARALLEL_READS);
        assert_eq!(updates.last().map(|update| update.done), Some(20));
        assert!(
            !driver
                .read
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.name_of(ObjectKind::Catalog) == Some("system"))
        );
        // Chosen by name, it is read.
        let system = MetadataScope {
            all: false,
            include: vec![ObjectPath::default().child(ObjectKind::Catalog, "system")],
        };
        let (outcome, _) = apply(
            &driver,
            &dir,
            id,
            &system,
            &system,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.refreshed, 1);
        release_caches(&dir).await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn changing_tls_keeps_the_cache_and_another_user_empties_it() {
        let dir = std::env::temp_dir().join(format!("jykre-identity-{}", ConnectionId::new()));
        let id = ConnectionId::new();
        let config = |user: &str, tls: &str| ConnectionConfig {
            driver: DriverKind::Postgres,
            options: serde_json::json!({"host":"db","database":"app","user":user,"sslMode":tls}),
        };
        let first = config("app", "disable");
        // A cache an earlier version keyed by the whole configuration.
        let legacy = MetadataRepository::open(&dir, id, &first).await.unwrap();
        let table = MetadataObject::new(ObjectKind::Database, &ObjectPath::default(), "app");
        let mut run = legacy.begin(&ObjectPath::default()).await.unwrap();
        run.batch(vec![table.clone()]).await.unwrap();
        run.activate(&IntrospectionReport::default(), &CancellationToken::new())
            .await
            .unwrap();
        drop(run);
        legacy.close().await;
        // Adopted under the driver's identity, then kept across a TLS change.
        prepare_cache(&dir, id, &first).await.unwrap();
        let tuned = config("app", "require");
        prepare_cache(&dir, id, &tuned).await.unwrap();
        let read = cached_metadata(&dir, id, &tuned, false).await.unwrap();
        assert_eq!(read.pages[0].1.objects, [table]);
        // Another user sees another catalog: the cache starts empty.
        let other = config("reporting", "require");
        prepare_cache(&dir, id, &other).await.unwrap();
        assert!(
            cached_metadata(&dir, id, &other, false)
                .await
                .unwrap()
                .is_empty()
        );
        release_caches(&dir).await;
        let _ = std::fs::remove_dir_all(dir);
    }
}
