use crate::*;
use sqlx::{Connection, Row, SqliteConnection};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

const SCHEMA: &str = r#"
CREATE TABLE metadata_info (singleton INTEGER PRIMARY KEY CHECK(singleton=1), identity TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 0);
CREATE TABLE objects (
    path_key TEXT PRIMARY KEY, parent_key TEXT, path_json TEXT NOT NULL,
    kind TEXT NOT NULL, name TEXT NOT NULL, ordinal INTEGER, object_json TEXT NOT NULL,
    FOREIGN KEY(parent_key) REFERENCES objects(path_key) DEFERRABLE INITIALLY DEFERRED
);
CREATE INDEX objects_parent ON objects(parent_key,kind,ordinal,name,path_key);
CREATE TABLE refresh_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT, scope_key TEXT NOT NULL, scope_json TEXT NOT NULL,
    base_version INTEGER NOT NULL, state TEXT NOT NULL,
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE UNIQUE INDEX one_running_refresh ON refresh_runs((1)) WHERE state='running';
CREATE TABLE staging_objects (
    run_id INTEGER NOT NULL REFERENCES refresh_runs(id) ON DELETE CASCADE,
    path_key TEXT NOT NULL, parent_key TEXT, path_json TEXT NOT NULL,
    kind TEXT NOT NULL, name TEXT NOT NULL, ordinal INTEGER, object_json TEXT NOT NULL,
    PRIMARY KEY(run_id,path_key)
);
CREATE TABLE protected_scopes (run_id INTEGER NOT NULL REFERENCES refresh_runs(id) ON DELETE CASCADE, path_key TEXT NOT NULL);
CREATE TABLE scope_status (
    scope_key TEXT PRIMARY KEY, scope_json TEXT NOT NULL, state TEXT NOT NULL,
    last_run INTEGER NOT NULL, last_success_run INTEGER,
    last_attempt_at TEXT NOT NULL, last_success_at TEXT, version INTEGER NOT NULL,
    message TEXT
);
"#;

/// A key is kept when it lies inside a kept subtree (`?1`), is a kept node
/// (`?2`) or is an ancestor of either. Both are JSON arrays of keys; '/'
/// separates segments and '0' is the character after it.
const RETAIN_OBJECTS: &str = "DELETE FROM objects WHERE NOT EXISTS(SELECT 1 FROM json_each(?1) s WHERE objects.path_key=s.value OR (objects.path_key>=s.value||'/' AND objects.path_key<s.value||'0') OR (s.value>=objects.path_key||'/' AND s.value<objects.path_key||'0')) AND NOT EXISTS(SELECT 1 FROM json_each(?2) n WHERE objects.path_key=n.value OR (n.value>=objects.path_key||'/' AND n.value<objects.path_key||'0'))";
const RETAIN_STATUSES: &str = "DELETE FROM scope_status WHERE scope_key<>'' AND NOT EXISTS(SELECT 1 FROM json_each(?1) s WHERE scope_status.scope_key=s.value OR (scope_status.scope_key>=s.value||'/' AND scope_status.scope_key<s.value||'0') OR (s.value>=scope_status.scope_key||'/' AND s.value<scope_status.scope_key||'0')) AND NOT EXISTS(SELECT 1 FROM json_each(?2) n WHERE scope_status.scope_key=n.value OR (n.value>=scope_status.scope_key||'/' AND n.value<scope_status.scope_key||'0'))";

/// Hex-encoded structural segments make prefix ranges unambiguous and indexable.
/// Neither dots, slashes, LIKE wildcards nor case folding can change node identity.
fn key(path: &ObjectPath) -> DriverResult<String> {
    path.0
        .iter()
        .map(|part| {
            serde_json::to_vec(part)
                .map(|bytes| {
                    bytes
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                })
                .map_err(json_error)
        })
        .collect::<DriverResult<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

/// Adds the namespace nodes on `path` (each prefix of length `1..=depth`)
/// that the cache does not hold yet. A namespace is fully described by its
/// path, so a refresh of a schema can publish under a database or catalog
/// that no refresh of its own has read.
async fn ensure_nodes(
    tx: &mut sqlx::SqliteConnection,
    path: &ObjectPath,
    depth: usize,
) -> DriverResult<()> {
    for length in 1..=depth.min(path.0.len()) {
        let node = ObjectPath(path.0[..length].to_vec());
        let Some(last) = node.0.last() else {
            continue;
        };
        let parent = node.parent().unwrap_or_default();
        let object = MetadataObject::new(last.kind, &parent, last.name.clone());
        let parent_key = if parent.0.is_empty() {
            None
        } else {
            Some(key(&parent)?)
        };
        sqlx::query("INSERT INTO objects(path_key,parent_key,path_json,kind,name,ordinal,object_json) VALUES(?,?,?,?,?,NULL,?) ON CONFLICT(path_key) DO NOTHING")
            .bind(key(&node)?)
            .bind(parent_key)
            .bind(serde_json::to_string(&node).map_err(json_error)?)
            .bind(serde_json::to_string(&object.kind).map_err(json_error)?)
            .bind(&object.name)
            .bind(serde_json::to_string(&object).map_err(json_error)?)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
    }
    Ok(())
}

/// How long publishing, retaining or resetting the cache waits for another
/// writer, and a new refresh for one that is finishing (a cancelled one
/// releases its lock once its task ends).
const LOCK_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

/// Takes the lock at `path`, waiting up to `patience` for its holder to
/// release it.
async fn writer_lock_within(path: &Path, patience: std::time::Duration) -> DriverResult<File> {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        match writer_lock(path) {
            Ok(lock) => return Ok(lock),
            Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}

fn canonical(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical).collect())
        }
        value => value,
    }
}

/// Two OS locks guard a cache file. The run lock is held for a whole
/// refresh, so owning it proves that no live refresh owns a `running` run
/// (one left by a crash can be recovered). The writer lock is held only
/// while publishing, retaining or resetting the cache, so a refresh reading
/// the server does not block those.
#[derive(Clone)]
pub struct MetadataRepository {
    pool: SqlitePool,
    identity: String,
    lock_path: PathBuf,
    run_lock_path: PathBuf,
    pub recovered: bool,
}

/// How a scope's last refresh ended, stored as text in `scope_status.state`
/// (the text is unchanged, so caches written before keep loading).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefreshState {
    /// A refresh of the scope is running.
    Running,
    /// The last refresh read and published the whole scope.
    Success,
    /// The last refresh published the scope, but parts it could not read
    /// kept their previous read.
    Partial,
    /// A part inside a refreshed scope could not be read; its previous read
    /// stays.
    Warning,
    /// The last refresh failed; the previous read stays.
    Failed,
    /// The last refresh was cancelled; the previous read stays.
    Cancelled,
    /// The application stopped during the last refresh; the previous read
    /// stays.
    Interrupted,
    /// A state this version does not know, such as one written by a newer
    /// version.
    #[default]
    Unknown,
}

impl RefreshState {
    /// The text stored in `scope_status.state`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Warning => "warning",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::Unknown => "unknown",
        }
    }

    /// Reads the stored text; anything unrecognised is `Unknown`.
    pub fn parse(text: &str) -> Self {
        match text {
            "running" => Self::Running,
            "success" => Self::Success,
            "partial" => Self::Partial,
            "warning" => Self::Warning,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RefreshStatus {
    pub scope: ObjectPath,
    pub state: RefreshState,
    pub last_attempt_at: Option<String>,
    pub last_success_at: Option<String>,
    pub version: i64,
    pub message: Option<String>,
}

pub struct PendingRefresh {
    repository: MetadataRepository,
    id: i64,
    version: i64,
    scope: ObjectPath,
    _run: File,
}

fn writer_lock(path: &Path) -> DriverResult<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| error("Could not open the cache lock."))?;
    file.try_lock()
        .map_err(|_| DriverError::validation("Another refresh is using this cache."))?;
    Ok(file)
}

impl MetadataRepository {
    /// Reconcile the cache only after accepting the current saved profile.
    /// Background readers and refresh jobs must use `open` instead.
    pub async fn prepare(
        dir: &Path,
        id: ConnectionId,
        config: &ConnectionConfig,
    ) -> DriverResult<Self> {
        Self::open_with_identity(dir, id, config, true).await
    }

    /// Open a cache without replacing a different configuration's identity.
    pub async fn open(
        dir: &Path,
        id: ConnectionId,
        config: &ConnectionConfig,
    ) -> DriverResult<Self> {
        Self::open_with_identity(dir, id, config, false).await
    }

    async fn open_with_identity(
        dir: &Path,
        id: ConnectionId,
        config: &ConnectionConfig,
        reconcile: bool,
    ) -> DriverResult<Self> {
        let dir = dir.join("metadata");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|_| error("Could not create the metadata cache."))?;
        let path = dir.join(format!("{id}.db"));
        let mut recovered = false;
        let pool = match open_pool(&path, false).await {
            Ok(pool) => pool,
            Err(_) => {
                // Recover only confirmed corruption; permission/I/O errors must
                // not rename an otherwise valid cache.
                let check =
                    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
                        .await;
                let corrupt = match check {
                    Ok(mut connection) => {
                        let result = sqlx::query_scalar::<_, String>("PRAGMA quick_check")
                            .fetch_one(&mut connection)
                            .await;
                        let _ = sqlx::Connection::close(connection).await;
                        match result {
                            Ok(value) => value != "ok",
                            Err(error) => error
                                .as_database_error()
                                .and_then(|db| db.code())
                                .is_some_and(|code| code == "26" || code == "11"),
                        }
                    }
                    Err(error) => error
                        .as_database_error()
                        .and_then(|db| db.code())
                        .is_some_and(|code| code == "26" || code == "11"),
                };
                if !corrupt {
                    return Err(error("Could not open the metadata cache."));
                }
                let stamp = ConnectionId::new();
                for suffix in ["", "-wal", "-shm"] {
                    let from = dir.join(format!("{id}.db{suffix}"));
                    if tokio::fs::try_exists(&from)
                        .await
                        .map_err(|_| error("Could not verify the cache."))?
                    {
                        tokio::fs::rename(
                            &from,
                            dir.join(format!("{id}.db.corrupt-{stamp}{suffix}")),
                        )
                        .await
                        .map_err(|_| error("Could not preserve the corrupted cache."))?;
                    }
                }
                recovered = true;
                open_pool(&path, false).await?
            }
        };
        schema(&pool, SCHEMA).await?;
        let identity = serde_json::to_string(&canonical(
            serde_json::to_value(config).map_err(json_error)?,
        ))
        .map_err(json_error)?;
        let mut tx = pool.begin().await.map_err(sql_error)?;
        sqlx::query("INSERT OR IGNORE INTO metadata_info(singleton,identity) VALUES(1,?)")
            .bind(&identity)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        let current: String =
            sqlx::query_scalar("SELECT identity FROM metadata_info WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(sql_error)?;
        if current != identity && !reconcile {
            tx.rollback().await.map_err(sql_error)?;
            pool.close().await;
            return Err(DriverError::validation(
                "The connection configuration changed. Reopen the cache.",
            ));
        }
        let lock_path = dir.join(format!("{id}.lock"));
        let run_lock_path = dir.join(format!("{id}.run.lock"));
        // A refresh still reading for the previous configuration does not
        // hold this lock; its publication fails as stale instead.
        let _lock = if current != identity {
            Some(writer_lock_within(&lock_path, LOCK_PATIENCE).await?)
        } else {
            None
        };
        if current != identity {
            sqlx::query("DELETE FROM objects")
                .execute(&mut *tx)
                .await
                .map_err(sql_error)?;
            sqlx::query("DELETE FROM scope_status")
                .execute(&mut *tx)
                .await
                .map_err(sql_error)?;
            sqlx::query("DELETE FROM refresh_runs")
                .execute(&mut *tx)
                .await
                .map_err(sql_error)?;
            sqlx::query("UPDATE metadata_info SET identity=?,version=version+1 WHERE singleton=1")
                .bind(&identity)
                .execute(&mut *tx)
                .await
                .map_err(sql_error)?;
        }
        tx.commit().await.map_err(sql_error)?;
        Ok(Self {
            pool,
            identity,
            lock_path,
            run_lock_path,
            recovered,
        })
    }

    /// Called once at application startup, never when opening a second reader.
    pub async fn recover_interrupted(&self) -> DriverResult<()> {
        let Ok(_run) = writer_lock(&self.run_lock_path) else {
            return Ok(());
        };
        let mut tx = self.pool.begin().await.map_err(sql_error)?;
        sqlx::query("UPDATE scope_status SET state='interrupted',message='Refresh interrupted; previous cache preserved.' WHERE state='running'").execute(&mut *tx).await.map_err(sql_error)?;
        sqlx::query("DELETE FROM refresh_runs WHERE state='running'")
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)
    }
    pub async fn close(&self) {
        self.pool.close().await;
    }
    pub async fn version(&self) -> DriverResult<i64> {
        sqlx::query_scalar("SELECT version FROM metadata_info WHERE singleton=1")
            .fetch_one(&self.pool)
            .await
            .map_err(sql_error)
    }
    /// Every cached object, for assertions; the application reads pages.
    #[cfg(test)]
    async fn all(&self) -> DriverResult<Vec<MetadataObject>> {
        let values: Vec<String> =
            sqlx::query_scalar("SELECT object_json FROM objects ORDER BY path_key")
                .fetch_all(&self.pool)
                .await
                .map_err(sql_error)?;
        values
            .into_iter()
            .map(|value| serde_json::from_str(&value).map_err(json_error))
            .collect()
    }
    pub async fn search(&self, query: &str, limit: u32) -> DriverResult<Vec<MetadataObject>> {
        if !(1..=200).contains(&limit) {
            return Err(DriverError::validation("Invalid search limit."));
        }
        let values: Vec<String> = sqlx::query_scalar("SELECT object_json FROM objects WHERE instr(lower(name),lower(?))>0 ORDER BY name,path_key LIMIT ?")
            .bind(query).bind(limit).fetch_all(&self.pool).await.map_err(sql_error)?;
        let mut found = std::collections::HashMap::new();
        for value in values {
            let object: MetadataObject = serde_json::from_str(&value).map_err(json_error)?;
            let mut parent = object.path.parent();
            found.insert(object.path.clone(), object);
            while let Some(path) = parent.filter(|path| !path.0.is_empty()) {
                parent = path.parent();
                if found.contains_key(&path) {
                    continue;
                }
                if let Some(value) = sqlx::query_scalar::<_, String>(
                    "SELECT object_json FROM objects WHERE path_key=?",
                )
                .bind(key(&path)?)
                .fetch_optional(&self.pool)
                .await
                .map_err(sql_error)?
                {
                    found.insert(path, serde_json::from_str(&value).map_err(json_error)?);
                }
            }
        }
        let mut found: Vec<_> = found.into_values().collect();
        found.sort_by(|a, b| {
            a.path
                .0
                .iter()
                .map(|part| &part.name)
                .cmp(b.path.0.iter().map(|part| &part.name))
        });
        Ok(found)
    }
    pub async fn children(
        &self,
        parent: &ObjectPath,
        offset: u32,
        limit: u32,
    ) -> DriverResult<Vec<MetadataObject>> {
        if !(1..=500).contains(&limit) {
            return Err(DriverError::validation("Invalid metadata page."));
        }
        let parent = if parent.0.is_empty() {
            None
        } else {
            Some(key(parent)?)
        };
        let values: Vec<String> = sqlx::query_scalar("SELECT object_json FROM objects WHERE parent_key IS ? ORDER BY kind,ordinal,name,path_key LIMIT ? OFFSET ?")
            .bind(parent).bind(limit).bind(offset).fetch_all(&self.pool).await.map_err(sql_error)?;
        values
            .into_iter()
            .map(|value| serde_json::from_str(&value).map_err(json_error))
            .collect()
    }
    pub async fn statuses(&self) -> DriverResult<Vec<RefreshStatus>> {
        let rows = sqlx::query("SELECT scope_json,state,last_attempt_at,last_success_at,version,message FROM scope_status ORDER BY last_run,length(scope_key)").fetch_all(&self.pool).await.map_err(sql_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(RefreshStatus {
                    scope: serde_json::from_str(
                        &row.try_get::<String, _>("scope_json").map_err(sql_error)?,
                    )
                    .map_err(json_error)?,
                    state: RefreshState::parse(
                        &row.try_get::<String, _>("state").map_err(sql_error)?,
                    ),
                    last_attempt_at: row.try_get("last_attempt_at").map_err(sql_error)?,
                    last_success_at: row.try_get("last_success_at").map_err(sql_error)?,
                    version: row.try_get("version").map_err(sql_error)?,
                    message: row.try_get("message").map_err(sql_error)?,
                })
            })
            .collect()
    }
    /// The paths of every cached object of `kind`, in key order.
    pub async fn paths(&self, kind: ObjectKind) -> DriverResult<Vec<ObjectPath>> {
        let values: Vec<String> =
            sqlx::query_scalar("SELECT path_json FROM objects WHERE kind=? ORDER BY path_key")
                .bind(serde_json::to_string(&kind).map_err(json_error)?)
                .fetch_all(&self.pool)
                .await
                .map_err(sql_error)?;
        values
            .into_iter()
            .map(|value| serde_json::from_str(&value).map_err(json_error))
            .collect()
    }

    /// Keeps only what the user chose to introspect: the objects inside
    /// `subtrees`, the namespaces in `nodes` and the ancestors of both, which
    /// are added when missing. Everything else goes, with its refresh status,
    /// in one transaction, and the cache version changes so readers page
    /// again. An empty path in `subtrees` keeps everything. Returns the new
    /// version.
    pub async fn retain(&self, subtrees: &[ObjectPath], nodes: &[ObjectPath]) -> DriverResult<i64> {
        let _lock = writer_lock_within(&self.lock_path, LOCK_PATIENCE).await?;
        let mut tx = self.pool.begin().await.map_err(sql_error)?;
        let (version, identity): (i64, String) =
            sqlx::query_as("SELECT version,identity FROM metadata_info WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(sql_error)?;
        if identity != self.identity {
            return Err(error(
                "The connection configuration changed. Reopen the cache.",
            ));
        }
        if subtrees.iter().any(|path| path.0.is_empty()) {
            return Ok(version);
        }
        for node in nodes {
            ensure_nodes(&mut tx, node, node.0.len()).await?;
        }
        let keys = |paths: &[ObjectPath]| -> DriverResult<String> {
            serde_json::to_string(&paths.iter().map(key).collect::<DriverResult<Vec<_>>>()?)
                .map_err(json_error)
        };
        let (subtrees, nodes) = (keys(subtrees)?, keys(nodes)?);
        sqlx::query(RETAIN_OBJECTS)
            .bind(&subtrees)
            .bind(&nodes)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        sqlx::query(RETAIN_STATUSES)
            .bind(&subtrees)
            .bind(&nodes)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        sqlx::query("UPDATE metadata_info SET version=version+1 WHERE singleton=1")
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)?;
        Ok(version + 1)
    }

    /// Starts a refresh of `scope`, waiting a few seconds for one that is
    /// finishing. Fails while another refresh of this cache runs.
    pub async fn begin(&self, scope: &ObjectPath) -> DriverResult<PendingRefresh> {
        self.begin_within(scope, LOCK_PATIENCE).await
    }

    async fn begin_within(
        &self,
        scope: &ObjectPath,
        patience: std::time::Duration,
    ) -> DriverResult<PendingRefresh> {
        let run = writer_lock_within(&self.run_lock_path, patience).await?;
        let mut tx = self.pool.begin().await.map_err(sql_error)?;
        // Owning the run lock proves no live refresh can own an abandoned run.
        sqlx::query("UPDATE scope_status SET state='interrupted',message='Refresh interrupted; previous cache preserved.' WHERE state='running'").execute(&mut *tx).await.map_err(sql_error)?;
        sqlx::query("DELETE FROM refresh_runs WHERE state='running'")
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        let (version, identity): (i64, String) =
            sqlx::query_as("SELECT version,identity FROM metadata_info WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(sql_error)?;
        if identity != self.identity {
            return Err(error(
                "The connection configuration changed. Reopen the cache.",
            ));
        }
        let scope_key = key(scope)?;
        let scope_json = serde_json::to_string(scope).map_err(json_error)?;
        let run_id = sqlx::query("INSERT INTO refresh_runs(scope_key,scope_json,base_version,state) VALUES(?,?,?,'running')")
            .bind(&scope_key).bind(&scope_json).bind(version).execute(&mut *tx).await.map_err(|_| DriverError::validation("A metadata refresh is already in progress."))?.last_insert_rowid();
        sqlx::query("INSERT INTO scope_status(scope_key,scope_json,state,last_run,last_attempt_at,version) VALUES(?,?,'running',?,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?) ON CONFLICT(scope_key) DO UPDATE SET state='running',last_run=excluded.last_run,last_attempt_at=excluded.last_attempt_at,message=NULL")
            .bind(scope_key).bind(scope_json).bind(run_id).bind(version).execute(&mut *tx).await.map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)?;
        Ok(PendingRefresh {
            repository: self.clone(),
            id: run_id,
            version,
            scope: scope.clone(),
            _run: run,
        })
    }
}

impl PendingRefresh {
    pub async fn batch(&mut self, objects: Vec<MetadataObject>) -> DriverResult<()> {
        let mut tx = self.repository.pool.begin().await.map_err(sql_error)?;
        for object in objects {
            if !object.path.is_within(&self.scope)
                || object
                    .path
                    .0
                    .last()
                    .is_none_or(|part| part.kind != object.kind || part.name != object.name)
            {
                return Err(DriverError::validation("Object outside the refresh scope."));
            }
            let parent = object
                .path
                .parent()
                .filter(|parent| !parent.0.is_empty())
                .map(|parent| key(&parent))
                .transpose()?;
            sqlx::query("INSERT INTO staging_objects(run_id,path_key,parent_key,path_json,kind,name,ordinal,object_json) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(run_id,path_key) DO UPDATE SET parent_key=excluded.parent_key,ordinal=excluded.ordinal,object_json=excluded.object_json")
                .bind(self.id).bind(key(&object.path)?).bind(parent).bind(serde_json::to_string(&object.path).map_err(json_error)?)
                .bind(serde_json::to_string(&object.kind).map_err(json_error)?).bind(&object.name).bind(object.ordinal).bind(serde_json::to_string(&object).map_err(json_error)?)
                .execute(&mut *tx).await.map_err(sql_error)?;
        }
        tx.commit().await.map_err(sql_error)
    }

    pub async fn activate(
        &mut self,
        report: &IntrospectionReport,
        cancel: &CancellationToken,
    ) -> DriverResult<i64> {
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        let scope_key = key(&self.scope)?;
        let lower = format!("{scope_key}/");
        let upper = format!("{scope_key}0");
        let _lock = writer_lock_within(&self.repository.lock_path, LOCK_PATIENCE).await?;
        let mut tx = self.repository.pool.begin().await.map_err(sql_error)?;
        let current: (i64, String) =
            sqlx::query_as("SELECT version,identity FROM metadata_info WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(sql_error)?;
        if current != (self.version, self.repository.identity.clone()) {
            return Err(error("Stale read: the configuration or the cache changed."));
        }
        for warning in &report.warnings {
            if warning.path.is_within(&self.scope) || self.scope.is_within(&warning.path) {
                let protected = if self.scope.is_within(&warning.path) {
                    &self.scope
                } else {
                    &warning.path
                };
                sqlx::query("INSERT INTO protected_scopes(run_id,path_key) VALUES(?,?)")
                    .bind(self.id)
                    .bind(key(protected)?)
                    .execute(&mut *tx)
                    .await
                    .map_err(sql_error)?;
            }
        }
        sqlx::query("DELETE FROM objects WHERE (?='' OR path_key=? OR (path_key>=? AND path_key<?)) AND NOT EXISTS(SELECT 1 FROM protected_scopes p WHERE p.run_id=? AND (p.path_key='' OR objects.path_key=p.path_key OR (objects.path_key>=p.path_key||'/' AND objects.path_key<p.path_key||'0')))")
            .bind(&scope_key).bind(&scope_key).bind(lower).bind(upper).bind(self.id).execute(&mut *tx).await.map_err(sql_error)?;
        let published = sqlx::query("INSERT INTO objects(path_key,parent_key,path_json,kind,name,ordinal,object_json) SELECT path_key,parent_key,path_json,kind,name,ordinal,object_json FROM staging_objects WHERE run_id=? ON CONFLICT(path_key) DO UPDATE SET parent_key=excluded.parent_key,path_json=excluded.path_json,kind=excluded.kind,name=excluded.name,ordinal=excluded.ordinal,object_json=excluded.object_json")
            .bind(self.id).execute(&mut *tx).await.map_err(sql_error)?.rows_affected();
        // The scope's own ancestors are outside it, so the driver does not
        // emit them; a scope that exists on the server still needs them.
        if published > 0 {
            let depth = self.scope.0.len().saturating_sub(1);
            ensure_nodes(&mut tx, &self.scope, depth).await?;
        }
        let state = if report.warnings.is_empty() {
            RefreshState::Success
        } else {
            RefreshState::Partial
        }
        .as_str();
        sqlx::query("UPDATE scope_status SET state=?,last_success_at=CASE WHEN ?='success' THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') ELSE last_success_at END,last_success_run=CASE WHEN ?='success' THEN last_run ELSE last_success_run END,version=?,message=? WHERE scope_key=? AND last_run=?")
            .bind(state).bind(state).bind(state).bind(self.version+1)
            .bind(if report.warnings.is_empty() { None } else { Some("Parts without permission were preserved.") }).bind(&scope_key).bind(self.id)
            .execute(&mut *tx).await.map_err(sql_error)?;
        for warning in &report.warnings {
            if warning.path.is_within(&self.scope) && warning.path != self.scope {
                sqlx::query("INSERT INTO scope_status(scope_key,scope_json,state,last_run,last_attempt_at,version,message) VALUES(?,?,'warning',?,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?,?) ON CONFLICT(scope_key) DO UPDATE SET state='warning',last_run=excluded.last_run,last_attempt_at=excluded.last_attempt_at,message=excluded.message")
                    .bind(key(&warning.path)?).bind(serde_json::to_string(&warning.path).map_err(json_error)?).bind(self.id).bind(self.version+1).bind(&warning.message).execute(&mut *tx).await.map_err(sql_error)?;
            }
        }
        sqlx::query("UPDATE metadata_info SET version=version+1 WHERE singleton=1")
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        sqlx::query("DELETE FROM refresh_runs WHERE id=?")
            .bind(self.id)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        if cancel.is_cancelled() {
            return Err(DriverError::cancelled());
        }
        tx.commit().await.map_err(sql_error)?;
        Ok(self.version + 1)
    }

    pub async fn fail(&mut self, failure: &DriverError) -> DriverResult<()> {
        let mut tx = self.repository.pool.begin().await.map_err(sql_error)?;
        sqlx::query(
            "UPDATE scope_status SET state=?,message=? WHERE last_run=? AND state='running'",
        )
        .bind(
            if failure.code == ErrorCode::Cancelled {
                RefreshState::Cancelled
            } else {
                RefreshState::Failed
            }
            .as_str(),
        )
        .bind(&failure.message)
        .bind(self.id)
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
        sqlx::query("DELETE FROM refresh_runs WHERE id=?")
            .bind(self.id)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)
    }
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
