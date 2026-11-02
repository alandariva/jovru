//! Application operations, independent of the UI and its executor.
use crate::drivers::*;
use crate::tr;
use jykre_storage::{MetadataRepository, PendingRefresh, RefreshStatus};
use std::path::{Path, PathBuf};
use std::{future::Future, sync::OnceLock, time::Instant};

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
    Ok(snapshot)
}

pub async fn search_metadata(
    dir: &Path,
    id: ConnectionId,
    config: &ConnectionConfig,
    query: &str,
) -> DriverResult<Vec<MetadataObject>> {
    let repository = MetadataRepository::open(dir, id, config).await?;
    let result = repository.search(query, 200).await;
    repository.close().await;
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

/// Drive network futures on Tokio while allowing the GUI to await their result.
pub async fn run<T: Send + 'static>(
    work: impl Future<Output = DriverResult<T>> + Send + 'static,
) -> DriverResult<T> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, std::io::Error>> = OnceLock::new();
    let runtime = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
    });
    let runtime = runtime
        .as_ref()
        .map_err(|_| DriverError::new(ErrorCode::Internal, tr!("Cannot start database runtime")))?;
    runtime
        .spawn(work)
        .await
        .map_err(|_| DriverError::new(ErrorCode::Internal, tr!("Database task failed")))?
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

#[derive(Default)]
pub struct QueryResult {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Row>,
    pub outcome: DriverOutcome,
    pub elapsed_ms: u128,
    pub limited: bool,
}

pub struct QueryBatch {
    pub completed: Vec<QueryResult>,
    /// The first failed instruction follows the completed results. Earlier
    /// statements may already have committed; never imply they were rolled back.
    pub error: Option<DriverError>,
}
impl RowSink for QueryResult {
    fn columns(&mut self, columns: Vec<ColumnInfo>) -> SinkControl {
        self.columns = columns;
        SinkControl::Continue
    }
    fn row(&mut self, row: Row) -> SinkControl {
        self.rows.push(row);
        SinkControl::Continue
    }
}

/// All statements in a run share one dedicated session. Always close it, also
/// after failure; cancelled sessions are never reused by subsequent runs.
pub async fn query(
    input: ResolvedConnection,
    requests: Vec<QueryRequest>,
    cancel: CancellationToken,
) -> DriverResult<QueryBatch> {
    for request in &requests {
        request.validate()?;
    }
    let driver = builtin_registry()?.get(input.config.driver)?;
    let mut session = driver.open_session(&input, cancel.clone()).await?;
    let mut results = Vec::new();
    let mut failure = None;
    for request in requests {
        let start = Instant::now();
        let mut result = QueryResult::default();
        match session.execute(&request, &mut result, cancel.clone()).await {
            Ok(outcome) => {
                result.outcome = outcome;
                result.elapsed_ms = start.elapsed().as_millis();
                result.limited = result.rows.len() >= request.options.max_rows as usize;
                results.push(result);
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    let closed = session.close().await;
    if failure.is_none() {
        failure = closed.err();
    }
    Ok(QueryBatch {
        completed: results,
        error: failure,
    })
}

pub fn cell_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".into(),
        Cell::Boolean(value) => value.to_string(),
        Cell::Integer(value) => value.to_string(),
        Cell::UnsignedInteger(value) => value.to_string(),
        Cell::Float(value) => value.to_string(),
        Cell::Decimal(value) | Cell::Text(value) | Cell::Temporal(value) => value.clone(),
        Cell::Binary(bytes) => format!(
            "0x{}",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ),
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
}
