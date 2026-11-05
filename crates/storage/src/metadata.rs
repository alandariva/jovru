use crate::*;
use sqlx::{Connection, Row, SqliteConnection};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::OnceLock,
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

/// The metadata file's schema version: version 1 is [`SCHEMA`]; each later
/// version is one step of [`STEPS`], applied in order. A released step is
/// never edited ([ADR-0018](../../../docs/adr/0018-metadata-cache-migrations.md)).
const VERSION: i64 = 3;

/// Step 2: Unicode search names (filled by [`migrate`]) and the time of the
/// last publication, complete or partial.
const STEP_2: &str = "ALTER TABLE objects ADD COLUMN search_name TEXT; ALTER TABLE staging_objects ADD COLUMN search_name TEXT; ALTER TABLE scope_status ADD COLUMN last_published_at TEXT; UPDATE scope_status SET last_published_at=last_success_at; CREATE INDEX objects_search ON objects(search_name,kind,name,path_key); CREATE VIRTUAL TABLE objects_fts USING fts5(search_name, content='objects', content_rowid='rowid', tokenize='trigram');";

/// The end of step 2, once the search names are filled: the trigram index
/// of the names, kept in step with `objects` by triggers.
const STEP_2_INDEX: &str = "INSERT INTO objects_fts(objects_fts) VALUES('rebuild'); CREATE TRIGGER objects_fts_insert AFTER INSERT ON objects BEGIN INSERT INTO objects_fts(rowid,search_name) VALUES(new.rowid,new.search_name); END; CREATE TRIGGER objects_fts_delete AFTER DELETE ON objects BEGIN INSERT INTO objects_fts(objects_fts,rowid,search_name) VALUES('delete',old.rowid,old.search_name); END; CREATE TRIGGER objects_fts_update AFTER UPDATE OF search_name ON objects BEGIN INSERT INTO objects_fts(objects_fts,rowid,search_name) VALUES('delete',old.rowid,old.search_name); INSERT INTO objects_fts(rowid,search_name) VALUES(new.rowid,new.search_name); END;";

/// Step 3: compact keys and one copy of each path
/// ([ADR-0019](../../../docs/adr/0019-compact-metadata-cache-keys.md)).
/// Refreshes left behind by a closed application are dropped, as
/// [`MetadataRepository::recover_interrupted`] would (foreign keys are off
/// during a step, so nothing cascades), and the secondary indexes go while
/// [`compact_keys`] rewrites every key and stored object; [`STEP_3_END`]
/// drops the paths and builds the indexes again.
const STEP_3: &str = "UPDATE scope_status SET state='interrupted',message='Refresh interrupted; previous cache preserved.' WHERE state='running'; DELETE FROM protected_scopes; DELETE FROM staging_objects; DELETE FROM refresh_runs; DROP INDEX objects_parent; DROP INDEX objects_search;";

/// The end of step 3, once every key is rewritten from `path_json`.
const STEP_3_END: &str = "ALTER TABLE objects DROP COLUMN path_json; ALTER TABLE staging_objects DROP COLUMN path_json; CREATE INDEX objects_parent ON objects(parent_key,kind,ordinal,name,path_key); CREATE INDEX objects_search ON objects(search_name,kind,name,path_key);";

const STEPS: [&str; 2] = [STEP_2, STEP_3];

/// How many objects step 3 rewrites per read, so a large cache is not held
/// in memory at once.
const COMPACT_CHUNK: i64 = 10_000;

/// Rewrites the hex keys of objects and statuses as [`key`] builds them, and
/// stores each object without its path. Old keys start with `7b` (the hex of
/// `{`) and new ones with `{`, so no new key meets an old one. Returns
/// whether there was any object to rewrite.
async fn compact_keys(tx: &mut SqliteConnection) -> DriverResult<bool> {
    let mut after = 0_i64;
    loop {
        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT rowid,path_json,object_json FROM objects WHERE rowid>? ORDER BY rowid LIMIT ?",
        )
        .bind(after)
        .bind(COMPACT_CHUNK)
        .fetch_all(&mut *tx)
        .await
        .map_err(sql_error)?;
        let Some(&(last, _, _)) = rows.last() else {
            break;
        };
        // The chunk is written by one statement: one per row costs more than
        // the rewrite itself.
        let mut rewritten = Vec::with_capacity(rows.len());
        for (rowid, path, object) in rows {
            let path: ObjectPath = serde_json::from_str(&path).map_err(json_error)?;
            let object: MetadataObject = serde_json::from_str(&object).map_err(json_error)?;
            let parent = path
                .parent()
                .filter(|parent| !parent.0.is_empty())
                .map(|parent| key(&parent))
                .transpose()?;
            rewritten.push((rowid, key(&path)?, parent, stored(object)?));
        }
        sqlx::query("UPDATE objects SET path_key=r.value->>1,parent_key=r.value->>2,object_json=r.value->>3 FROM json_each(?) r WHERE objects.rowid=r.value->>0")
            .bind(serde_json::to_string(&rewritten).map_err(json_error)?)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        after = last;
    }
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT scope_key,scope_json FROM scope_status")
            .fetch_all(&mut *tx)
            .await
            .map_err(sql_error)?;
    for (old, scope) in statuses {
        let scope: ObjectPath = serde_json::from_str(&scope).map_err(json_error)?;
        sqlx::query("UPDATE scope_status SET scope_key=? WHERE scope_key=?")
            .bind(key(&scope)?)
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
    }
    sqlx::raw_sql(STEP_3_END)
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
    Ok(after > 0)
}

/// How the search compares names: lower case by Unicode rules, so `ação`
/// finds `AÇÃO`.
pub(crate) fn search_name(name: &str) -> String {
    name.to_lowercase()
}

/// Brings the file to [`VERSION`]: a new file gets version 1 and then every
/// step; an older one only the steps it lacks, each in one transaction that
/// ends by setting `user_version`. A file from a newer version is refused.
///
/// Steps run on a connection of their own with foreign keys off, as SQLite
/// advises for schema changes, and check the keys before committing. The
/// connection is closed afterwards, so the pool never hands it out.
async fn migrate(pool: &SqlitePool) -> DriverResult<()> {
    let mut version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await
        .map_err(sql_error)?;
    if version > VERSION {
        return Err(error(
            "This file was created by a newer version of the application.",
        ));
    }
    if version == 0 {
        schema(pool, SCHEMA).await?;
        version = 1;
    }
    if version == VERSION {
        return Ok(());
    }
    let mut connection = pool.acquire().await.map_err(sql_error)?.detach();
    sqlx::raw_sql("PRAGMA foreign_keys=OFF")
        .execute(&mut connection)
        .await
        .map_err(sql_error)?;
    let mut compacted = false;
    while version < VERSION {
        let step = usize::try_from(version - 1)
            .ok()
            .and_then(|index| STEPS.get(index))
            .ok_or_else(|| error("Unknown metadata cache version."))?;
        let mut tx = connection.begin().await.map_err(sql_error)?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(*step))
            .execute(&mut *tx)
            .await
            .map_err(sql_error)?;
        if version + 1 == 2 {
            // SQLite folds ASCII only, so the names are folded here.
            let names: Vec<(String, String)> = sqlx::query_as("SELECT path_key,name FROM objects")
                .fetch_all(&mut *tx)
                .await
                .map_err(sql_error)?;
            for (path_key, name) in names {
                sqlx::query("UPDATE objects SET search_name=? WHERE path_key=?")
                    .bind(search_name(&name))
                    .bind(path_key)
                    .execute(&mut *tx)
                    .await
                    .map_err(sql_error)?;
            }
            sqlx::raw_sql(STEP_2_INDEX)
                .execute(&mut *tx)
                .await
                .map_err(sql_error)?;
        }
        if version + 1 == 3 {
            compacted = compact_keys(&mut tx).await?;
        }
        let broken = sqlx::query("PRAGMA foreign_key_check")
            .fetch_optional(&mut *tx)
            .await
            .map_err(sql_error)?;
        if broken.is_some() {
            return Err(error("The metadata cache holds a broken reference."));
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "PRAGMA user_version={}",
            version + 1
        )))
        .execute(&mut *tx)
        .await
        .map_err(sql_error)?;
        tx.commit().await.map_err(sql_error)?;
        version += 1;
    }
    connection.close().await.map_err(sql_error)?;
    // The rewrite leaves much of the file free; giving it back takes one pass
    // over the file, once.
    if compacted {
        sqlx::raw_sql("VACUUM")
            .execute(pool)
            .await
            .map_err(sql_error)?;
    }
    Ok(())
}

/// A key is kept when it lies inside a kept subtree (`?1`), is a kept node
/// (`?2`) or is an ancestor of either. Both are JSON arrays of keys; '/'
/// separates segments and '0' is the character after it.
const RETAIN_OBJECTS: &str = "DELETE FROM objects WHERE NOT EXISTS(SELECT 1 FROM json_each(?1) s WHERE objects.path_key=s.value OR (objects.path_key>=s.value||'/' AND objects.path_key<s.value||'0') OR (s.value>=objects.path_key||'/' AND s.value<objects.path_key||'0')) AND NOT EXISTS(SELECT 1 FROM json_each(?2) n WHERE objects.path_key=n.value OR (n.value>=objects.path_key||'/' AND n.value<objects.path_key||'0'))";
const RETAIN_STATUSES: &str = "DELETE FROM scope_status WHERE scope_key<>'' AND NOT EXISTS(SELECT 1 FROM json_each(?1) s WHERE scope_status.scope_key=s.value OR (scope_status.scope_key>=s.value||'/' AND scope_status.scope_key<s.value||'0') OR (s.value>=scope_status.scope_key||'/' AND s.value<scope_status.scope_key||'0')) AND NOT EXISTS(SELECT 1 FROM json_each(?2) n WHERE scope_status.scope_key=n.value OR (n.value>=scope_status.scope_key||'/' AND n.value<scope_status.scope_key||'0'))";

/// A key is the path's segments, each as its JSON with `%` and `/` escaped
/// (`%25`, `%2F`), joined by `/`. No segment holds the separator, so the
/// descendants of a node are exactly the keys from `key/` up to `key0` ('0'
/// follows '/'), and neither dots, LIKE wildcards nor case folding can change
/// node identity ([ADR-0019](../../../docs/adr/0019-compact-metadata-cache-keys.md)).
fn key(path: &ObjectPath) -> DriverResult<String> {
    path.0
        .iter()
        .map(|part| {
            serde_json::to_string(part)
                .map(|json| json.replace('%', "%25").replace('/', "%2F"))
                .map_err(json_error)
        })
        .collect::<DriverResult<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

/// The path a [`key`] was built from. Every `%` of a key starts an escape,
/// so `%2F` is undone before `%25`.
fn path_of(key: &str) -> DriverResult<ObjectPath> {
    if key.is_empty() {
        return Ok(ObjectPath::default());
    }
    key.split('/')
        .map(|part| {
            serde_json::from_str(&part.replace("%2F", "/").replace("%25", "%")).map_err(json_error)
        })
        .collect::<DriverResult<_>>()
        .map(ObjectPath)
}

/// An object as the cache stores it: without its path, which its key holds.
fn stored(mut object: MetadataObject) -> DriverResult<String> {
    object.path = ObjectPath::default();
    serde_json::to_string(&object).map_err(json_error)
}

/// An object read back from its stored JSON and its key.
fn object_of(json: &str, key: &str) -> DriverResult<MetadataObject> {
    let mut object: MetadataObject = serde_json::from_str(json).map_err(json_error)?;
    object.path = path_of(key)?;
    Ok(object)
}

/// The order a node's children are listed in, by kind: tables before views
/// and views before materialized views, whatever their stored text. Kinds
/// not listed come last, ordered by their text.
const KIND_ORDER: [ObjectKind; 9] = [
    ObjectKind::Database,
    ObjectKind::Catalog,
    ObjectKind::Schema,
    ObjectKind::Table,
    ObjectKind::View,
    ObjectKind::MaterializedView,
    ObjectKind::Column,
    ObjectKind::ForeignKey,
    ObjectKind::Partition,
];

fn kind_text_of(kind: ObjectKind) -> DriverResult<String> {
    serde_json::to_string(&kind).map_err(json_error)
}

/// Where a stored kind sorts among a node's children: its position in
/// [`KIND_ORDER`], then its text for kinds not listed.
fn kind_rank(kind: &str) -> (usize, &str) {
    static TEXTS: OnceLock<Vec<String>> = OnceLock::new();
    let texts = TEXTS.get_or_init(|| {
        KIND_ORDER
            .iter()
            .filter_map(|kind| serde_json::to_string(kind).ok())
            .collect()
    });
    let rank = texts
        .iter()
        .position(|text| text == kind)
        .unwrap_or(KIND_ORDER.len());
    (rank, kind)
}

/// Where the next page of a node's children starts: just after the last
/// child of the previous page, in the order pages are read (kind as in
/// [`KIND_ORDER`], then ordinal, name and key). A page read from a cursor
/// costs the same at any depth, unlike an offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageCursor {
    kind: String,
    ordinal: Option<i64>,
    name: String,
    path_key: String,
}

/// One page of a node's children; `next` is where the following page
/// starts, `None` after the last child.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChildPage {
    pub objects: Vec<MetadataObject>,
    pub next: Option<PageCursor>,
}

/// A page to read with [`MetadataRepository::pages`]: up to `limit`
/// children of `parent`, after `after` (from the first when `None`).
#[derive(Clone, Debug)]
pub struct PageRequest {
    pub parent: ObjectPath,
    pub after: Option<PageCursor>,
    pub limit: u32,
}

/// The kinds stored under a node, found by skipping through the
/// `objects_parent` index one kind at a time rather than reading children.
const CHILD_KINDS: &str = "WITH RECURSIVE kinds(kind) AS (SELECT (SELECT min(kind) FROM objects WHERE parent_key IS ?1) UNION ALL SELECT (SELECT min(kind) FROM objects WHERE parent_key IS ?1 AND kind>kinds.kind) FROM kinds WHERE kinds.kind IS NOT NULL) SELECT kind FROM kinds WHERE kind IS NOT NULL";

/// Reads up to `limit` children of `parent` after `after`. It reads only
/// the kinds the node holds, each in its turn and, within a kind, the
/// children without an ordinal (relations) before those with one (columns),
/// so every query walks the `objects_parent` index from the cursor instead
/// of skipping rows.
async fn read_children(
    connection: &mut SqliteConnection,
    parent: &ObjectPath,
    after: Option<&PageCursor>,
    limit: u32,
) -> DriverResult<ChildPage> {
    let parent = if parent.0.is_empty() {
        None
    } else {
        Some(key(parent)?)
    };
    let mut kinds: Vec<String> = sqlx::query_scalar(CHILD_KINDS)
        .bind(parent.clone())
        .fetch_all(&mut *connection)
        .await
        .map_err(sql_error)?;
    kinds.sort_by(|a, b| kind_rank(a).cmp(&kind_rank(b)));
    if let Some(cursor) = after {
        let from = kind_rank(&cursor.kind);
        kinds.retain(|kind| kind_rank(kind) >= from);
    }
    // One more than asked tells whether another page follows.
    let wanted = limit as usize + 1;
    let mut rows: Vec<(String, PageCursor)> = Vec::new();
    for kind in kinds {
        let within = after.filter(|cursor| cursor.kind == kind);
        let mut steps = Vec::with_capacity(2);
        if within.is_none_or(|cursor| cursor.ordinal.is_none()) {
            let (name, path_key) = within.map_or((String::new(), String::new()), |cursor| {
                (cursor.name.clone(), cursor.path_key.clone())
            });
            steps.push(
                sqlx::query("SELECT object_json,kind,ordinal,name,path_key FROM objects WHERE parent_key IS ? AND kind=? AND ordinal IS NULL AND (name,path_key)>(?,?) ORDER BY name,path_key LIMIT ?")
                    .bind(parent.clone()).bind(kind.clone()).bind(name).bind(path_key),
            );
        }
        let (ordinal, name, path_key) = match within {
            Some(PageCursor {
                ordinal: Some(ordinal),
                name,
                path_key,
                ..
            }) => (*ordinal, name.clone(), path_key.clone()),
            _ => (i64::MIN, String::new(), String::new()),
        };
        steps.push(
            sqlx::query("SELECT object_json,kind,ordinal,name,path_key FROM objects WHERE parent_key IS ? AND kind=? AND ordinal IS NOT NULL AND (ordinal,name,path_key)>(?,?,?) ORDER BY ordinal,name,path_key LIMIT ?")
                .bind(parent.clone()).bind(kind).bind(ordinal).bind(name).bind(path_key),
        );
        for step in steps {
            if rows.len() >= wanted {
                break;
            }
            let found = step
                .bind((wanted - rows.len()) as i64)
                .fetch_all(&mut *connection)
                .await
                .map_err(sql_error)?;
            for row in found {
                rows.push((
                    row.try_get("object_json").map_err(sql_error)?,
                    PageCursor {
                        kind: row.try_get("kind").map_err(sql_error)?,
                        ordinal: row.try_get("ordinal").map_err(sql_error)?,
                        name: row.try_get("name").map_err(sql_error)?,
                        path_key: row.try_get("path_key").map_err(sql_error)?,
                    },
                ));
            }
        }
        if rows.len() >= wanted {
            break;
        }
    }
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = if more {
        rows.last().map(|(_, cursor)| cursor.clone())
    } else {
        None
    };
    let objects = rows
        .into_iter()
        .map(|(value, cursor)| object_of(&value, &cursor.path_key))
        .collect::<DriverResult<_>>()?;
    Ok(ChildPage { objects, next })
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
        let kind = serde_json::to_string(&object.kind).map_err(json_error)?;
        let (name, search) = (object.name.clone(), search_name(&object.name));
        sqlx::query("INSERT INTO objects(path_key,parent_key,kind,name,ordinal,object_json,search_name) VALUES(?,?,?,?,NULL,?,?) ON CONFLICT(path_key) DO NOTHING")
            .bind(key(&node)?)
            .bind(parent_key)
            .bind(kind)
            .bind(name)
            .bind(stored(object)?)
            .bind(search)
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

/// The configuration a cache belongs to, as stored in `metadata_info`.
fn identity(config: &ConnectionConfig) -> DriverResult<String> {
    serde_json::to_string(&canonical(
        serde_json::to_value(config).map_err(json_error)?,
    ))
    .map_err(json_error)
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
    /// The scope, or a part inside a refreshed scope, could not be read for
    /// lack of permission; its previous read stays.
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
    /// When a refresh last published this scope, complete or partial.
    pub last_published_at: Option<String>,
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
        migrate(&pool).await?;
        let identity = identity(config)?;
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

    /// Removes the cache files of a deleted connection: the database, its
    /// `-wal` and `-shm` files and its lock files. Close every repository
    /// open on it first. Files already gone are fine; one that cannot be
    /// removed (still open elsewhere) is an error, and the others are
    /// removed regardless.
    pub async fn remove(dir: &Path, id: ConnectionId) -> DriverResult<()> {
        let dir = dir.join("metadata");
        let mut failed = false;
        for name in [
            format!("{id}.db"),
            format!("{id}.db-wal"),
            format!("{id}.db-shm"),
            format!("{id}.lock"),
            format!("{id}.run.lock"),
        ] {
            match tokio::fs::remove_file(dir.join(name)).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => failed = true,
            }
        }
        if failed {
            return Err(error("Could not remove the metadata cache."));
        }
        Ok(())
    }

    /// Moves a cache from the identity `from` to `to` without emptying it,
    /// when it still holds `from`: what a new way of computing the identity
    /// needs, since the catalog the cache describes is the same. Does
    /// nothing otherwise, and never creates a cache.
    pub async fn adopt(
        dir: &Path,
        id: ConnectionId,
        from: &ConnectionConfig,
        to: &ConnectionConfig,
    ) -> DriverResult<()> {
        let path = dir.join("metadata").join(format!("{id}.db"));
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(());
        }
        let (from, to) = (identity(from)?, identity(to)?);
        if from == to {
            return Ok(());
        }
        let pool = open_pool(&path, false).await?;
        let result = async {
            migrate(&pool).await?;
            sqlx::query("UPDATE metadata_info SET identity=? WHERE singleton=1 AND identity=?")
                .bind(to)
                .bind(from)
                .execute(&pool)
                .await
                .map_err(sql_error)
        }
        .await;
        pool.close().await;
        result.map(|_| ())
    }

    /// Whether this cache was opened for `config`'s identity, so a reader
    /// kept open can serve it. After the profile changes, the cache must be
    /// opened again.
    pub fn belongs_to(&self, config: &ConnectionConfig) -> bool {
        identity(config).is_ok_and(|identity| identity == self.identity)
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
        let values: Vec<(String, String)> =
            sqlx::query_as("SELECT object_json,path_key FROM objects ORDER BY path_key")
                .fetch_all(&self.pool)
                .await
                .map_err(sql_error)?;
        values
            .into_iter()
            .map(|(value, key)| object_of(&value, &key))
            .collect()
    }
    /// Up to `limit` (1 to 200) objects whose name contains `query`, compared
    /// by Unicode lower case, with their ancestors so a tree can show them.
    /// `kinds` keeps only those kinds before the limit applies, so tables are
    /// found behind any number of columns with the same name.
    pub async fn search(
        &self,
        query: &str,
        kinds: Option<&[ObjectKind]>,
        limit: u32,
    ) -> DriverResult<Vec<MetadataObject>> {
        if !(1..=200).contains(&limit) {
            return Err(DriverError::validation("Invalid search limit."));
        }
        let kinds = kinds
            .map(|kinds| {
                serde_json::to_string(
                    &kinds
                        .iter()
                        .map(|kind| kind_text_of(*kind))
                        .collect::<DriverResult<Vec<_>>>()?,
                )
                .map_err(json_error)
            })
            .transpose()?;
        let folded = search_name(query);
        // Three characters or more: through the trigram index. Fewer, which a
        // trigram cannot match: a scan of the name index, which holds only
        // the short folded names.
        let values: Vec<(String, String)> = if folded.chars().count() >= 3 {
            let phrase = format!("\"{}\"", folded.replace('"', "\"\""));
            sqlx::query_as("SELECT object_json,path_key FROM objects WHERE rowid IN (SELECT rowid FROM objects_fts WHERE objects_fts MATCH ?1) AND (?2 IS NULL OR kind IN (SELECT value FROM json_each(?2))) ORDER BY name,path_key LIMIT ?3")
                .bind(phrase).bind(kinds).bind(limit).fetch_all(&self.pool).await.map_err(sql_error)?
        } else {
            sqlx::query_as("SELECT object_json,path_key FROM objects WHERE path_key IN (SELECT path_key FROM objects INDEXED BY objects_search WHERE instr(search_name,?1)>0 AND (?2 IS NULL OR kind IN (SELECT value FROM json_each(?2))) ORDER BY name,path_key LIMIT ?3) ORDER BY name,path_key")
                .bind(folded).bind(kinds).bind(limit).fetch_all(&self.pool).await.map_err(sql_error)?
        };
        let mut found = std::collections::HashMap::new();
        for (value, key) in values {
            let object = object_of(&value, &key)?;
            found.insert(object.path.clone(), object);
        }
        // Every ancestor not found, read in one query.
        let mut ancestors = std::collections::BTreeSet::new();
        for path in found.keys() {
            let mut parent = path.parent();
            while let Some(path) = parent.filter(|path| !path.0.is_empty()) {
                parent = path.parent();
                if !found.contains_key(&path) {
                    ancestors.insert(key(&path)?);
                }
            }
        }
        if !ancestors.is_empty() {
            let keys = serde_json::to_string(&ancestors).map_err(json_error)?;
            let values: Vec<(String, String)> = sqlx::query_as(
                "SELECT object_json,path_key FROM objects WHERE path_key IN (SELECT value FROM json_each(?))",
            )
            .bind(keys)
            .fetch_all(&self.pool)
            .await
            .map_err(sql_error)?;
            for (value, key) in values {
                let object = object_of(&value, &key)?;
                found.insert(object.path.clone(), object);
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
    /// One page of up to `limit` (1 to 500) children of `parent`, starting
    /// after `after` (the first child when `None`). Reads only that page.
    pub async fn children(
        &self,
        parent: &ObjectPath,
        after: Option<&PageCursor>,
        limit: u32,
    ) -> DriverResult<ChildPage> {
        if !(1..=500).contains(&limit) {
            return Err(DriverError::validation("Invalid metadata page."));
        }
        let mut connection = self.pool.acquire().await.map_err(sql_error)?;
        read_children(&mut connection, parent, after, limit).await
    }

    /// Reads the pages in `requests` (each up to 100,000 children) and the
    /// cache version from one snapshot of the file, so a refresh that
    /// publishes meanwhile cannot mix two reads.
    pub async fn pages(&self, requests: &[PageRequest]) -> DriverResult<(i64, Vec<ChildPage>)> {
        if requests
            .iter()
            .any(|request| !(1..=100_000).contains(&request.limit))
        {
            return Err(DriverError::validation("Invalid metadata page."));
        }
        let mut tx = self.pool.begin().await.map_err(sql_error)?;
        let version: i64 =
            sqlx::query_scalar("SELECT version FROM metadata_info WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(sql_error)?;
        let mut pages = Vec::with_capacity(requests.len());
        for request in requests {
            pages.push(
                read_children(
                    &mut tx,
                    &request.parent,
                    request.after.as_ref(),
                    request.limit,
                )
                .await?,
            );
        }
        tx.rollback().await.map_err(sql_error)?;
        Ok((version, pages))
    }
    pub async fn statuses(&self) -> DriverResult<Vec<RefreshStatus>> {
        let rows = sqlx::query("SELECT scope_json,state,last_attempt_at,last_success_at,last_published_at,version,message FROM scope_status ORDER BY last_run,length(scope_key)").fetch_all(&self.pool).await.map_err(sql_error)?;
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
                    last_published_at: row.try_get("last_published_at").map_err(sql_error)?,
                    version: row.try_get("version").map_err(sql_error)?,
                    message: row.try_get("message").map_err(sql_error)?,
                })
            })
            .collect()
    }
    /// Every cached relation (table, view or materialized view) with its
    /// kind and the cache version they were read from, without decoding the
    /// objects: what the editor's name index is built from.
    pub async fn relations(&self) -> DriverResult<(i64, Vec<(ObjectPath, ObjectKind)>)> {
        let kinds = serde_json::to_string(
            &[
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView,
            ]
            .iter()
            .map(|kind| kind_text_of(*kind))
            .collect::<DriverResult<Vec<_>>>()?,
        )
        .map_err(json_error)?;
        let mut tx = self.pool.begin().await.map_err(sql_error)?;
        let version: i64 =
            sqlx::query_scalar("SELECT version FROM metadata_info WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(sql_error)?;
        let rows = sqlx::query("SELECT path_key,kind FROM objects WHERE kind IN (SELECT value FROM json_each(?)) ORDER BY path_key")
            .bind(kinds)
            .fetch_all(&mut *tx)
            .await
            .map_err(sql_error)?;
        tx.rollback().await.map_err(sql_error)?;
        let relations = rows
            .into_iter()
            .map(|row| {
                let path = path_of(&row.try_get::<String, _>("path_key").map_err(sql_error)?)?;
                let kind: ObjectKind =
                    serde_json::from_str(&row.try_get::<String, _>("kind").map_err(sql_error)?)
                        .map_err(json_error)?;
                Ok((path, kind))
            })
            .collect::<DriverResult<_>>()?;
        Ok((version, relations))
    }

    /// The paths of every cached object of `kind`, in key order.
    pub async fn paths(&self, kind: ObjectKind) -> DriverResult<Vec<ObjectPath>> {
        let values: Vec<String> =
            sqlx::query_scalar("SELECT path_key FROM objects WHERE kind=? ORDER BY path_key")
                .bind(serde_json::to_string(&kind).map_err(json_error)?)
                .fetch_all(&self.pool)
                .await
                .map_err(sql_error)?;
        values.iter().map(|value| path_of(value)).collect()
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
            let path_key = key(&object.path)?;
            let kind = serde_json::to_string(&object.kind).map_err(json_error)?;
            let (name, ordinal, search) = (
                object.name.clone(),
                object.ordinal,
                search_name(&object.name),
            );
            sqlx::query("INSERT INTO staging_objects(run_id,path_key,parent_key,kind,name,ordinal,object_json,search_name) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(run_id,path_key) DO UPDATE SET parent_key=excluded.parent_key,ordinal=excluded.ordinal,object_json=excluded.object_json,search_name=excluded.search_name")
                .bind(self.id).bind(path_key).bind(parent)
                .bind(kind).bind(name).bind(ordinal).bind(stored(object)?).bind(search)
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
        let published = sqlx::query("INSERT INTO objects(path_key,parent_key,kind,name,ordinal,object_json,search_name) SELECT path_key,parent_key,kind,name,ordinal,object_json,search_name FROM staging_objects WHERE run_id=? ON CONFLICT(path_key) DO UPDATE SET parent_key=excluded.parent_key,kind=excluded.kind,name=excluded.name,ordinal=excluded.ordinal,object_json=excluded.object_json,search_name=excluded.search_name")
            .bind(self.id).execute(&mut *tx).await.map_err(sql_error)?.rows_affected();
        // The scope's own ancestors are outside it, so the driver does not
        // emit them; a scope that exists on the server still needs them.
        if published > 0 {
            let depth = self.scope.0.len().saturating_sub(1);
            ensure_nodes(&mut tx, &self.scope, depth).await?;
        }
        // A warning on the scope itself (or above it) means nothing of it could
        // be read: no access, rather than a partial read.
        let state = if report
            .warnings
            .iter()
            .any(|warning| self.scope.is_within(&warning.path))
        {
            RefreshState::Warning
        } else if report.warnings.is_empty() {
            RefreshState::Success
        } else {
            RefreshState::Partial
        }
        .as_str();
        sqlx::query("UPDATE scope_status SET state=?,last_published_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),last_success_at=CASE WHEN ?='success' THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') ELSE last_success_at END,last_success_run=CASE WHEN ?='success' THEN last_run ELSE last_success_run END,version=?,message=? WHERE scope_key=? AND last_run=?")
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
